//! Files that changed both here and on the NAS.
//!
//! Nothing waits for the person: as soon as a conflict is found, the local
//! version gets a new name (`relatorio (conflict 2026-11-21 153012).docx`)
//! and is sent like any other file, while the NAS version comes back under
//! the original name. This list remembers those pairs (in `conflicts.json`,
//! next to the index), so the interface can ask later which one to keep:
//!
//! * `mine`: the local version goes back to the original name, replacing the
//!   NAS one (only if the NAS one has not changed again since);
//! * `nas`: the local version is deleted;
//! * `both`: nothing changes, the pair just leaves the list.
//!
//! The engines (Linux/macOS VFS, Windows Cloud Files) do the actual work.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::remote::Version;

/// Most conflicts remembered; the oldest go first.
const MAX: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: u64,
    /// The original name. The NAS version is (again) there.
    pub path: String,
    /// The new name of the local version, in the same folder.
    pub copy: String,
    /// When the conflict was found (Unix seconds).
    pub at: i64,
    /// The local version, when the conflict was found.
    pub mine_size: u64,
    pub mine_mtime: i64,
    /// The NAS version the choice applies to. `mine` only replaces this one.
    pub nas: Version,
}

/// Which version to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    Mine,
    Nas,
    Both,
}

/// What happened to a choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// The NAS version changed again: nothing was done, and the conflict now
    /// refers to the new NAS version (choosing again replaces that one).
    Changed,
    /// The local version has not reached the NAS yet (Windows): try again later.
    Sending,
    /// `nas` was chosen but the NAS has no version any more: nothing was done.
    NasGone,
    /// The local copy is gone (deleted or renamed meanwhile): nothing to choose.
    Gone,
}

pub struct Conflicts {
    file: PathBuf,
    list: Mutex<Vec<Conflict>>,
}

impl Conflicts {
    /// Reads `conflicts.json` from `state_dir` (an unreadable file counts as empty).
    pub fn open(state_dir: &Path) -> Self {
        let file = state_dir.join("conflicts.json");
        let list = std::fs::read(&file).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
        Self { file, list: Mutex::new(list) }
    }

    /// Remembers a new conflict and returns its id.
    pub fn add(&self, path: &str, copy: &str, mine_size: u64, mine_mtime: i64, nas: Version, at: i64) -> u64 {
        let mut list = self.list.lock().unwrap();
        // From the clock, so an id is never given twice (the app notifies
        // each id once), even after the list has forgotten it.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
        let id = now.max(list.iter().map(|c| c.id).max().unwrap_or(0) + 1);
        list.push(Conflict { id, path: path.to_string(), copy: copy.to_string(), at, mine_size, mine_mtime, nas });
        let excess = list.len().saturating_sub(MAX);
        list.drain(..excess);
        self.save(&list);
        id
    }

    pub fn list(&self) -> Vec<Conflict> {
        self.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: u64) -> Option<Conflict> {
        self.list.lock().unwrap().iter().find(|c| c.id == id).cloned()
    }

    pub fn remove(&self, id: u64) {
        self.retain(|c| c.id != id);
    }

    /// Keeps only the conflicts for which `keep` says so.
    pub fn retain(&self, keep: impl FnMut(&Conflict) -> bool) {
        let mut list = self.list.lock().unwrap();
        let before = list.len();
        list.retain(keep);
        if list.len() != before {
            self.save(&list);
        }
    }

    /// The NAS version changed again: from now on the choice applies to `nas`.
    pub fn set_nas(&self, id: u64, nas: Version) {
        let mut list = self.list.lock().unwrap();
        if let Some(c) = list.iter_mut().find(|c| c.id == id) {
            c.nas = nas;
            self.save(&list);
        }
    }

    /// Written to a temporary file first, so a crash never leaves half a list.
    fn save(&self, list: &[Conflict]) {
        let tmp = self.file.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(list).map_err(std::io::Error::other).and_then(|raw| std::fs::write(&tmp, raw)).and_then(|()| std::fs::rename(&tmp, &self.file));
        if let Err(e) = res {
            log::warn!("could not save the list of conflicts in {}: {e}", self.file.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let v = |size| Version { etag: Some("x".into()), mtime: 1, size };
        let c = Conflicts::open(dir.path());
        let a = c.add("a.txt", "a (conflict 1).txt", 3, 10, v(4), 100);
        let b = c.add("d/b.txt", "d/b (conflict 1).txt", 5, 11, v(6), 101);
        assert_ne!(a, b);
        c.set_nas(b, v(7));
        c.remove(a);
        let again = Conflicts::open(dir.path());
        assert_eq!(again.list().len(), 1);
        assert_eq!(again.get(b).unwrap().nas.size, 7);
        assert!(again.get(a).is_none());
        assert!(again.add("x", "y", 0, 0, v(0), 0) > b, "ids are never reused");
    }
}
