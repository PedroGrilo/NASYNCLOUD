//! On-disk index of everything we know about the remote tree: which files are
//! cached, pinned or waiting to be uploaded. A single redb file, updated with
//! atomic transactions so a crash never leaves it half-written.
//!
//! Most commits skip the fsync (one per new file would make large copies crawl),
//! and redb drops those if the process dies before the next fsynced one. So
//! each of them is also appended to a journal next to the index, without
//! fsync either: the operating system keeps it when only our process dies
//! (killed, crashed), and the next start replays it. Every commit carries a
//! sequence number, stored in the index too, so a journal entry is only ever
//! applied on top of the exact state it followed.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::ranges::RangeSet;
use crate::remote::Version;

const NODES: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Persistent part of a node. JSON, with defaults, so fields can be added
/// later without invalidating existing caches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub parent: u64,
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
    /// Version on the server this node corresponds to; `None` = not uploaded yet.
    #[serde(default)]
    pub remote: Option<Version>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub cached: RangeSet,
    #[serde(default)]
    pub cache_gen: u32,
    /// Local content is newer than the server's.
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub atime: i64,
    /// (folders) the children are known.
    #[serde(default)]
    pub listed: bool,
    /// (symbolic links) the target. Links only exist on this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

pub struct Store {
    db: Database,
    /// The journal and the last commit's sequence number. Held for the whole
    /// commit, so journal entries are in sequence order.
    journal: Mutex<(File, u64)>,
}

/// One commit, as written to the journal.
#[derive(Serialize, Deserialize)]
struct Entry {
    seq: u64,
    upserts: Vec<Record>,
    deletes: Vec<u64>,
    next_id: u64,
}

/// FNV-1a: tells a whole journal line from a torn or stale one.
fn checksum(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path).with_context(|| format!("cannot open the index {}", path.display()))?;
        // Make sure the tables exist so read transactions never fail on a fresh file.
        let txn = db.begin_write()?;
        txn.open_table(NODES)?;
        let seq = txn.open_table(META)?.get("seq")?.map_or(0, |v| v.value());
        txn.commit()?;
        let journal_path = path.with_extension("journal");
        let replay = Self::read_journal(&journal_path, seq);
        let file = File::options().create(true).append(true).open(&journal_path).with_context(|| format!("cannot open {}", journal_path.display()))?;
        let store = Self { db, journal: Mutex::new((file, seq)) };
        if !replay.is_empty() {
            log::warn!("recovering {} index change(s) from {} (nasyncloud did not stop cleanly)", replay.len(), journal_path.display());
            for e in replay {
                store.commit(&e.upserts, &e.deletes, e.next_id, false)?;
            }
        }
        // One fsynced commit: everything replayed is safe, the journal starts empty.
        store.commit(&[], &[], store.load()?.1, true)?;
        Ok(store)
    }

    /// The journal entries that follow commit `seq`, in order, up to the first
    /// line that is torn, stale or out of sequence.
    fn read_journal(path: &Path, mut seq: u64) -> Vec<Entry> {
        let Ok(f) = File::open(path) else { return Vec::new() };
        let mut out = Vec::new();
        for line in BufReader::new(f).split(b'\n') {
            let Ok(line) = line else { break };
            let Some((sum, json)) = std::str::from_utf8(&line).ok().and_then(|l| l.split_once(' ')) else { break };
            if u64::from_str_radix(sum, 16).ok() != Some(checksum(json.as_bytes())) {
                break;
            }
            let Ok(e) = serde_json::from_str::<Entry>(json) else { break };
            if e.seq <= seq {
                continue; // already in the index
            }
            if e.seq != seq + 1 {
                break;
            }
            seq = e.seq;
            out.push(e);
        }
        out
    }

    /// Returns every record plus the next free node id.
    pub fn load(&self) -> Result<(Vec<Record>, u64)> {
        let txn = self.db.begin_read()?;
        let nodes = txn.open_table(NODES)?;
        let mut records = Vec::new();
        for item in nodes.iter()? {
            let (key, value) = item?;
            match serde_json::from_slice::<Record>(value.value()) {
                Ok(rec) => records.push(rec),
                Err(e) => log::warn!("dropping unreadable index record {}: {e}", key.value()),
            }
        }
        let next_id = txn.open_table(META)?.get("next_id")?.map_or(2, |v| v.value());
        Ok((records, next_id))
    }

    /// Writes a batch atomically. `durable` waits for the data to reach the disk.
    pub fn commit(&self, upserts: &[Record], deletes: &[u64], next_id: u64, durable: bool) -> Result<()> {
        let mut journal = self.journal.lock().unwrap();
        let seq = journal.1 + 1;
        let mut txn = self.db.begin_write()?;
        txn.set_durability(if durable { Durability::Immediate } else { Durability::None })?;
        {
            let mut nodes = txn.open_table(NODES)?;
            for id in deletes {
                nodes.remove(id)?;
            }
            for rec in upserts {
                nodes.insert(rec.id, serde_json::to_vec(rec)?.as_slice())?;
            }
            let mut meta = txn.open_table(META)?;
            meta.insert("next_id", next_id)?;
            meta.insert("seq", seq)?;
        }
        txn.commit()?;
        journal.1 = seq;
        if durable {
            // Everything before is on disk now.
            journal.0.set_len(0)?;
        } else {
            let entry = Entry { seq, upserts: upserts.to_vec(), deletes: deletes.to_vec(), next_id };
            let json = serde_json::to_string(&entry)?;
            journal.0.write_all(format!("{:016x} {json}\n", checksum(json.as_bytes())).as_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        let rec = |id: u64, name: &str| Record {
            id,
            parent: 1,
            name: name.into(),
            dir: false,
            size: 10,
            mtime: 5,
            remote: Some(Version { etag: Some("\"e\"".into()), mtime: 5, size: 10 }),
            pinned: true,
            cached: RangeSet::full(10),
            cache_gen: 3,
            dirty: false,
            atime: 0,
            listed: false,
            link: None,
        };
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.load().unwrap().1, 2);
            store.commit(&[rec(2, "a"), rec(3, "b")], &[], 4, true).unwrap();
            store.commit(&[], &[2], 4, false).unwrap();
        }
        let (records, next) = Store::open(&path).unwrap().load().unwrap();
        assert_eq!(next, 4);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "b");
        assert!(records[0].cached.is_complete(10));
    }

    fn rec(id: u64, name: &str) -> Record {
        Record { id, parent: 1, name: name.into(), dir: false, size: 0, mtime: 0, remote: None, pinned: false, cached: RangeSet::default(), cache_gen: 0, dirty: true, atime: 0, listed: false, link: None }
    }

    fn line(seq: u64, upserts: Vec<Record>, next_id: u64) -> String {
        let json = serde_json::to_string(&Entry { seq, upserts, deletes: vec![], next_id }).unwrap();
        format!("{:016x} {json}\n", checksum(json.as_bytes()))
    }

    fn names(path: &Path) -> Vec<String> {
        let mut n: Vec<String> = Store::open(path).unwrap().load().unwrap().0.into_iter().map(|r| r.name).collect();
        n.sort();
        n
    }

    /// What a process killed between two fsynced commits leaves behind.
    #[test]
    fn journal_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        let seq = {
            let store = Store::open(&path).unwrap();
            store.commit(&[rec(2, "a")], &[], 3, true).unwrap();
            let seq = store.journal.lock().unwrap().1;
            seq
        };
        let journal = path.with_extension("journal");
        let mut text = line(seq, vec![rec(9, "stale")], 10); // already in the index
        text += &line(seq + 1, vec![rec(3, "b")], 4);
        text += &line(seq + 2, vec![rec(4, "c")], 5);
        text += &line(seq + 4, vec![rec(5, "gap")], 6); // seq + 3 is missing
        std::fs::write(&journal, &text).unwrap();
        assert_eq!(names(&path), ["a", "b", "c"]);
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), 0, "emptied once safe");
        assert_eq!(Store::open(&path).unwrap().load().unwrap().1, 5);

        // A torn last line (power cut while writing) stops the replay there.
        let seq = {
            let store = Store::open(&path).unwrap();
            let seq = store.journal.lock().unwrap().1;
            seq
        };
        let torn = line(seq + 2, vec![rec(7, "torn")], 8);
        let text = line(seq + 1, vec![rec(6, "d")], 7) + &torn[..torn.len() - 10];
        std::fs::write(&journal, text).unwrap();
        assert_eq!(names(&path), ["a", "b", "c", "d"]);
    }

    /// Commits without fsync go to the journal; an fsynced one empties it.
    #[test]
    fn journal_follows_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        let journal = path.with_extension("journal");
        let store = Store::open(&path).unwrap();
        store.commit(&[rec(2, "a")], &[], 3, false).unwrap();
        store.commit(&[], &[2], 3, false).unwrap();
        let text = std::fs::read_to_string(&journal).unwrap();
        assert_eq!(text.lines().count(), 2);
        let seq = store.journal.lock().unwrap().1;
        assert_eq!(Store::read_journal(&journal, seq - 2).len(), 2);
        store.commit(&[], &[], 3, true).unwrap();
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), 0);
    }
}
