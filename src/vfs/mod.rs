//! The on-demand engine behind the FUSE mount (Linux/macOS).
//!
//! The remote tree is mirrored lazily into an in-memory tree of nodes that is
//! persisted in a small index. File contents live in sparse cache files and
//! are fetched range by range when something reads them. Local changes are
//! written to the cache first and uploaded in the background, so they survive
//! the NAS being unreachable.
//!
//! Locking: one `std::sync::Mutex` guards the whole tree. It is never held
//! across an `.await`; network I/O always happens outside of it.

mod sync;
mod transfer;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::activity::{Activity, Transfer};
use crate::config::Config;
use crate::ranges::RangeSet;
use crate::remote::{self, Quota, Remote, RemoteError, Version};
use crate::store::{Record, Store};
use transfer::{Dl, UploadHandle};

/// Extended attributes through which the `nasyncloud` command talks to a running mount.
pub const XATTR_STATUS: &str = "user.nasyncloud.status";
pub const XATTR_INFO: &str = "user.nasyncloud.info";
pub const XATTR_PIN: &str = "user.nasyncloud.pin";
pub const XATTR_EVICT: &str = "user.nasyncloud.evict";
pub const XATTR_REFRESH: &str = "user.nasyncloud.refresh";

pub type NodeId = u64;
pub const ROOT: NodeId = 1;

/// How long an operation waits for a stale folder listing to be refreshed
/// before answering from the cached one.
const REFRESH_WAIT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VfsError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("not a folder")]
    NotDir,
    #[error("is a folder")]
    IsDir,
    #[error("folder not empty")]
    NotEmpty,
    #[error("invalid name or argument")]
    Invalid,
    #[error("the NAS is unreachable ({0})")]
    Offline(String),
    #[error("busy: {0}")]
    Busy(String),
    #[error("permission denied by the NAS")]
    Denied,
    #[error("{0}")]
    Io(String),
}

impl From<RemoteError> for VfsError {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::NotFound => VfsError::NotFound,
            RemoteError::Unauthorized | RemoteError::Forbidden => VfsError::Denied,
            e if e.is_offline() => VfsError::Offline(e.to_string()),
            e => VfsError::Io(e.to_string()),
        }
    }
}

impl From<std::io::Error> for VfsError {
    fn from(e: std::io::Error) -> Self {
        VfsError::Io(format!("local cache: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, VfsError>;

#[derive(Clone, Debug)]
pub struct VfsOptions {
    pub state_dir: PathBuf,
    pub cache_max: u64,
    pub dir_ttl: Duration,
    pub poll_interval: Duration,
    pub upload_delay: Duration,
    pub parallel_uploads: usize,
    pub readahead: u64,
    pub atomic_upload: bool,
}

impl VfsOptions {
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            state_dir: cfg.state_dir()?,
            cache_max: cfg.cache_max_bytes()?,
            dir_ttl: Duration::from_secs(cfg.dir_ttl_secs),
            poll_interval: Duration::from_secs(cfg.poll_interval_secs.max(10)),
            upload_delay: Duration::from_secs(cfg.upload_delay_secs),
            parallel_uploads: cfg.parallel_uploads.clamp(1, 16),
            readahead: cfg.readahead_mb.clamp(1, 1024) << 20,
            atomic_upload: cfg.atomic_upload,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// What the file system layer needs to answer `stat`.
#[derive(Clone, Debug)]
pub struct Attr {
    pub id: NodeId,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    /// Bytes actually stored on this machine.
    pub local_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pin {
    No,
    Yes,
    Inherited,
}

/// User-visible state of a file or folder.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NodeStatus {
    pub dir: bool,
    /// `online`, `partial`, `cached`, `modified`, `uploading`, `local` or `folder`.
    pub state: String,
    pub pinned: Pin,
    pub size: u64,
    pub cached: u64,
    /// (folders) number of known files below and how many are fully local.
    pub files: u64,
    pub files_cached: u64,
    /// Files below (or this one) still waiting to be uploaded.
    pub pending: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Info {
    pub online: bool,
    pub pending_uploads: u64,
    pub uploading: u64,
    pub cache_bytes: u64,
    pub cache_max: u64,
    pub nodes: u64,
    pub errors: Vec<String>,
}

struct Node {
    // ---- persisted (see `Record`)
    id: NodeId,
    parent: NodeId,
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
    remote: Option<Version>,
    pinned: bool,
    cached: RangeSet,
    cache_gen: u32,
    dirty: bool,
    atime: i64,
    listed: bool,
    // ---- runtime only
    children: BTreeMap<String, NodeId>,
    listed_at: Option<Instant>,
    /// Bumped on every local change to the folder's namespace, so a listing
    /// fetched before the change is not applied on top of it.
    mut_gen: u64,
    refresh_lock: Option<Arc<tokio::sync::Mutex<()>>>,
    opens: u32,
    writers: u32,
    /// Removed from the namespace but still open somewhere.
    unlinked: bool,
    /// Bumped on every local content change (detects edits during an upload).
    local_gen: u64,
    file: Option<Arc<File>>,
    dls: Vec<Arc<Dl>>,
    progress: Option<Arc<watch::Sender<u64>>>,
    upload: Option<UploadHandle>,
    /// The cache file holds data that has not been fsynced yet.
    unsynced: bool,
    /// An interrupted upload of ours may have left a half-written copy on the
    /// server: the next upload overwrites it without treating it as a conflict.
    overwrite_ok: bool,
    error: Option<String>,
}

impl Node {
    fn new(id: NodeId, parent: NodeId, name: &str, kind: Kind) -> Self {
        Node {
            id,
            parent,
            name: name.to_string(),
            kind,
            size: 0,
            mtime: unix_now(),
            remote: None,
            pinned: false,
            cached: RangeSet::new(),
            cache_gen: 0,
            dirty: false,
            atime: 0,
            listed: false,
            children: BTreeMap::new(),
            listed_at: None,
            mut_gen: 0,
            refresh_lock: None,
            opens: 0,
            writers: 0,
            unlinked: false,
            local_gen: 0,
            file: None,
            dls: Vec::new(),
            progress: None,
            upload: None,
            unsynced: false,
            overwrite_ok: false,
            error: None,
        }
    }

    fn from_record(r: Record) -> Self {
        let mut n = Node::new(r.id, r.parent, &r.name, if r.dir { Kind::Dir } else { Kind::File });
        n.size = r.size;
        n.mtime = r.mtime;
        n.remote = r.remote;
        n.pinned = r.pinned;
        n.cached = r.cached;
        n.cache_gen = r.cache_gen;
        n.dirty = r.dirty;
        n.atime = r.atime;
        n.listed = r.listed;
        n
    }

    fn record(&self) -> Record {
        Record {
            id: self.id,
            parent: self.parent,
            name: self.name.clone(),
            dir: self.kind == Kind::Dir,
            size: self.size,
            mtime: self.mtime,
            remote: self.remote.clone(),
            pinned: self.pinned,
            cached: self.cached.clone(),
            cache_gen: self.cache_gen,
            dirty: self.dirty,
            atime: self.atime,
            listed: self.listed,
        }
    }

    fn attr(&self) -> Attr {
        Attr { id: self.id, kind: self.kind, size: self.size, mtime: self.mtime, local_bytes: self.cached.len() }
    }

    /// All the content is on this machine.
    fn complete(&self) -> bool {
        self.cached.is_complete(self.size)
    }

    /// Exists only here (or was changed here) and still has to reach the server.
    fn local_change(&self) -> bool {
        self.remote.is_none() || self.dirty
    }
}

struct Job {
    due: Instant,
    attempts: u32,
}

struct State {
    nodes: HashMap<NodeId, Node>,
    next_id: NodeId,
    /// Bytes held in cache files (all files, including pinned and modified ones).
    cache_bytes: u64,
    /// Pending uploads / remote folder creations, by node.
    jobs: HashMap<NodeId, Job>,
    uploading: usize,
    /// Index records to rewrite / delete at the next flush.
    stale: HashSet<NodeId>,
    gone: Vec<NodeId>,
}

impl State {
    fn get(&self, id: NodeId) -> Result<&Node> {
        self.nodes.get(&id).ok_or(VfsError::NotFound)
    }

    fn get_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        self.nodes.get_mut(&id).ok_or(VfsError::NotFound)
    }

    fn dir(&self, id: NodeId) -> Result<&Node> {
        let n = self.get(id)?;
        if n.kind == Kind::Dir {
            Ok(n)
        } else {
            Err(VfsError::NotDir)
        }
    }

    fn dir_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        let n = self.get_mut(id)?;
        if n.kind == Kind::Dir {
            Ok(n)
        } else {
            Err(VfsError::NotDir)
        }
    }

    fn file(&self, id: NodeId) -> Result<&Node> {
        let n = self.get(id)?;
        if n.kind == Kind::File {
            Ok(n)
        } else {
            Err(VfsError::IsDir)
        }
    }

    fn file_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        let n = self.get_mut(id)?;
        if n.kind == Kind::File {
            Ok(n)
        } else {
            Err(VfsError::IsDir)
        }
    }

    fn child(&self, parent: NodeId, name: &str) -> Result<NodeId> {
        self.dir(parent)?.children.get(name).copied().ok_or(VfsError::NotFound)
    }

    /// Path relative to the remote root; `None` for nodes no longer in the tree.
    fn path(&self, id: NodeId) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT {
            let n = self.nodes.get(&cur)?;
            if n.unlinked || parts.len() > 1024 {
                return None;
            }
            parts.push(n.name.as_str());
            cur = n.parent;
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    fn pin_of(&self, id: NodeId) -> Pin {
        let mut cur = id;
        let mut depth = 0;
        while let Some(n) = self.nodes.get(&cur) {
            if n.pinned {
                return if cur == id { Pin::Yes } else { Pin::Inherited };
            }
            if cur == ROOT || depth > 1024 {
                break;
            }
            cur = n.parent;
            depth += 1;
        }
        Pin::No
    }

    fn touch(&mut self, id: NodeId) {
        self.stale.insert(id);
    }

    /// The node and everything below it (parents before children).
    fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            if let Some(n) = self.nodes.get(&out[i]) {
                out.extend(n.children.values().copied());
            }
            i += 1;
        }
        out
    }

    fn has_local_changes(&self, id: NodeId) -> bool {
        self.subtree(id).iter().any(|i| self.nodes.get(i).is_some_and(Node::local_change))
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Links a freshly built node into its parent.
    fn insert(&mut self, node: Node) {
        let (id, parent, name) = (node.id, node.parent, node.name.clone());
        self.nodes.insert(id, node);
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.insert(name, id);
        }
        self.touch(id);
    }

    fn bump(&mut self, dir: NodeId) {
        if let Some(d) = self.nodes.get_mut(&dir) {
            d.mut_gen += 1;
            d.mtime = unix_now();
        }
    }
}

pub struct Vfs {
    opts: VfsOptions,
    remote: Arc<dyn Remote>,
    store: Arc<Store>,
    data_dir: PathBuf,
    state: Mutex<State>,
    online: AtomicBool,
    atomic_upload: AtomicBool,
    stop: AtomicBool,
    stop_notify: Notify,
    flush_wake: Notify,
    upload_wake: Notify,
    pin_wake: Notify,
    evict_wake: Notify,
    quota: Mutex<Option<Quota>>,
    part_seq: AtomicU64,
    /// Index commits made so far without fsync, and how many of them a later
    /// fsynced commit has made permanent.
    commits: AtomicU64,
    durable_at: AtomicU64,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Recent uploads and downloads, for the interface.
    activity: Activity,
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Names that make sense locally but should never be sent to the NAS.
fn is_local_only_name(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini" | ".directory") || name.starts_with("._") || name.starts_with(".fuse_hidden")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name.len() <= 255 && !name.contains(['/', '\0']) && !remote::is_part_file(name)
}

fn cache_path(data_dir: &Path, id: NodeId, gen: u32) -> PathBuf {
    data_dir.join(format!("{:02x}", id & 0xff)).join(format!("{id}-{gen}"))
}

impl Vfs {
    /// Opens (or creates) the local state and starts the background workers.
    /// Must be called from inside a Tokio runtime.
    pub async fn open(opts: VfsOptions, remote: Arc<dyn Remote>) -> anyhow::Result<Arc<Self>> {
        let data_dir = opts.state_dir.join("data");
        for i in 0..256 {
            std::fs::create_dir_all(data_dir.join(format!("{i:02x}"))).with_context(|| format!("cannot create the cache folder {}", data_dir.display()))?;
        }
        let store = Arc::new(Store::open(&opts.state_dir.join("index.redb"))?);
        let (records, next_id) = store.load()?;
        let mut st = State { nodes: HashMap::new(), next_id: next_id.max(2), cache_bytes: 0, jobs: HashMap::new(), uploading: 0, stale: HashSet::new(), gone: Vec::new() };
        Self::rebuild(&mut st, records, &data_dir);
        let vfs = Arc::new(Vfs {
            atomic_upload: AtomicBool::new(opts.atomic_upload),
            opts,
            remote,
            store,
            data_dir,
            state: Mutex::new(st),
            online: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            stop_notify: Notify::new(),
            flush_wake: Notify::new(),
            upload_wake: Notify::new(),
            pin_wake: Notify::new(),
            evict_wake: Notify::new(),
            quota: Mutex::new(None),
            part_seq: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            durable_at: AtomicU64::new(0),
            tasks: Mutex::new(Vec::new()),
            activity: Activity::default(),
        });
        vfs.flush(true).await?;
        let mut tasks = Vec::new();
        for worker in 0..5 {
            let me = vfs.clone();
            tasks.push(tokio::spawn(async move {
                match worker {
                    0 => me.flusher().await,
                    1 => me.uploader().await,
                    2 => me.pinner().await,
                    3 => me.prober().await,
                    _ => me.evictor().await,
                }
            }));
        }
        *vfs.tasks.lock().unwrap() = tasks;
        Ok(vfs)
    }

    /// Rebuilds the tree from the index and reconciles it with the cache files
    /// that actually survived on disk.
    fn rebuild(st: &mut State, records: Vec<Record>, data_dir: &Path) {
        for r in records {
            st.next_id = st.next_id.max(r.id + 1);
            st.nodes.insert(r.id, Node::from_record(r));
        }
        let root = st.nodes.entry(ROOT).or_insert_with(|| Node::new(ROOT, ROOT, "", Kind::Dir));
        root.kind = Kind::Dir;
        root.remote.get_or_insert_with(Version::default);
        // Link children to parents, then drop whatever is not reachable from the root.
        let mut ids: Vec<NodeId> = st.nodes.keys().copied().filter(|id| *id != ROOT).collect();
        ids.sort_unstable();
        for id in ids {
            let (parent, name) = (st.nodes[&id].parent, st.nodes[&id].name.clone());
            if let Some(p) = st.nodes.get_mut(&parent).filter(|p| p.kind == Kind::Dir) {
                p.children.entry(name).or_insert(id);
            }
        }
        let reachable: HashSet<NodeId> = st.subtree(ROOT).into_iter().collect();
        let orphans: Vec<NodeId> = st.nodes.keys().copied().filter(|id| !reachable.contains(id)).collect();
        for id in orphans {
            st.nodes.remove(&id);
            st.gone.push(id);
        }
        // Reconcile with the cache files.
        let mut lost = Vec::new();
        let mut keep_files = HashSet::new();
        for n in st.nodes.values_mut().filter(|n| n.kind == Kind::File) {
            let path = cache_path(data_dir, n.id, n.cache_gen);
            let on_disk = std::fs::metadata(&path).map(|m| m.len()).ok();
            if n.dirty {
                match on_disk {
                    // The file on disk is the truth for local edits.
                    Some(len) => {
                        n.size = len;
                        n.cached = RangeSet::full(len);
                    }
                    None if n.size == 0 => n.cached.clear(),
                    None => {
                        log::error!("local changes to {:?} were lost (cache file missing)", n.name);
                        match &n.remote {
                            Some(v) => {
                                n.dirty = false;
                                n.size = v.size;
                                n.mtime = v.mtime;
                                n.cached.clear();
                            }
                            None => lost.push(n.id),
                        }
                    }
                }
            } else if n.cached.max_end() > on_disk.unwrap_or(0) {
                n.cached.clear();
            }
            if !n.cached.is_empty() {
                keep_files.insert(path);
            }
        }
        for id in lost {
            if let Some(n) = st.nodes.remove(&id) {
                if let Some(p) = st.nodes.get_mut(&n.parent) {
                    p.children.remove(&n.name);
                }
                st.gone.push(id);
            }
        }
        // Remove cache files nothing refers to any more.
        for shard in std::fs::read_dir(data_dir).into_iter().flatten().flatten() {
            for entry in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                if !keep_files.contains(&entry.path()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        st.cache_bytes = st.nodes.values().map(|n| n.cached.len()).sum();
        // Whatever still has to reach the server gets queued again.
        let now = Instant::now();
        let pending: Vec<NodeId> = st.nodes.values().filter(|n| n.id != ROOT && n.local_change() && !is_local_only_name(&n.name)).map(|n| n.id).collect();
        for id in pending {
            st.jobs.insert(id, Job { due: now, attempts: 0 });
        }
        st.stale = st.nodes.keys().copied().collect();
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    fn set_online(&self, online: bool) {
        if self.online.swap(online, Ordering::SeqCst) != online {
            if online {
                log::info!("NAS reachable again, resuming sync");
                self.upload_wake.notify_one();
                self.pin_wake.notify_one();
            } else {
                log::warn!("NAS unreachable, working offline");
            }
        }
    }

    fn note_error(&self, e: &RemoteError) {
        if e.is_offline() {
            self.set_online(false);
        }
    }

    /// Stops the workers and writes everything to the index.
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.stop_notify.notify_waiters();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        let uploads: Vec<UploadHandle> = self.st().nodes.values_mut().filter_map(|n| n.upload.take()).collect();
        for n in self.st().nodes.values() {
            n.dls.iter().for_each(|dl| dl.cancel());
        }
        for t in tasks {
            t.abort();
            let _ = t.await;
        }
        for up in uploads {
            self.cancel_upload(up).await;
        }
        if let Err(e) = self.flush(true).await {
            log::error!("could not save the index: {e}");
        }
    }

    /// Number of files/folders still waiting to be sent to the NAS.
    pub fn pending_uploads(&self) -> usize {
        self.st().jobs.len()
    }

    // ------------------------------------------------------------------
    // Persistence

    async fn flusher(self: Arc<Self>) {
        let mut last_durable = Instant::now();
        while !self.stopped() {
            tokio::select! {
                _ = self.flush_wake.notified() => tokio::time::sleep(Duration::from_millis(100)).await,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = self.stop_notify.notified() => break,
            }
            // Cheap commits as things happen, a real (fsynced) one every few seconds.
            let durable = last_durable.elapsed() >= Duration::from_secs(5);
            if durable {
                last_durable = Instant::now();
            }
            if let Err(e) = self.flush(durable).await {
                log::error!("could not save the index: {e}");
            }
        }
    }

    /// Writes pending index changes. Cache files are fsynced *before* the
    /// index claims their contents, so a crash can never make us serve bytes
    /// that did not reach the disk.
    pub async fn flush(&self, durable: bool) -> anyhow::Result<()> {
        let (records, gone, next_id, to_sync) = {
            let mut st = self.st();
            let caught_up = self.durable_at.load(Ordering::SeqCst) == self.commits.load(Ordering::SeqCst);
            if st.stale.is_empty() && st.gone.is_empty() && (!durable || caught_up) {
                return Ok(());
            }
            let ids: Vec<NodeId> = st.stale.drain().collect();
            let mut records = Vec::with_capacity(ids.len());
            let mut to_sync = Vec::new();
            for id in ids {
                let Some(n) = st.nodes.get_mut(&id) else { continue };
                if n.unlinked {
                    continue;
                }
                if std::mem::take(&mut n.unsynced) {
                    to_sync.push(cache_path(&self.data_dir, n.id, n.cache_gen));
                }
                records.push(n.record());
            }
            (records, std::mem::take(&mut st.gone), st.next_id, to_sync)
        };
        let store = self.store.clone();
        let seen = self.commits.load(Ordering::SeqCst);
        tokio::task::spawn_blocking(move || {
            for path in to_sync {
                if let Ok(f) = File::open(&path) {
                    let _ = f.sync_data();
                }
            }
            store.commit(&records, &gone, next_id, durable)
        })
        .await??;
        // Commits without fsync only become permanent with the next fsynced
        // one, so remember whether one is still owed (e.g. at shutdown).
        if durable {
            self.durable_at.store(seen, Ordering::SeqCst);
        } else {
            self.commits.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Cache files

    fn cache_file(&self, n: &mut Node) -> std::io::Result<Arc<File>> {
        if let Some(f) = &n.file {
            return Ok(f.clone());
        }
        let path = cache_path(&self.data_dir, n.id, n.cache_gen);
        let f = Arc::new(File::options().read(true).write(true).create(true).truncate(false).open(path)?);
        n.file = Some(f.clone());
        Ok(f)
    }

    /// Throws away the cached content of a file and starts a new cache
    /// generation (the old file is never modified, only unlinked).
    fn drop_cache(&self, st: &mut State, id: NodeId) -> u64 {
        let Some(n) = st.nodes.get_mut(&id) else { return 0 };
        let freed = n.cached.len();
        let _ = std::fs::remove_file(cache_path(&self.data_dir, id, n.cache_gen));
        n.cache_gen = n.cache_gen.wrapping_add(1);
        n.cached.clear();
        n.file = None;
        n.unsynced = false;
        for dl in n.dls.drain(..) {
            dl.cancel();
        }
        if let Some(p) = &n.progress {
            p.send_modify(|v| *v += 1);
        }
        st.cache_bytes = st.cache_bytes.saturating_sub(freed);
        st.touch(id);
        freed
    }

    /// Removes a node and everything below it from the tree and the cache.
    /// Nodes that are still open somewhere linger, unlinked, until released.
    fn drop_subtree(&self, st: &mut State, id: NodeId) {
        if id == ROOT {
            return;
        }
        if let Some(n) = st.nodes.get(&id) {
            let (parent, name) = (n.parent, n.name.clone());
            if let Some(p) = st.nodes.get_mut(&parent) {
                if p.children.get(&name) == Some(&id) {
                    p.children.remove(&name);
                }
            }
        }
        for nid in st.subtree(id).into_iter().rev() {
            st.jobs.remove(&nid);
            st.stale.remove(&nid);
            st.gone.push(nid);
            let Some(n) = st.nodes.get_mut(&nid) else { continue };
            n.children.clear();
            if n.opens > 0 {
                n.unlinked = true;
            } else {
                self.drop_cache(st, nid);
                st.stale.remove(&nid);
                st.nodes.remove(&nid);
            }
        }
        self.flush_wake.notify_one();
    }

    fn schedule(&self, st: &mut State, id: NodeId, delay: Duration) {
        let Some(n) = st.nodes.get(&id) else { return };
        if n.unlinked || is_local_only_name(&n.name) {
            return;
        }
        st.jobs.insert(id, Job { due: Instant::now() + delay, attempts: 0 });
        self.upload_wake.notify_one();
    }

    // ------------------------------------------------------------------
    // Namespace: lookups and listings

    pub fn getattr(&self, id: NodeId) -> Result<Attr> {
        Ok(self.st().get(id)?.attr())
    }

    /// Like [`Vfs::getattr`], but first makes sure the listing `id` came from
    /// is no older than the TTL. Linux repeats `lookup` when a name expires,
    /// which already does this; macFUSE keeps names cached and only asks for
    /// the attributes again, so without this a file changed on the NAS would
    /// keep its old size and contents on macOS.
    pub async fn getattr_fresh(self: &Arc<Self>, id: NodeId) -> Result<Attr> {
        let parent = {
            let st = self.st();
            let n = st.get(id)?;
            (id != ROOT && !n.unlinked).then_some(n.parent)
        };
        if let Some(parent) = parent {
            // A failed refresh just means we answer from what we already know.
            let _ = self.ensure_listed(parent).await;
        }
        self.getattr(id)
    }

    pub async fn lookup(self: &Arc<Self>, parent: NodeId, name: &str) -> Result<Attr> {
        self.ensure_listed(parent).await?;
        let st = self.st();
        let id = st.child(parent, name)?;
        Ok(st.get(id)?.attr())
    }

    pub async fn readdir(self: &Arc<Self>, dir: NodeId) -> Result<Vec<(String, Attr)>> {
        self.ensure_listed(dir).await?;
        let st = self.st();
        Ok(st.dir(dir)?.children.iter().filter_map(|(name, id)| Some((name.clone(), st.nodes.get(id)?.attr()))).collect())
    }

    /// The node at `path` (relative, `/`-separated) if it is already known.
    pub fn resolve(&self, path: &str) -> Option<NodeId> {
        let st = self.st();
        let mut cur = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = st.child(cur, part).ok()?;
        }
        Some(cur)
    }

    /// Makes sure we have a listing for `dir`, refreshing it when it is older
    /// than the configured TTL. A stale listing is still served when the NAS
    /// is slow or unreachable.
    async fn ensure_listed(self: &Arc<Self>, dir: NodeId) -> Result<()> {
        let listed = {
            let st = self.st();
            let n = st.dir(dir)?;
            if n.remote.is_none() || n.listed_at.is_some_and(|t| t.elapsed() < self.opts.dir_ttl) {
                return Ok(());
            }
            n.listed
        };
        if !listed {
            return self.refresh_dir(dir).await;
        }
        if !self.is_online() {
            return Ok(());
        }
        let me = self.clone();
        let task = tokio::spawn(async move { me.refresh_dir(dir).await });
        match tokio::time::timeout(REFRESH_WAIT, task).await {
            Ok(Ok(Err(VfsError::NotFound))) => Err(VfsError::NotFound),
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------------
    // Files: open, read, write

    pub fn open_file(&self, id: NodeId, write: bool) -> Result<()> {
        let mut st = self.st();
        let n = st.file_mut(id)?;
        n.opens += 1;
        if write {
            n.writers += 1;
        }
        Ok(())
    }

    pub fn release(&self, id: NodeId, write: bool) {
        let mut st = self.st();
        let Some(n) = st.nodes.get_mut(&id) else { return };
        n.opens = n.opens.saturating_sub(1);
        if write {
            n.writers = n.writers.saturating_sub(1);
        }
        if n.opens == 0 {
            if n.dls.is_empty() {
                n.file = None;
            }
            if n.unlinked {
                self.drop_cache(&mut st, id);
                st.stale.remove(&id);
                st.nodes.remove(&id);
                return;
            }
        }
        if n.dirty && n.writers == 0 && n.upload.is_none() {
            self.schedule(&mut st, id, self.opts.upload_delay);
        }
        st.touch(id);
        self.flush_wake.notify_one();
    }

    pub async fn read(self: &Arc<Self>, id: NodeId, offset: u64, len: u32) -> Result<Vec<u8>> {
        loop {
            let (dl, mut rx) = {
                let mut st = self.st();
                let n = st.file_mut(id)?;
                let end = n.size.min(offset.saturating_add(len as u64));
                if offset >= end {
                    return Ok(Vec::new());
                }
                n.atime = unix_now();
                match n.cached.first_gap(offset, end) {
                    None => {
                        // Keep any running stream ahead of this reader.
                        n.dls.iter().for_each(|dl| dl.touch(end));
                        let file = self.cache_file(n)?;
                        drop(st);
                        let mut buf = vec![0u8; (end - offset) as usize];
                        file.read_exact_at(&mut buf, offset)?;
                        return Ok(buf);
                    }
                    Some(gap) => self.demand(&mut st, id, gap, end, false)?,
                }
            };
            self.wait_progress(&dl, &mut rx).await?;
        }
    }

    pub async fn write(self: &Arc<Self>, id: NodeId, offset: u64, data: &[u8]) -> Result<u32> {
        loop {
            {
                let mut st = self.st();
                let n = st.file_mut(id)?;
                if n.complete() && n.dirty {
                    let file = self.cache_file(n)?;
                    // Done under the lock so it cannot interleave with a
                    // download or an invalidation of the same file.
                    file.write_all_at(data, offset)?;
                    let old = n.size;
                    n.size = old.max(offset + data.len() as u64);
                    n.cached = RangeSet::full(n.size);
                    n.local_gen += 1;
                    n.mtime = unix_now();
                    n.unsynced = true;
                    let grown = n.size - old;
                    st.cache_bytes += grown;
                    st.touch(id);
                    return Ok(data.len() as u32);
                }
            }
            self.prepare_write(id).await?;
        }
    }

    /// Changes the size of a file (`truncate`).
    pub async fn truncate(self: &Arc<Self>, id: NodeId, size: u64) -> Result<Attr> {
        loop {
            {
                let mut st = self.st();
                let n = st.file_mut(id)?;
                if n.size == size {
                    return Ok(n.attr());
                }
                if size == 0 {
                    // No need to download anything first. The previous cache
                    // file is left untouched (new generation), so the index
                    // stays truthful even if we crash right now.
                    self.drop_cache(&mut st, id);
                    let n = st.file_mut(id)?;
                    n.size = 0;
                    n.dirty = true;
                    n.local_gen += 1;
                    n.mtime = unix_now();
                    let attr = n.attr();
                    self.flush_wake.notify_one();
                    return Ok(attr);
                }
                if n.complete() && n.dirty {
                    self.cache_file(n)?.set_len(size)?;
                    let old = n.cached.len();
                    n.size = size;
                    n.cached = RangeSet::full(size);
                    n.local_gen += 1;
                    n.mtime = unix_now();
                    n.unsynced = true;
                    let attr = n.attr();
                    st.cache_bytes = st.cache_bytes.saturating_sub(old) + size;
                    st.touch(id);
                    return Ok(attr);
                }
            }
            self.prepare_write(id).await?;
        }
    }

    /// Gets a file ready for in-place modification: all of it local, and the
    /// "modified" flag safely on disk before the first byte changes.
    async fn prepare_write(self: &Arc<Self>, id: NodeId) -> Result<()> {
        let complete = self.st().file(id)?.complete();
        if !complete {
            return self.ensure_full(id).await;
        }
        {
            let mut st = self.st();
            let n = st.file_mut(id)?;
            if n.dirty {
                return Ok(());
            }
            n.dirty = true;
            n.local_gen += 1;
            st.touch(id);
        }
        self.flush(true).await.map_err(|e| VfsError::Io(format!("local index: {e}")))
    }

    pub fn set_mtime(&self, id: NodeId, mtime: i64) -> Result<Attr> {
        let mut st = self.st();
        let n = st.get_mut(id)?;
        n.mtime = mtime;
        let attr = n.attr();
        st.touch(id);
        Ok(attr)
    }

    pub async fn fsync(&self, id: NodeId) -> Result<()> {
        self.st().get(id)?;
        self.flush(true).await.map_err(|e| VfsError::Io(format!("local index: {e}")))
    }

    // ------------------------------------------------------------------
    // Namespace: changes

    pub async fn create(self: &Arc<Self>, parent: NodeId, name: &str) -> Result<Attr> {
        if !valid_name(name) {
            return Err(VfsError::Invalid);
        }
        let mut st = self.st();
        if st.dir(parent)?.children.contains_key(name) {
            return Err(VfsError::Exists);
        }
        let id = st.alloc_id();
        let mut n = Node::new(id, parent, name, Kind::File);
        n.dirty = true;
        n.local_gen = 1;
        n.opens = 1;
        n.writers = 1;
        n.atime = unix_now();
        let attr = n.attr();
        st.insert(n);
        st.bump(parent);
        self.flush_wake.notify_one();
        Ok(attr)
    }

    pub async fn mkdir(self: &Arc<Self>, parent: NodeId, name: &str) -> Result<Attr> {
        if !valid_name(name) {
            return Err(VfsError::Invalid);
        }
        let mut st = self.st();
        if st.dir(parent)?.children.contains_key(name) {
            return Err(VfsError::Exists);
        }
        let id = st.alloc_id();
        let mut n = Node::new(id, parent, name, Kind::Dir);
        n.listed = true;
        let attr = n.attr();
        st.insert(n);
        st.bump(parent);
        self.schedule(&mut st, id, Duration::ZERO);
        self.flush_wake.notify_one();
        Ok(attr)
    }

    pub async fn unlink(self: &Arc<Self>, parent: NodeId, name: &str) -> Result<()> {
        let (id, path, on_server, upload) = {
            let mut st = self.st();
            let id = st.child(parent, name)?;
            let path = st.path(id).ok_or(VfsError::NotFound)?;
            let n = st.file_mut(id)?;
            (id, path, n.remote.is_some(), n.upload.take())
        };
        let mut leftovers = false;
        if let Some(up) = upload {
            leftovers = self.cancel_upload(up).await.is_some();
        }
        if on_server || leftovers {
            if let Err(e) = self.remote.delete(&path, false).await {
                self.note_error(&e);
                if on_server {
                    return Err(e.into());
                }
            }
        }
        let mut st = self.st();
        if st.child(parent, name) == Ok(id) {
            self.drop_subtree(&mut st, id);
            st.bump(parent);
        }
        Ok(())
    }

    pub async fn rmdir(self: &Arc<Self>, parent: NodeId, name: &str) -> Result<()> {
        let (id, path, on_server) = {
            let st = self.st();
            let id = st.child(parent, name)?;
            let n = st.dir(id)?;
            if !n.children.is_empty() {
                return Err(VfsError::NotEmpty);
            }
            (id, st.path(id).ok_or(VfsError::NotFound)?, n.remote.is_some())
        };
        if on_server {
            // Deleting a folder on the server is recursive: look again right
            // before doing it, in case someone else just put files there.
            if !self.remote_dir_empty(id, &path).await? {
                return Err(VfsError::NotEmpty);
            }
            if let Err(e) = self.remote.delete(&path, true).await {
                self.note_error(&e);
                return Err(e.into());
            }
        }
        let mut st = self.st();
        if st.child(parent, name) == Ok(id) {
            if !st.dir(id)?.children.is_empty() {
                return Err(VfsError::NotEmpty);
            }
            self.drop_subtree(&mut st, id);
            st.bump(parent);
        }
        Ok(())
    }

    pub async fn rename(self: &Arc<Self>, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str, no_replace: bool) -> Result<()> {
        if !valid_name(new_name) {
            return Err(VfsError::Invalid);
        }
        // 1. Look at both ends and take over any upload in flight.
        let (src, dst, from, to, is_dir, src_remote, dst_remote, uploads) = {
            let mut st = self.st();
            let src = st.child(parent, name)?;
            let dst = st.dir(new_parent)?.children.get(new_name).copied();
            if dst == Some(src) {
                return Ok(());
            }
            let is_dir = st.get(src)?.kind == Kind::Dir;
            if let Some(d) = dst {
                if no_replace {
                    return Err(VfsError::Exists);
                }
                let dn = st.get(d)?;
                match (is_dir, dn.kind == Kind::Dir) {
                    (true, false) => return Err(VfsError::NotDir),
                    (false, true) => return Err(VfsError::IsDir),
                    (true, true) if !dn.children.is_empty() => return Err(VfsError::NotEmpty),
                    _ => {}
                }
            }
            if is_dir && st.subtree(src).contains(&new_parent) {
                return Err(VfsError::Invalid);
            }
            let from = st.path(src).ok_or(VfsError::NotFound)?;
            let to = remote::join(&st.path(new_parent).ok_or(VfsError::NotFound)?, new_name);
            let mut ids = st.subtree(src);
            ids.extend(dst);
            let uploads: Vec<(NodeId, UploadHandle)> = ids.into_iter().filter_map(|i| Some((i, st.nodes.get_mut(&i)?.upload.take()?))).collect();
            let src_remote = st.get(src)?.remote.is_some();
            let dst_remote = dst.and_then(|d| st.nodes.get(&d)).is_some_and(|d| d.remote.is_some());
            (src, dst, from, to, is_dir, src_remote, dst_remote, uploads)
        };
        for (id, up) in uploads {
            if let Some(partial) = self.cancel_upload(up).await {
                self.after_cancelled_upload(id, &partial).await;
            }
        }
        if src_remote {
            // The destination folder may itself still be waiting to be created.
            self.ensure_remote_dir(new_parent).await?;
            if let Some(d) = dst.filter(|_| is_dir && dst_remote) {
                if !self.remote_dir_empty(d, &to).await? {
                    return Err(VfsError::NotEmpty);
                }
            }
        }
        // 2. Tell the server, when the thing being moved exists there.
        let mut src_remote = src_remote;
        let mut moved_version = None;
        if src_remote {
            match self.remote.rename(&from, &to, is_dir, dst_remote).await {
                Ok(()) => {
                    if !is_dir {
                        // Some servers change the ETag on a move; adopt the new one.
                        moved_version = self.remote.stat(&to).await.ok().flatten().map(|e| e.version);
                    }
                }
                // Gone on the server: if we hold changes, carry on as a local-only item.
                Err(RemoteError::NotFound) if self.local_changes(src) => src_remote = false,
                Err(e) => {
                    self.note_error(&e);
                    let mut st = self.st();
                    for dir in [parent, new_parent] {
                        if let Some(d) = st.nodes.get_mut(&dir) {
                            d.listed_at = None;
                        }
                    }
                    return Err(match e {
                        RemoteError::Precondition(_) => VfsError::Exists,
                        e => e.into(),
                    });
                }
            }
        }
        // 3. Apply locally.
        let mut st = self.st();
        if st.child(parent, name) != Ok(src) {
            return Err(VfsError::NotFound);
        }
        let mut inherited = None;
        if let Some(d) = dst.filter(|d| st.child(new_parent, new_name) == Ok(*d)) {
            inherited = st.get(d)?.remote.clone();
            self.drop_subtree(&mut st, d);
        }
        st.dir_mut(parent)?.children.remove(name);
        st.dir_mut(new_parent)?.children.insert(new_name.to_string(), src);
        let n = st.get_mut(src)?;
        n.parent = new_parent;
        n.name = new_name.to_string();
        if src_remote {
            if let Some(v) = moved_version {
                n.remote = Some(v);
            }
        } else {
            // A local-only item replacing something on the server keeps that
            // version as its base, so the upload overwrites it knowingly.
            n.remote = inherited;
            if n.kind == Kind::File {
                n.dirty = true;
            }
        }
        st.bump(parent);
        st.bump(new_parent);
        for id in st.subtree(src) {
            st.touch(id);
            if st.nodes.get(&id).is_some_and(|n| n.local_change() && n.writers == 0) {
                self.schedule(&mut st, id, self.opts.upload_delay);
            }
        }
        self.flush_wake.notify_one();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Pinning, eviction, status

    /// "Keep offline": the file (or everything below the folder) is downloaded
    /// and never evicted.
    pub fn pin(&self, id: NodeId, on: bool) -> Result<()> {
        let mut st = self.st();
        st.get_mut(id)?.pinned = on;
        st.touch(id);
        self.flush_wake.notify_one();
        if on {
            self.pin_wake.notify_one();
        }
        Ok(())
    }

    /// "Free up space": drops the local copy of a file, or of everything below
    /// a folder. Modified and open files are skipped. Returns the bytes freed.
    pub fn evict(&self, id: NodeId) -> Result<u64> {
        let mut st = self.st();
        st.get(id)?;
        if st.pin_of(id) == Pin::Inherited {
            return Err(VfsError::Busy("kept offline by a parent folder (unpin that folder first)".into()));
        }
        let mut freed = 0;
        for nid in st.subtree(id) {
            let Some(n) = st.nodes.get_mut(&nid) else { continue };
            if n.pinned {
                n.pinned = false;
                st.touch(nid);
            }
            let n = &st.nodes[&nid];
            if n.kind == Kind::File && !n.dirty && n.opens == 0 && n.upload.is_none() && !n.cached.is_empty() {
                freed += self.drop_cache(&mut st, nid);
            }
        }
        self.flush_wake.notify_one();
        Ok(freed)
    }

    pub fn status(&self, id: NodeId) -> Result<NodeStatus> {
        let st = self.st();
        let n = st.get(id)?;
        let pinned = st.pin_of(id);
        if n.kind == Kind::File {
            let state = if is_local_only_name(&n.name) {
                "local"
            } else if n.upload.is_some() {
                "uploading"
            } else if n.local_change() {
                "modified"
            } else if n.complete() {
                "cached"
            } else if n.cached.is_empty() {
                "online"
            } else {
                "partial"
            };
            let pending = (n.local_change() && !is_local_only_name(&n.name)) as u64;
            return Ok(NodeStatus { dir: false, state: state.into(), pinned, size: n.size, cached: n.cached.len(), files: 1, files_cached: n.complete() as u64, pending, error: n.error.clone() });
        }
        let mut s = NodeStatus { dir: true, state: "folder".into(), pinned, size: 0, cached: 0, files: 0, files_cached: 0, pending: 0, error: n.error.clone() };
        for nid in st.subtree(id).into_iter().skip(1) {
            let Some(c) = st.nodes.get(&nid) else { continue };
            if c.kind == Kind::File {
                s.size += c.size;
                s.cached += c.cached.len();
                s.files += 1;
                s.files_cached += c.complete() as u64;
            }
            s.pending += (c.local_change() && !is_local_only_name(&c.name)) as u64;
            if s.error.is_none() {
                s.error = c.error.clone();
            }
        }
        Ok(s)
    }

    pub fn info(&self) -> Info {
        let st = self.st();
        Info {
            online: self.is_online(),
            pending_uploads: st.jobs.len() as u64,
            uploading: st.uploading as u64,
            cache_bytes: st.cache_bytes,
            cache_max: self.opts.cache_max,
            nodes: st.nodes.len() as u64,
            errors: st.nodes.values().filter_map(|n| n.error.as_ref().map(|e| format!("{}: {e}", st.path(n.id).unwrap_or_default()))).take(10).collect(),
        }
    }

    /// Recent uploads and downloads, most recent first.
    pub fn transfers(&self) -> Vec<Transfer> {
        self.activity.list()
    }

    /// (total, free) bytes to report for the volume.
    pub fn space(&self) -> (u64, u64) {
        match *self.quota.lock().unwrap() {
            Some(q) => (q.used.saturating_add(q.available), q.available),
            // The server does not say: pretend there is plenty so tools do not refuse to copy.
            None => (1 << 50, 1 << 49),
        }
    }

    /// Forces a fresh listing of a folder now.
    pub async fn refresh(self: &Arc<Self>, id: NodeId) -> Result<()> {
        if let Some(n) = self.st().nodes.get_mut(&id) {
            n.listed_at = None;
        }
        self.refresh_dir(id).await
    }
}
