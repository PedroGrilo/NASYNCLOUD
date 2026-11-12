//! Windows engine, built on the Cloud Files API (the mechanism behind
//! OneDrive's Files On-Demand).
//!
//! Here NTFS itself holds the tree: every file on the NAS is a *placeholder*
//! in the sync folder. Explorer shows the cloud / green-tick icons and offers
//! "Always keep on this device" and "Free up space" on its own. Our job is to
//!
//! * create placeholders when a folder is opened for the first time,
//! * supply the bytes when a placeholder is opened (hydration),
//! * send local changes to the NAS (new and modified files, renames, deletes),
//! * bring changes made on the NAS into the folder (polling).
//!
//! NOTE: this module is type-checked for Windows but, unlike the Linux/macOS
//! engine, has not been exercised by the automated tests.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::ext::FileExt as _;
use cloud_filter::filter::{info, ticket, Request, StateWatchEvent, SyncFilter};
use cloud_filter::metadata::{Metadata, MetadataExt as _};
use cloud_filter::placeholder::{ConvertOptions, PinOptions, PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use cloud_filter::root::{HydrationPolicy, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder, SyncRootInfo};
use cloud_filter::usn::Usn;
use cloud_filter::utility::WriteAt as _;
use futures_util::StreamExt;
use nasdrive::config::Config;
use nasdrive::remote::{self, norm_etag, Entry, Remote, RemoteError, Version};
use nasdrive::webdav::WebDav;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::service::{EntryView, Listing, MountInfo};

const ATTR_PINNED: u32 = 0x0008_0000;
const ATTR_UNPINNED: u32 = 0x0010_0000;
/// Quiet time after the last change to a file before we look at it.
const SETTLE: Duration = Duration::from_secs(2);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const NO_USN: Option<&mut Usn> = None;

struct Engine {
    root: PathBuf,
    /// `root` as text, without `\\?\` prefix or trailing separator.
    root_text: String,
    remote: Arc<WebDav>,
    rt: Handle,
    poll_interval: Duration,
    atomic_upload: AtomicBool,
    part_seq: AtomicU64,
    /// Items the engine itself is removing or renaming right now, so the
    /// callbacks those operations trigger are not echoed back to the NAS.
    ours: Mutex<HashSet<String>>,
    /// Local paths to examine, and when.
    queue: Mutex<HashMap<PathBuf, Instant>>,
    failures: Mutex<HashMap<PathBuf, (u32, String)>>,
    /// Whether the NAS answered the last time we asked.
    online: AtomicBool,
    /// Folders to re-read from the NAS as soon as possible.
    poke: Mutex<HashSet<String>>,
    /// Remote folders known to exist (saves MKCOL round trips).
    known_dirs: Mutex<HashSet<String>>,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Windows FILETIME (100 ns ticks since 1601) for a Unix timestamp.
fn filetime(unix: i64) -> i64 {
    (unix.max(0) + 11_644_473_600) * 10_000_000
}

fn clean(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).trim_end_matches('\\').to_string()
}

/// Names Windows cannot represent are left alone on the NAS.
fn usable_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    !name.is_empty()
        && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
        && !name.chars().any(|c| (c as u32) < 32)
        && !name.ends_with(['.', ' '])
        && !RESERVED.contains(&stem.as_str())
        && !remote::is_part_file(name)
}

/// Lock files, Office temporaries and shell litter never go to the NAS.
fn local_only(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("~$") || lower.starts_with(".~lock") || lower.ends_with(".tmp") || lower.ends_with(".crdownload") || lower.ends_with(".partial") || matches!(lower.as_str(), "desktop.ini" | "thumbs.db")
}

fn blob(v: &Version) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

fn from_blob(data: &[u8]) -> Option<Version> {
    serde_json::from_slice(data).ok()
}

fn metadata_for(e: &Entry) -> Metadata {
    let t = filetime(e.version.mtime);
    let m = if e.is_dir { Metadata::directory() } else { Metadata::file().size(e.version.size) };
    m.creation_time(t).last_write_time(t).last_access_time(t).change_time(t)
}

fn create_info(e: &Entry) -> PlaceholderCreateInfo {
    PlaceholderCreateInfo::new(&e.name).metadata(metadata_for(e)).blob(blob(&e.version)).mark_in_sync()
}

fn cloud_error(e: &RemoteError) -> CloudErrorKind {
    match e {
        e if e.is_offline() => CloudErrorKind::NetworkUnavailable,
        RemoteError::Unauthorized | RemoteError::Forbidden => CloudErrorKind::AccessDenied,
        _ => CloudErrorKind::Unsuccessful,
    }
}

fn api(e: impl std::fmt::Display) -> RemoteError {
    RemoteError::Other(format!("Cloud Files API: {e}"))
}

/// What `Placeholder` knows about a path: (is a placeholder, in sync, fully on disk, stored version).
struct Local {
    placeholder: bool,
    in_sync: bool,
    hydrated: bool,
    on_disk: u64,
    base: Option<Version>,
}

fn inspect(path: &Path, len: u64) -> Result<Local> {
    let info = Placeholder::open(path)?.info()?;
    Ok(match info {
        Some(i) => Local { placeholder: true, in_sync: i.is_in_sync(), hydrated: i.on_disk_data_size().max(0) as u64 >= len, on_disk: i.on_disk_data_size().max(0) as u64, base: from_blob(i.blob()) },
        None => Local { placeholder: false, in_sync: false, hydrated: true, on_disk: len, base: None },
    })
}

/// True for folders whose children have all been materialised.
fn populated(path: &Path) -> bool {
    Placeholder::open(path).ok().and_then(|p| p.state().ok().flatten()).is_some_and(|s| !s.partial_on_disk())
}

impl Engine {
    /// Path relative to the sync folder, `/`-separated; `None` when outside it.
    fn rel(&self, path: &Path) -> Option<String> {
        let text = clean(path);
        let head = text.get(..self.root_text.len())?;
        let rest = &text[self.root_text.len()..];
        if !head.eq_ignore_ascii_case(&self.root_text) || !(rest.is_empty() || rest.starts_with('\\')) {
            return None;
        }
        Some(rest.split('\\').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/"))
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(rel.split('/').filter(|s| !s.is_empty()));
        path
    }

    fn is_ours(&self, rel: &str) -> bool {
        self.ours.lock().unwrap().contains(&rel.to_lowercase())
    }

    /// Runs `f` with the given items marked as "being changed by the engine".
    fn as_ours<T>(&self, rels: &[&str], f: impl FnOnce() -> T) -> T {
        let keys: Vec<String> = rels.iter().map(|r| r.to_lowercase()).collect();
        self.ours.lock().unwrap().extend(keys.iter().cloned());
        let out = f();
        let mut ours = self.ours.lock().unwrap();
        for k in &keys {
            ours.remove(k);
        }
        out
    }

    fn enqueue(&self, path: PathBuf, delay: Duration) {
        self.queue.lock().unwrap().insert(path, Instant::now() + delay);
    }

    fn part_name(&self) -> String {
        format!(".{:x}-{:x}-{:x}{}", std::process::id(), unix_now(), self.part_seq.fetch_add(1, Ordering::SeqCst), remote::PART_SUFFIX)
    }

    /// Creates on the NAS every folder above `rel` that is not known to exist.
    async fn ensure_parents(&self, rel: &str) -> Result<(), RemoteError> {
        let parts: Vec<&str> = rel.split('/').collect();
        for depth in 1..parts.len() {
            let dir = parts[..depth].join("/");
            if self.known_dirs.lock().unwrap().contains(&dir) {
                continue;
            }
            self.remote.mkdir(&dir).await?;
            self.known_dirs.lock().unwrap().insert(dir);
        }
        Ok(())
    }

    /// Uploads a local file (to a temporary name first, when the server allows
    /// it) and returns the version the server then reports.
    async fn put(&self, rel: &str, source: &Path, size: u64, mtime: i64) -> Result<Version, RemoteError> {
        if self.atomic_upload.load(Ordering::SeqCst) {
            let tmp = remote::join(remote::split(rel).0, &self.part_name());
            let moved = match self.remote.upload(&tmp, source, size, mtime).await {
                Ok(()) => self.remote.rename(&tmp, rel, false, true).await,
                Err(e) => Err(e),
            };
            match moved {
                Ok(()) => {}
                Err(e) => {
                    let _ = self.remote.delete(&tmp, false).await;
                    if !matches!(e, RemoteError::Http(405 | 501, _) | RemoteError::Precondition(_) | RemoteError::Forbidden) {
                        return Err(e);
                    }
                    log::warn!("the server cannot rename over an existing file ({e}); uploading in place from now on");
                    self.atomic_upload.store(false, Ordering::SeqCst);
                    self.remote.upload(rel, source, size, mtime).await?;
                }
            }
        } else {
            self.remote.upload(rel, source, size, mtime).await?;
        }
        self.remote.stat(rel).await?.map(|e| e.version).ok_or_else(|| RemoteError::Other("the uploaded file is not on the server".into()))
    }

    // ------------------------------------------------------------------
    // Local changes -> NAS

    /// Looks at one local path that changed (or whose pin state changed).
    fn process_local(&self, path: &Path) -> Result<()> {
        let Some(rel) = self.rel(path).filter(|r| !r.is_empty()) else { return Ok(()) };
        let name = remote::split(&rel).1;
        if local_only(name) || !usable_name(name) || self.is_ours(&rel) {
            return Ok(());
        }
        let Ok(meta) = fs::symlink_metadata(path) else { return Ok(()) }; // gone again
        let attrs = meta.file_attributes();
        let local = inspect(path, meta.len())?;

        if meta.is_dir() {
            if !local.placeholder {
                // A folder created here: create it on the NAS and adopt it.
                self.rt.block_on(async {
                    self.ensure_parents(&rel).await?;
                    self.remote.mkdir(&rel).await
                })?;
                self.known_dirs.lock().unwrap().insert(rel.clone());
                Placeholder::options().write_access().open(path)?.convert_to_placeholder(ConvertOptions::default().mark_in_sync().blob(blob(&Version::default())), NO_USN)?;
                log::info!("created folder {rel} on the NAS");
                // Whatever was put in it before we got here needs a look too.
                for child in fs::read_dir(path)?.flatten() {
                    self.enqueue(child.path(), Duration::ZERO);
                }
            } else if attrs & ATTR_PINNED != 0 {
                self.hydrate_tree(path);
            }
            return Ok(());
        }

        if local.in_sync {
            // Content unchanged; only the pin state may need acting upon.
            if attrs & ATTR_PINNED != 0 && !local.hydrated {
                // "Always keep on this device"
                Placeholder::open(path)?.hydrate(..)?;
            } else if attrs & ATTR_UNPINNED != 0 && local.hydrated && meta.len() > 0 {
                // "Free up space": drop the data, then clear the request so
                // the file may be opened (and kept) again later.
                fs::OpenOptions::new().write(true).open(path)?.dehydrate(..)?;
                Placeholder::options().write_access().open(path)?.mark_pin(PinState::Unspecified, PinOptions::default())?;
            }
            return Ok(());
        }

        // New or modified file: send it.
        if !local.hydrated {
            Placeholder::open(path)?.hydrate(..)?;
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or_else(unix_now, |d| d.as_secs() as i64);
        let outcome = self.rt.block_on(async {
            self.ensure_parents(&rel).await?;
            if let Some(current) = self.remote.stat(&rel).await? {
                let ours = !current.is_dir && local.base.as_ref().is_some_and(|b| b.same(&current.version));
                if !ours {
                    return Ok(None); // changed on both sides
                }
            }
            self.put(&rel, path, meta.len(), mtime).await.map(Some)
        });
        let version = match outcome {
            Ok(Some(v)) => v,
            Ok(None) => return self.keep_both(path, &rel),
            Err(e) => return Err(anyhow!("upload of {rel} failed: {e}")),
        };
        // Only call it "in sync" if nobody touched it while it was uploading.
        let after = fs::symlink_metadata(path)?;
        if after.len() != meta.len() || after.last_write_time() != meta.last_write_time() {
            self.enqueue(path.to_path_buf(), SETTLE);
            return Ok(());
        }
        let mut ph = Placeholder::options().write_access().open(path)?;
        let identity = blob(&version);
        if local.placeholder {
            ph.update(UpdateOptions::default().mark_in_sync().blob(&identity), NO_USN)?;
        } else {
            ph.convert_to_placeholder(ConvertOptions::default().mark_in_sync().blob(identity), NO_USN)?;
        }
        log::info!("uploaded {rel} ({})", nasdrive::config::format_size(meta.len()));
        Ok(())
    }

    /// The file changed here and on the NAS: the local version gets a new
    /// name (and is uploaded under it), the NAS version comes back under the
    /// original one.
    fn keep_both(&self, path: &Path, rel: &str) -> Result<()> {
        let dir = path.parent().context("no parent")?;
        let name = remote::conflict_name(remote::split(rel).1, unix_now(), |c| dir.join(c).exists());
        let target = dir.join(&name);
        let new_rel = remote::join(remote::split(rel).0, &name);
        self.as_ours(&[rel, &new_rel], || fs::rename(path, &target))?;
        log::warn!("{rel} changed both here and on the NAS; keeping the local version as {name:?}");
        self.enqueue(target, Duration::ZERO);
        self.poke.lock().unwrap().insert(remote::split(rel).0.to_string());
        Ok(())
    }

    /// Downloads everything below a folder marked "Always keep on this device".
    fn hydrate_tree(&self, dir: &Path) {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            // Enumerating makes Windows ask us for the folder's content if needed.
            let Ok(entries) = fs::read_dir(&d) else { continue };
            for entry in entries.flatten() {
                let (path, Ok(meta)) = (entry.path(), entry.metadata()) else { continue };
                if meta.is_dir() {
                    stack.push(path);
                } else if inspect(&path, meta.len()).is_ok_and(|l| l.placeholder && l.in_sync && !l.hydrated) {
                    if let Err(e) = Placeholder::open(&path).and_then(|mut p| p.hydrate(..)) {
                        log::warn!("could not download {}: {e}", path.display());
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // NAS changes -> local

    /// Compares one populated folder with the NAS and applies the differences.
    /// Returns the sub-folders that are populated too (to be visited next).
    fn poll_dir(&self, rel: &str) -> Result<Vec<String>> {
        let dir = self.abs(rel);
        if !populated(&dir) {
            return Ok(Vec::new());
        }
        let entries = match self.rt.block_on(self.remote.list(rel)) {
            Ok(e) => e,
            Err(RemoteError::NotFound) => return Ok(Vec::new()), // the parent's pass deals with it
            Err(e) => {
                if e.is_offline() {
                    self.online.store(false, Ordering::SeqCst);
                }
                bail!("cannot list {rel:?}: {e}")
            }
        };
        self.online.store(true, Ordering::SeqCst);
        self.known_dirs.lock().unwrap().insert(rel.to_string());
        let by_name: HashMap<&str, &Entry> = entries.iter().map(|e| (e.name.as_str(), e)).collect();
        let pinned_dir = fs::metadata(&dir).is_ok_and(|m| m.file_attributes() & ATTR_PINNED != 0);
        let mut seen = HashSet::new();
        let mut sub = Vec::new();
        for item in fs::read_dir(&dir)?.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            let child = remote::join(rel, &name);
            seen.insert(name.to_lowercase());
            if local_only(&name) || !usable_name(&name) {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            let Ok(local) = inspect(&path, meta.len()) else { continue }; // busy: next time
            match by_name.get(name.as_str()) {
                Some(e) if local.placeholder => {
                    if e.is_dir != meta.is_dir() {
                        // A file became a folder or vice versa.
                        if local.in_sync && !self.has_unsynced(&path) {
                            self.remove_local(&path, &child);
                            seen.remove(&name.to_lowercase());
                        }
                    } else if meta.is_dir() {
                        sub.push(child);
                    } else if local.in_sync && !local.base.as_ref().is_some_and(|b| b.same(&e.version)) {
                        // Changed on the NAS: new metadata, stale data dropped.
                        let identity = blob(&e.version);
                        let res = Placeholder::options().write_access().open(&path).and_then(|mut p| {
                            p.update(UpdateOptions::default().metadata(metadata_for(e)).blob(&identity).dehydrate().mark_in_sync(), NO_USN)?;
                            if meta.file_attributes() & ATTR_PINNED != 0 {
                                p.hydrate(..)?;
                            }
                            Ok(())
                        });
                        match res {
                            Ok(()) => log::info!("{child} changed on the NAS"),
                            Err(e) => log::debug!("could not refresh {child} yet: {e}"),
                        }
                    }
                }
                None if local.placeholder && local.in_sync => {
                    // Deleted on the NAS (unless something below still has to be uploaded).
                    if !self.has_unsynced(&path) {
                        self.remove_local(&path, &child);
                    }
                }
                // Plain local file, or local edits pending: the upload path handles it.
                _ => {
                    if !self.queue.lock().unwrap().contains_key(&path) {
                        self.enqueue(path, SETTLE);
                    }
                }
            }
        }
        for e in entries.iter().filter(|e| usable_name(&e.name) && !seen.contains(&e.name.to_lowercase())) {
            match create_info(e).create(&dir) {
                Ok(_) if pinned_dir && !e.is_dir => {
                    let path = dir.join(&e.name);
                    if let Err(err) = Placeholder::open(&path).and_then(|mut p| p.hydrate(..)) {
                        log::warn!("could not download {}: {err}", path.display());
                    }
                }
                Ok(_) => {}
                Err(err) => log::debug!("could not create {}: {err}", e.name),
            }
        }
        Ok(sub)
    }

    /// Whether anything below `path` still has to be uploaded.
    fn has_unsynced(&self, path: &Path) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else { return false };
        if !meta.is_dir() {
            return !inspect(path, meta.len()).is_ok_and(|l| l.placeholder && l.in_sync);
        }
        if !populated(path) {
            return false; // never opened: nothing local in there
        }
        fs::read_dir(path).map(|it| it.flatten().any(|e| !local_only(&e.file_name().to_string_lossy()) && self.has_unsynced(&e.path()))).unwrap_or(true)
    }

    /// Removes a local item because it no longer exists on the NAS.
    fn remove_local(&self, path: &Path, rel: &str) {
        let res = self.as_ours(&[rel], || if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) });
        match res {
            Ok(()) => log::info!("{rel} was removed on the NAS"),
            Err(e) => log::debug!("could not remove {rel} yet: {e}"),
        }
    }

    fn poll_tree(&self) {
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            match self.poll_dir(&rel) {
                Ok(children) => stack.extend(children),
                Err(e) => log::debug!("{e:#}"),
            }
        }
    }

    /// Background loop: handles queued local changes and polls the NAS.
    fn work(&self, stop: &AtomicBool) {
        let mut next_poll = Instant::now() + Duration::from_secs(2);
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
            let now = Instant::now();
            let mut due: Vec<PathBuf> = {
                let mut queue = self.queue.lock().unwrap();
                let due: Vec<PathBuf> = queue.iter().filter(|(_, at)| **at <= now).map(|(p, _)| p.clone()).collect();
                due.iter().for_each(|p| {
                    queue.remove(p);
                });
                due
            };
            due.sort(); // folders before what is inside them
            for path in due {
                match self.process_local(&path) {
                    Ok(()) => {
                        self.failures.lock().unwrap().remove(&path);
                    }
                    Err(e) => {
                        // Typically: the file is still open in an application, or the NAS is away.
                        let attempts = {
                            let mut failures = self.failures.lock().unwrap();
                            let n = failures.entry(path.clone()).or_insert((0, String::new()));
                            n.0 += 1;
                            n.1 = format!("{e:#}");
                            n.0
                        };
                        let delay = Duration::from_secs((10u64 << attempts.min(6)).min(600));
                        if attempts == 1 || attempts % 10 == 0 {
                            log::warn!("{}: {e:#} (retrying in {}s)", path.display(), delay.as_secs());
                        }
                        self.enqueue(path, delay);
                    }
                }
            }
            let poked: Vec<String> = self.poke.lock().unwrap().drain().collect();
            for rel in poked {
                if let Err(e) = self.poll_dir(&rel) {
                    log::debug!("{e:#}");
                }
            }
            if Instant::now() >= next_poll {
                self.poll_tree();
                next_poll = Instant::now() + if self.online.load(Ordering::SeqCst) { self.poll_interval } else { Duration::from_secs(15) };
            }
        }
    }
}

/// The callbacks Windows invokes on us.
struct Hooks(Arc<Engine>);

impl SyncFilter for Hooks {
    /// A placeholder is being read: supply its bytes.
    fn fetch_data(&self, request: Request, ticket: ticket::FetchData, info: info::FetchData) -> CResult<()> {
        let eng = &self.0;
        let path = request.path();
        let rel = eng.rel(&path).ok_or(CloudErrorKind::NotUnderSyncRoot)?;
        let size = request.file_size();
        let range = info.required_file_range();
        let (start, end) = (range.start, range.end.min(size));
        if start >= end {
            return Ok(());
        }
        let expected = from_blob(request.file_blob());
        let res: Result<(), RemoteError> = eng.rt.block_on(async {
            let mut dl = eng.remote.open_range(&rel, start, Some(end)).await?;
            let etag_differs = match (expected.as_ref().and_then(|v| v.etag.as_deref()), dl.etag.as_deref()) {
                (Some(a), Some(b)) => norm_etag(a) != norm_etag(b),
                _ => false,
            };
            if etag_differs || dl.total.is_some_and(|t| t != size) {
                return Err(RemoteError::Changed);
            }
            let mut skip = dl.skip;
            let mut pos = start;
            let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
            while pos + (buf.len() as u64) < end {
                let chunk = match tokio::time::timeout(CHUNK_TIMEOUT, dl.stream.next()).await {
                    Err(_) => return Err(RemoteError::Network("download stalled".into())),
                    Ok(None) => return Err(RemoteError::Network("the server closed the connection early".into())),
                    Ok(Some(chunk)) => chunk?,
                };
                let mut data = &chunk[..];
                if skip > 0 {
                    let s = skip.min(data.len() as u64) as usize;
                    data = &data[s..];
                    skip -= s as u64;
                }
                let room = (end - pos) as usize - buf.len();
                buf.extend_from_slice(&data[..data.len().min(room)]);
                // The API wants whole 4 KiB blocks, except at the very end.
                let whole = buf.len() & !4095;
                if whole >= 256 << 10 {
                    ticket.write_at(&buf[..whole], pos).map_err(api)?;
                    pos += whole as u64;
                    buf.drain(..whole);
                    let _ = ticket.report_progress(end - start, pos - start);
                }
            }
            if !buf.is_empty() {
                ticket.write_at(&buf, pos).map_err(api)?;
            }
            Ok(())
        });
        res.map_err(|e| {
            if e.is_offline() {
                eng.online.store(false, Ordering::SeqCst);
            }
            log::warn!("could not download {rel}: {e}");
            if matches!(e, RemoteError::Changed | RemoteError::NotFound) {
                // Our picture of this file is out of date: look at its folder again.
                eng.poke.lock().unwrap().insert(remote::split(&rel).0.to_string());
            }
            cloud_error(&e)
        })
    }

    /// A folder is being opened for the first time: say what is in it.
    fn fetch_placeholders(&self, request: Request, ticket: ticket::FetchPlaceholders, _info: info::FetchPlaceholders) -> CResult<()> {
        let eng = &self.0;
        let rel = eng.rel(&request.path()).ok_or(CloudErrorKind::NotUnderSyncRoot)?;
        let entries = match eng.rt.block_on(eng.remote.list(&rel)) {
            Ok(entries) => entries,
            Err(RemoteError::NotFound) => Vec::new(), // gone on the NAS: an empty folder, removed at the next poll
            Err(e) => {
                log::warn!("cannot list {rel:?}: {e}");
                return Err(cloud_error(&e));
            }
        };
        eng.known_dirs.lock().unwrap().insert(rel.clone());
        let mut infos: Vec<PlaceholderCreateInfo> = entries
            .iter()
            .filter(|e| {
                let ok = usable_name(&e.name);
                if !ok && !remote::is_part_file(&e.name) {
                    log::warn!("{:?} in {rel:?} has a name Windows cannot use; skipping it", e.name);
                }
                ok
            })
            .map(create_info)
            .collect();
        ticket.pass_with_placeholders(&mut infos).map_err(|e| {
            log::warn!("could not populate {rel:?}: {e}");
            CloudErrorKind::Unsuccessful
        })
    }

    /// Something is about to be deleted: delete it on the NAS first.
    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        let eng = &self.0;
        if let Some(rel) = eng.rel(&request.path()).filter(|r| !r.is_empty() && !eng.is_ours(r)) {
            if !local_only(remote::split(&rel).1) {
                if let Err(e) = eng.rt.block_on(eng.remote.delete(&rel, info.is_directory())) {
                    log::warn!("cannot delete {rel} on the NAS: {e}");
                    return Err(cloud_error(&e));
                }
                log::info!("deleted {rel} on the NAS");
            }
        }
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// Something is about to be renamed or moved: do the same on the NAS first.
    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let eng = &self.0;
        let from = eng.rel(&request.path()).filter(|r| !r.is_empty());
        let to = eng.rel(&info.target_path()).filter(|r| !r.is_empty());
        let is_dir = info.is_directory();
        let res = match (&from, &to) {
            (Some(f), _) if eng.is_ours(f) => Ok(()),
            (Some(f), Some(t)) => eng.rt.block_on(async {
                eng.ensure_parents(t).await?;
                match eng.remote.rename(f, t, is_dir, true).await {
                    // Never made it to the NAS: it will be uploaded from its new place.
                    Err(RemoteError::NotFound) => Ok(()),
                    other => other,
                }
            }),
            // Moved out of the sync folder (the Recycle Bin, for instance).
            (Some(f), None) => eng.rt.block_on(eng.remote.delete(f, is_dir)),
            // Moved in: it shows up as a new item and gets uploaded.
            (None, _) => Ok(()),
        };
        if let Err(e) = res {
            log::warn!("cannot move {from:?} to {to:?} on the NAS: {e}");
            return Err(cloud_error(&e));
        }
        if is_dir {
            eng.known_dirs.lock().unwrap().clear();
        }
        if let (Some(f), Some(t)) = (&from, &to) {
            if !eng.is_ours(f) {
                log::info!("moved {f} to {t} on the NAS");
            }
        }
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// "Free up space" (or Storage Sense): fine by us, the data is on the NAS.
    fn dehydrate(&self, _request: Request, ticket: ticket::Dehydrate, _info: info::Dehydrate) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// Files were created, modified, renamed or (un)pinned in the sync folder.
    fn state_changed(&self, event: StateWatchEvent) {
        match event {
            StateWatchEvent::Changed(paths) => {
                for path in paths {
                    self.0.enqueue(path, SETTLE);
                }
            }
            StateWatchEvent::Stopped(e) => log::error!("the folder watcher stopped ({e}); restart nasdrive to pick up local changes again"),
        }
    }
}

fn sync_root_id(cfg: &Config) -> Result<SyncRootId> {
    // One registration per (address, user), so several NAS folders can coexist.
    let account = cfg.state_dir()?.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(SyncRootIdBuilder::new("nasdrive").user_security_id(SecurityId::current_user()?).account_name(account).build())
}

/// A connected sync folder: the Cloud Files connection plus the worker that
/// uploads local changes and polls the NAS. Both live on one dedicated thread.
pub struct Mounted {
    engine: Arc<Engine>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    ended: Arc<watch::Sender<bool>>,
}

fn ensure_registered(cfg: &Config, root: &Path) -> Result<()> {
    let id = sync_root_id(cfg)?;
    if id.is_registered()? {
        return Ok(());
    }
    let host = cfg.url.split("://").nth(1).and_then(|s| s.split(['/', ':']).next()).unwrap_or("NAS");
    id.register(
        SyncRootInfo::default()
            .with_display_name(format!("NAS ({host})"))
            .with_icon("%SystemRoot%\\system32\\shell32.dll,9")
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_hydration_type(HydrationType::Full)
            .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
            .with_population_type(PopulationType::Full)
            .with_allow_pinning(true)
            .with_path(root)?,
    )
    .with_context(|| format!("cannot register {} as a sync folder (it must be on an NTFS drive)", root.display()))?;
    log::info!("registered {} with Windows", root.display());
    Ok(())
}

impl Mounted {
    pub async fn start(cfg: &Config) -> Result<Self> {
        if !cloud_filter::root::is_supported()? {
            bail!("this Windows version has no Cloud Files API (Windows 10 1709 or later is required)");
        }
        let root = cfg.mountpoint();
        fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
        let (dav, online) = crate::connect(cfg).await?;
        if !online {
            log::warn!("only files already on this computer are available until the NAS is back");
        }
        ensure_registered(cfg, &root)?;
        let engine = Arc::new(Engine {
            root_text: clean(&root),
            root: root.clone(),
            remote: Arc::new(dav),
            rt: Handle::current(),
            poll_interval: Duration::from_secs(cfg.poll_interval_secs.max(15)),
            atomic_upload: AtomicBool::new(cfg.atomic_upload),
            part_seq: AtomicU64::new(0),
            ours: Mutex::default(),
            queue: Mutex::default(),
            failures: Mutex::default(),
            poke: Mutex::default(),
            known_dirs: Mutex::new(HashSet::from([String::new()])),
            online: AtomicBool::new(online),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(watch::channel(false).0);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let worker = {
            let (engine, stop, ended, root) = (engine.clone(), stop.clone(), ended.clone(), root.clone());
            std::thread::Builder::new().name("nasdrive-sync".into()).spawn(move || {
                match Session::new().connect(&root, Hooks(engine.clone())) {
                    Ok(connection) => {
                        let _ = ready_tx.send(Ok(()));
                        engine.work(&stop);
                        drop(connection);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::Error::new(e).context("cannot connect to the sync folder")));
                    }
                }
                ended.send_replace(true);
            })?
        };
        tokio::task::spawn_blocking(move || ready_rx.recv()).await?.map_err(|_| anyhow!("the sync thread stopped unexpectedly"))??;
        log::info!("{} is available in {}", cfg.url, root.display());
        Ok(Self { engine, stop, worker: Mutex::new(Some(worker)), ended })
    }

    pub async fn wait_ended(&self) {
        let mut rx = self.ended.subscribe();
        let _ = rx.wait_for(|done| *done).await;
    }

    pub async fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let worker = self.worker.lock().unwrap().take();
        if let Some(w) = worker {
            let _ = tokio::task::spawn_blocking(move || w.join()).await;
        }
        let pending = self.engine.queue.lock().unwrap().len();
        if pending > 0 {
            log::warn!("{pending} local change(s) not sent yet; they will be picked up next time");
        }
    }

    pub fn info(&self) -> MountInfo {
        let e = &self.engine;
        let errors = e.failures.lock().unwrap().iter().take(10).map(|(p, (n, msg))| format!("{}: {msg} ({n}x)", e.rel(p).unwrap_or_default())).collect();
        MountInfo { online: e.online.load(Ordering::SeqCst), pending_uploads: e.queue.lock().unwrap().len() as u64, uploading: 0, cache_bytes: None, cache_max: None, errors }
    }

    pub async fn browse(&self, rel: &str) -> Result<Listing> {
        let (e, rel) = (self.engine.clone(), rel.to_string());
        tokio::task::spawn_blocking(move || e.browse(&rel)).await?
    }

    pub async fn pin(&self, rel: &str, on: bool) -> Result<()> {
        let (e, rel) = (self.engine.clone(), rel.to_string());
        tokio::task::spawn_blocking(move || e.pin(&rel, on)).await?
    }

    pub async fn free(&self, rel: &str) -> Result<u64> {
        let (e, rel) = (self.engine.clone(), rel.to_string());
        tokio::task::spawn_blocking(move || e.free(&rel)).await?
    }

    pub async fn refresh(&self, rel: &str) -> Result<()> {
        self.engine.poke.lock().unwrap().insert(rel.to_string());
        Ok(())
    }
}

/// What the interface needs: listings with states, pin and free.
impl Engine {
    fn browse(&self, rel: &str) -> Result<Listing> {
        let dir = self.abs(rel);
        let meta = fs::metadata(&dir).with_context(|| format!("{rel}: not found"))?;
        if !meta.is_dir() {
            bail!("{rel} is not a folder");
        }
        let this = self.view(remote::split(rel).1, &dir, &meta);
        let mut entries = Vec::new();
        // Enumerating an unopened folder makes Windows ask us for its content.
        for item in fs::read_dir(&dir)?.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if let Ok(m) = item.metadata() {
                entries.push(self.view(&name, &item.path(), &m));
            }
        }
        Ok(Listing { path: rel.to_string(), this, entries })
    }

    fn view(&self, name: &str, path: &Path, meta: &fs::Metadata) -> EntryView {
        let attrs = meta.file_attributes();
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
        let error = self.failures.lock().unwrap().get(path).map(|(_, e)| e.clone());
        let pinned = if attrs & ATTR_PINNED != 0 { "yes" } else { "no" };
        let mut v = EntryView { name: name.to_string(), dir: meta.is_dir(), size: 0, mtime, state: "folder".into(), pinned: pinned.into(), cached: 0, files: 0, files_cached: 0, pending: 0, error };
        if v.dir {
            return v;
        }
        v.size = meta.len();
        let local = inspect(path, meta.len()).ok();
        let state = if local_only(name) {
            "local"
        } else {
            match &local {
                Some(l) if l.placeholder && l.in_sync && l.hydrated => "cached",
                Some(l) if l.placeholder && l.in_sync && l.on_disk > 0 => "partial",
                Some(l) if l.placeholder && l.in_sync => "online",
                _ => "modified",
            }
        };
        v.state = state.into();
        v.cached = local.map_or(meta.len(), |l| if l.hydrated { meta.len() } else { l.on_disk });
        v.files = 1;
        v.files_cached = u64::from(v.cached >= v.size);
        v.pending = u64::from(state == "modified");
        v
    }

    fn pin(&self, rel: &str, on: bool) -> Result<()> {
        let path = self.abs(rel);
        let mut opts = PinOptions::default();
        if path.is_dir() {
            opts.recurse();
        }
        let mut ph = Placeholder::options().write_access().open(&path)?;
        ph.mark_pin(if on { PinState::Pinned } else { PinState::Unspecified }, opts)?;
        if on {
            // The worker sees the pinned attribute and downloads what is missing.
            self.enqueue(path, Duration::ZERO);
        }
        Ok(())
    }

    fn free(&self, rel: &str) -> Result<u64> {
        let path = self.abs(rel);
        // Pinned files cannot be dehydrated: unpin first.
        let mut opts = PinOptions::default();
        if path.is_dir() {
            opts.recurse();
        }
        Placeholder::options().write_access().open(&path)?.mark_pin(PinState::Unspecified, opts)?;
        let mut freed = 0;
        let mut stack = vec![path];
        while let Some(p) = stack.pop() {
            let Ok(meta) = fs::symlink_metadata(&p) else { continue };
            if meta.is_dir() {
                if populated(&p) {
                    stack.extend(fs::read_dir(&p)?.flatten().map(|e| e.path()));
                }
                continue;
            }
            let Ok(l) = inspect(&p, meta.len()) else { continue };
            if !(l.placeholder && l.in_sync && l.on_disk > 0) {
                continue; // modified files keep their data until uploaded
            }
            match fs::OpenOptions::new().write(true).open(&p).map_err(anyhow::Error::from).and_then(|f| f.dehydrate(..).map_err(anyhow::Error::from)) {
                Ok(()) => freed += l.on_disk,
                Err(e) => log::warn!("could not free {}: {e:#}", p.display()),
            }
        }
        Ok(freed)
    }
}

/// Removes the registration; Explorer then treats the folder as an ordinary one.
pub fn unregister(cfg: &Config) -> Result<()> {
    let id = sync_root_id(cfg)?;
    if id.is_registered()? {
        id.unregister()?;
        println!("{} is no longer a sync folder. Files that were only online are now empty shells: delete the folder.", cfg.mountpoint().display());
    } else {
        println!("nothing registered for this configuration");
    }
    Ok(())
}
