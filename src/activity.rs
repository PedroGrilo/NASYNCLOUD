//! Recent transfers, for the "what went up, what came down" list of the
//! interface (the tray panel, OneDrive-style).
//!
//! Transfers of the same file close together in time are merged into one
//! entry: a video read in several ranges shows up once, not once per seek.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

/// How many entries are kept (active ones are never dropped).
const KEEP: usize = 100;
/// A new transfer of a file finished less than this long ago reuses its entry.
const MERGE_SECS: i64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Clone, Debug, Serialize)]
pub struct Transfer {
    /// Stable while the entry exists, so the interface can follow it.
    pub id: u64,
    /// Relative to the NAS folder, `/`-separated.
    pub path: String,
    pub direction: Direction,
    /// Bytes moved in this entry (for a download, possibly less than `total`).
    pub bytes: u64,
    /// Size of the file.
    pub total: u64,
    /// Transfers of this file still running.
    pub active: u32,
    /// Unix seconds.
    pub started: i64,
    pub finished: Option<i64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Activity {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    seq: u64,
    /// Most recent first.
    items: VecDeque<Transfer>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Activity {
    /// A transfer of `path` starts. Returns the entry to report progress to.
    pub fn begin(&self, direction: Direction, path: &str, total: u64) -> u64 {
        let mut a = self.inner.lock().unwrap();
        let t = now();
        let reuse = a.items.iter().position(|e| {
            e.direction == direction && e.path == path && (e.active > 0 || e.finished.is_some_and(|f| t - f < MERGE_SECS))
        });
        if let Some(i) = reuse {
            let mut e = a.items.remove(i).unwrap();
            if e.active == 0 {
                e.finished = None;
                e.error = None;
                if direction == Direction::Upload {
                    e.bytes = 0;
                }
            }
            e.active += 1;
            e.total = total;
            let id = e.id;
            a.items.push_front(e);
            return id;
        }
        a.seq += 1;
        let id = a.seq;
        a.items.push_front(Transfer { id, path: path.to_string(), direction, bytes: 0, total, active: 1, started: t, finished: None, error: None });
        while a.items.len() > KEEP {
            match a.items.iter().rposition(|e| e.active == 0) {
                Some(i) => {
                    a.items.remove(i);
                }
                None => break,
            }
        }
        id
    }

    /// `bytes` more were moved.
    pub fn progress(&self, id: u64, bytes: u64) {
        if let Some(e) = self.inner.lock().unwrap().items.iter_mut().find(|e| e.id == id) {
            e.bytes = e.bytes.saturating_add(bytes);
        }
    }

    /// One transfer of the entry ended; `error` if it failed.
    pub fn end(&self, id: u64, error: Option<String>) {
        let mut a = self.inner.lock().unwrap();
        if let Some(e) = a.items.iter_mut().find(|e| e.id == id) {
            e.active = e.active.saturating_sub(1);
            if error.is_some() {
                e.error = error;
            }
            if e.active == 0 {
                e.finished = Some(now());
            }
        }
    }

    /// Drops an entry that turned out to be nothing (e.g. a transfer that
    /// stopped before moving a byte), unless something else is using it.
    pub fn discard_if_empty(&self, id: u64) {
        let mut a = self.inner.lock().unwrap();
        if let Some(i) = a.items.iter().position(|e| e.id == id) {
            let e = &mut a.items[i];
            e.active = e.active.saturating_sub(1);
            if e.active == 0 && e.bytes == 0 && e.error.is_none() {
                a.items.remove(i);
            } else if e.active == 0 {
                e.finished = Some(now());
            }
        }
    }

    /// Most recent first.
    pub fn list(&self) -> Vec<Transfer> {
        self.inner.lock().unwrap().items.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_transfers_of_the_same_file() {
        let a = Activity::default();
        let x = a.begin(Direction::Download, "v.mkv", 100);
        let y = a.begin(Direction::Download, "v.mkv", 100);
        assert_eq!(x, y);
        a.progress(x, 30);
        a.end(x, None);
        assert_eq!(a.list()[0].active, 1);
        a.progress(y, 20);
        a.end(y, None);
        let l = a.list();
        assert_eq!((l.len(), l[0].bytes, l[0].active), (1, 50, 0));
        assert!(l[0].finished.is_some());
        // Soon after, the same file again: same entry, more bytes.
        let z = a.begin(Direction::Download, "v.mkv", 100);
        assert_eq!(z, x);
        a.progress(z, 10);
        a.end(z, None);
        assert_eq!(a.list()[0].bytes, 60);
        // An upload of it is a different entry.
        assert_ne!(a.begin(Direction::Upload, "v.mkv", 100), x);
    }

    #[test]
    fn errors_empty_entries_and_limit() {
        let a = Activity::default();
        let x = a.begin(Direction::Upload, "a", 5);
        a.end(x, Some("offline".into()));
        assert_eq!(a.list()[0].error.as_deref(), Some("offline"));
        // A retry clears the error.
        let y = a.begin(Direction::Upload, "a", 5);
        assert_eq!(a.list()[0].error, None);
        a.progress(y, 5);
        a.end(y, None);
        let e = a.begin(Direction::Download, "b", 9);
        a.discard_if_empty(e);
        assert!(a.list().iter().all(|t| t.path != "b"));
        for i in 0..150 {
            let id = a.begin(Direction::Download, &format!("f{i}"), 1);
            a.end(id, None);
        }
        assert_eq!(a.list().len(), KEEP);
    }
}
