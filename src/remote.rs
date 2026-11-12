//! Backend abstraction. Today there is one implementation (WebDAV); adding SMB
//! or SFTP later means implementing these few methods.
//!
//! Paths are always relative to the configured root, `/`-separated, without a
//! leading slash. The root itself is the empty string.

use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

/// What we know about one remote revision of a file or folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub etag: Option<String>,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
    pub size: u64,
}

impl Version {
    /// Whether two observations describe the same content.
    pub fn same(&self, other: &Version) -> bool {
        if self.size != other.size {
            return false;
        }
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => norm_etag(a) == norm_etag(b),
            _ => self.mtime == other.mtime,
        }
    }
}

/// Strips the weak marker and quotes so `W/"abc"` and `"abc"` compare equal.
pub fn norm_etag(etag: &str) -> &str {
    etag.trim().trim_start_matches("W/").trim_matches('"')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub version: Version,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RemoteError {
    #[error("not found")]
    NotFound,
    /// The server could not be reached at all (treated as "we are offline").
    #[error("network: {0}")]
    Network(String),
    #[error("authentication failed (check the username and password)")]
    Unauthorized,
    #[error("permission denied by the server")]
    Forbidden,
    /// The request was refused because of the state on the server (name
    /// already taken, missing parent folder, locked...).
    #[error("refused by the server: {0}")]
    Precondition(String),
    /// The file changed on the server while we were reading it.
    #[error("the file changed on the server")]
    Changed,
    #[error("server error {0}: {1}")]
    Http(u16, String),
    #[error("{0}")]
    Other(String),
}

impl RemoteError {
    pub fn is_offline(&self) -> bool {
        matches!(self, RemoteError::Network(_) | RemoteError::Http(502..=504, _))
    }
}

/// A (possibly partial) file download in progress.
pub struct Download {
    pub etag: Option<String>,
    /// Total size of the file as reported by the server, when known.
    pub total: Option<u64>,
    /// Bytes to discard from the start of `stream` (servers that ignore `Range`).
    pub skip: u64,
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, RemoteError>> + Send>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Quota {
    pub used: u64,
    pub available: u64,
}

#[async_trait]
pub trait Remote: Send + Sync + 'static {
    /// Children of a folder.
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, RemoteError>;
    /// Metadata for one path, `None` when it does not exist.
    async fn stat(&self, path: &str) -> Result<Option<Entry>, RemoteError>;
    /// Opens a stream over `[start, end)` of a file (`end = None` reads to EOF).
    async fn open_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Download, RemoteError>;
    /// Uploads exactly `size` bytes of the local file `source`, replacing `path`.
    async fn upload(&self, path: &str, source: &Path, size: u64, mtime: i64) -> Result<(), RemoteError>;
    /// Creates a folder. Succeeds if it already exists.
    async fn mkdir(&self, path: &str) -> Result<(), RemoteError>;
    /// Deletes a file or (recursively) a folder. Succeeds if it is already gone.
    async fn delete(&self, path: &str, is_dir: bool) -> Result<(), RemoteError>;
    async fn rename(&self, from: &str, to: &str, is_dir: bool, overwrite: bool) -> Result<(), RemoteError>;
    /// Storage quota, when the server reports one.
    async fn quota(&self) -> Result<Option<Quota>, RemoteError>;
}

/// Joins a folder path and a child name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Splits a path into (parent folder, name).
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Names we create on the server for in-flight uploads; never shown or synced.
pub const PART_SUFFIX: &str = ".nasyncloud-part";
/// Left over by versions before 1.0 (then called nasdrive); still cleaned up.
const LEGACY_PART_SUFFIX: &str = ".nasdrive-part";

pub fn is_part_file(name: &str) -> bool {
    name.ends_with(PART_SUFFIX) || name.ends_with(LEGACY_PART_SUFFIX)
}

/// Name for the local version of a file that changed on both sides, e.g.
/// `report (conflict 2026-11-12 153012).docx`. `taken` says whether a
/// candidate is already in use.
pub fn conflict_name(name: &str, unix_time: i64, mut taken: impl FnMut(&str) -> bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    // "a (conflict X) (conflict Y).zip" helps nobody: keep one marker.
    let mut stem = stem.to_string();
    while let Some(i) = stem.rfind(" (conflict ") {
        if !stem.ends_with(')') {
            break;
        }
        stem.truncate(i);
    }
    while stem.len() + ext.len() + 40 > 255 {
        stem.pop();
    }
    let stamp = timestamp(unix_time);
    let mut candidate = format!("{stem} (conflict {stamp}){ext}");
    let mut k = 2;
    while taken(&candidate) {
        candidate = format!("{stem} (conflict {stamp} {k}){ext}");
        k += 1;
    }
    candidate
}

/// `2026-11-12 153012` (UTC) for a Unix timestamp.
pub fn timestamp(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil-from-days, after Howard Hinnant.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}{:02}{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "1970-01-01 000000");
        assert_eq!(timestamp(1_794_391_200), "2026-11-11 100000");
        assert_eq!(timestamp(951_782_399), "2000-02-28 235959");
        assert_eq!(timestamp(951_868_800), "2000-03-01 000000");
    }

    #[test]
    fn conflict_names() {
        assert_eq!(conflict_name("a.tar.gz", 0, |_| false), "a.tar (conflict 1970-01-01 000000).gz");
        assert_eq!(conflict_name(".bashrc", 0, |_| false), ".bashrc (conflict 1970-01-01 000000)");
        assert_eq!(conflict_name("x", 0, |c| !c.ends_with(" 3)")), "x (conflict 1970-01-01 000000 3)");
        assert_eq!(conflict_name("a (conflict 2026-11-12 185942) (conflict 2026-11-12 192728 2).zip", 0, |_| false), "a (conflict 1970-01-01 000000).zip");
        assert!(conflict_name(&"é".repeat(127), 0, |_| false).len() <= 255);
    }

    #[test]
    fn versions_compare_by_etag_or_mtime() {
        let v = |etag: Option<&str>, mtime, size| Version { etag: etag.map(str::to_string), mtime, size };
        assert!(v(Some("W/\"a\""), 1, 5).same(&v(Some("\"a\""), 2, 5)));
        assert!(!v(Some("a"), 1, 5).same(&v(Some("b"), 1, 5)));
        assert!(v(None, 1, 5).same(&v(Some("b"), 1, 5)));
        assert!(!v(None, 1, 5).same(&v(None, 2, 5)));
        assert!(!v(Some("a"), 1, 5).same(&v(Some("a"), 1, 6)));
    }
}
