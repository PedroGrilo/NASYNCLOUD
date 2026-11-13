//! On-disk index of everything we know about the remote tree: which files are
//! cached, pinned or waiting to be uploaded. A single redb file, updated with
//! atomic transactions so a crash never leaves it half-written.

use std::path::Path;

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
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path).with_context(|| format!("cannot open the index {}", path.display()))?;
        // Make sure the tables exist so read transactions never fail on a fresh file.
        let txn = db.begin_write()?;
        txn.open_table(NODES)?;
        txn.open_table(META)?;
        txn.commit()?;
        Ok(Self { db })
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
            txn.open_table(META)?.insert("next_id", next_id)?;
        }
        txn.commit()?;
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
}
