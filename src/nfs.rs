//! NFS front-end (macOS): serves the VFS over NFSv3 on 127.0.0.1 and mounts it
//! with the NFS client that comes with macOS. Nothing has to be installed (no
//! macFUSE, no kernel extension) and an ordinary user may mount it.
//!
//! NFS has no open/close, so files are "opened" in the VFS when they are first
//! used and "closed" after a short pause (see [`Handles`]). Closing a changed
//! file is what schedules its upload, as with FUSE. The `._*` files macOS
//! uses for extended attributes stay in memory (see [`Litter`]).

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nasyncloud::vfs::{Attr, Kind, NodeId, Vfs, VfsError, ROOT};
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_mtime, set_size3, specdata3};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tokio::sync::watch;

/// A file nobody has touched for this long counts as closed.
const IDLE_READ: Duration = Duration::from_secs(5);
const IDLE_WRITE: Duration = Duration::from_secs(1);
/// Past this many open files, idle ones are closed at once (oldest first):
/// each holds a cache file descriptor, and a big copy touches thousands.
const MAX_OPEN: usize = 128;

/// Files opened implicitly by NFS requests, keyed by (node, for writing).
#[derive(Default)]
struct Handles {
    open: Mutex<HashMap<(NodeId, bool), Handle>>,
}

struct Handle {
    last: Instant,
    /// Requests still running on this file: never close it under them.
    busy: u32,
}

/// Keeps a file open in the VFS while a request uses it.
struct Use<'a> {
    handles: &'a Handles,
    key: (NodeId, bool),
}

impl Drop for Use<'_> {
    fn drop(&mut self) {
        if let Some(h) = self.handles.open.lock().unwrap().get_mut(&self.key) {
            h.busy -= 1;
            h.last = Instant::now();
        }
    }
}

impl Handles {
    /// Marks `id` as in use, opening it in the VFS if it was not open yet.
    /// `already_open` is for files the VFS opened itself (`create`).
    fn use_file<'a>(&'a self, vfs: &Vfs, id: NodeId, write: bool, already_open: bool) -> Result<Use<'a>, VfsError> {
        let mut open = self.open.lock().unwrap();
        match open.get_mut(&(id, write)) {
            Some(h) => {
                h.busy += 1;
                if already_open {
                    // We hold one open already: give back the extra one.
                    vfs.release(id, write);
                }
            }
            None => {
                if open.len() >= MAX_OPEN {
                    Self::close_oldest(&mut open, vfs);
                }
                if !already_open {
                    vfs.open_file(id, write)?;
                }
                open.insert((id, write), Handle { last: Instant::now(), busy: 1 });
            }
        }
        Ok(Use { handles: self, key: (id, write) })
    }

    /// Closes idle files, oldest first, until there is room under `MAX_OPEN`.
    fn close_oldest(open: &mut HashMap<(NodeId, bool), Handle>, vfs: &Vfs) {
        let mut idle: Vec<_> = open.iter().filter(|(_, h)| h.busy == 0).map(|(&k, h)| (h.last, k)).collect();
        idle.sort_unstable_by_key(|&(last, _)| last);
        let excess = (open.len() + 1).saturating_sub(MAX_OPEN);
        for (_, key) in idle.into_iter().take(excess.max(MAX_OPEN / 4)) {
            open.remove(&key);
            vfs.release(key.0, key.1);
        }
    }

    /// Closes the files that have been idle long enough (or all of them).
    fn close_idle(&self, vfs: &Vfs, all: bool) {
        let mut open = self.open.lock().unwrap();
        open.retain(|&(id, write), h| {
            let idle = if write { IDLE_WRITE } else { IDLE_READ };
            if all || (h.busy == 0 && h.last.elapsed() >= idle) {
                vfs.release(id, write);
                false
            } else {
                true
            }
        });
    }
}

fn status(e: &VfsError) -> nfsstat3 {
    match e {
        VfsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        VfsError::Exists => nfsstat3::NFS3ERR_EXIST,
        VfsError::NotDir => nfsstat3::NFS3ERR_NOTDIR,
        VfsError::IsDir => nfsstat3::NFS3ERR_ISDIR,
        VfsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        VfsError::Invalid => nfsstat3::NFS3ERR_INVAL,
        VfsError::Denied => nfsstat3::NFS3ERR_ACCES,
        // NFS has no "host is down": EIO it is.
        VfsError::Offline(msg) => {
            log::debug!("offline: {msg}");
            nfsstat3::NFS3ERR_IO
        }
        VfsError::Busy(msg) | VfsError::Io(msg) => {
            log::warn!("I/O error: {msg}");
            nfsstat3::NFS3ERR_IO
        }
    }
}

/// macOS keeps extended attributes and Finder data in `._name` and
/// `.DS_Store` files on file systems like this one.
fn finder_litter(name: &str) -> bool {
    name.starts_with("._") || name == ".DS_Store"
}

/// A name from the client, or `None` when it is not valid UTF-8.
fn name(n: &filename3) -> Option<&str> {
    std::str::from_utf8(&n.0).ok()
}

const LITTER_IDS: u64 = 1 << 62;
const LITTER_MAX: u64 = 16 << 20;

/// The `._name` and `.DS_Store` files, kept in memory and never sent to the
/// NAS: extended attributes (quarantine, tags, …) work while nasyncloud runs
/// (`ditto`, `cp -p` and apps that save them do not fail), and the NAS stays
/// free of them. They are not listed and are forgotten on restart.
#[derive(Default)]
struct Litter {
    next: u64,
    ids: HashMap<(NodeId, String), u64>,
    files: HashMap<u64, Blob>,
}

struct Blob {
    data: Vec<u8>,
    mtime: i64,
}

impl Litter {
    fn create(&mut self, dir: NodeId, name: &str, exclusive: bool) -> Result<u64, nfsstat3> {
        if let Some(&id) = self.ids.get(&(dir, name.to_string())) {
            return if exclusive { Err(nfsstat3::NFS3ERR_EXIST) } else { Ok(id) };
        }
        self.next += 1;
        let id = LITTER_IDS + self.next;
        self.ids.insert((dir, name.to_string()), id);
        self.files.insert(id, Blob { data: Vec::new(), mtime: now() });
        Ok(id)
    }

    fn blob(&mut self, id: u64) -> Result<&mut Blob, nfsstat3> {
        self.files.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn resize(&mut self, id: u64, size: u64) -> Result<(), nfsstat3> {
        if size > LITTER_MAX {
            return Err(nfsstat3::NFS3ERR_FBIG);
        }
        self.blob(id)?.data.resize(size as usize, 0);
        Ok(())
    }
}

fn time(secs: i64) -> nfstime3 {
    nfstime3 { seconds: secs.clamp(0, u32::MAX as i64) as u32, nseconds: 0 }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

struct NasNfs {
    vfs: Arc<Vfs>,
    handles: Arc<Handles>,
    owner: (u32, u32),
    litter: Mutex<Litter>,
}

impl NasNfs {
    fn fattr(&self, a: &Attr) -> fattr3 {
        let dir = a.kind == Kind::Dir;
        let (ftype, mode) = match a.kind {
            Kind::Dir => (ftype3::NF3DIR, 0o755),
            Kind::File => (ftype3::NF3REG, 0o644),
            Kind::Symlink => (ftype3::NF3LNK, 0o755),
        };
        // The nanoseconds carry the change counter: the NAS gives whole
        // seconds, and the client only drops its cached listing or contents
        // when the timestamp moves.
        let t = nfstime3 { nseconds: (a.change % 1_000_000_000) as u32, ..time(a.mtime) };
        fattr3 {
            ftype,
            mode,
            nlink: if dir { 2 } else { 1 },
            uid: self.owner.0,
            gid: self.owner.1,
            size: a.size,
            // What is really on this disk (the macOS client ignores it and
            // derives st_blocks from the size, as macFUSE did).
            used: a.local_bytes,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: a.id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

    fn attr_of(&self, id: NodeId) -> Result<fattr3, nfsstat3> {
        if id >= LITTER_IDS {
            let mut litter = self.litter.lock().unwrap();
            let b = litter.blob(id)?;
            let size = b.data.len() as u64;
            return Ok(self.fattr(&Attr { id, kind: Kind::File, size, mtime: b.mtime, local_bytes: size, change: 0 }));
        }
        self.vfs.getattr(id).map(|a| self.fattr(&a)).map_err(|e| status(&e))
    }

    async fn create_file(&self, dir: NodeId, file: &filename3, exclusive: bool) -> Result<NodeId, nfsstat3> {
        let Some(file) = name(file) else { return Err(nfsstat3::NFS3ERR_INVAL) };
        if finder_litter(file) {
            return self.litter.lock().unwrap().create(dir, file, exclusive);
        }
        match self.vfs.create(dir, file).await {
            Ok(a) => {
                // The VFS opened it for writing: close it like any other file.
                drop(self.handles.use_file(&self.vfs, a.id, true, true).map_err(|e| status(&e))?);
                Ok(a.id)
            }
            Err(VfsError::Exists) if !exclusive => match self.vfs.lookup(dir, file).await {
                Ok(a) if a.kind == Kind::File => Ok(a.id),
                Ok(_) => Err(nfsstat3::NFS3ERR_EXIST),
                Err(e) => Err(status(&e)),
            },
            Err(e) => Err(status(&e)),
        }
    }
}

#[async_trait]
impl NFSFileSystem for NasNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        match name(filename) {
            None => Err(nfsstat3::NFS3ERR_NOENT),
            Some(n) if finder_litter(n) => self.litter.lock().unwrap().ids.get(&(dirid, n.to_string())).copied().ok_or(nfsstat3::NFS3ERR_NOENT),
            Some(".") => Ok(dirid),
            Some("..") => self.vfs.parent(dirid).map_err(|e| status(&e)),
            Some(n) => self.vfs.lookup(dirid, n).await.map(|a| a.id).map_err(|e| status(&e)),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        if id >= LITTER_IDS {
            return self.attr_of(id);
        }
        // Notices a change made on the NAS (see `getattr_fresh`).
        self.vfs.getattr_fresh(id).await.map(|a| self.fattr(&a)).map_err(|e| status(&e))
    }

    async fn setattr(&self, id: fileid3, attr: sattr3) -> Result<fattr3, nfsstat3> {
        // chmod/chown are accepted and ignored: the NAS has its own permissions.
        if id >= LITTER_IDS {
            let mut litter = self.litter.lock().unwrap();
            if let set_size3::size(size) = attr.size {
                litter.resize(id, size)?;
            }
            if let set_mtime::SET_TO_CLIENT_TIME(t) = attr.mtime {
                litter.blob(id)?.mtime = t.seconds as i64;
            }
            drop(litter);
            return self.attr_of(id);
        }
        if let set_size3::size(size) = attr.size {
            let _w = self.handles.use_file(&self.vfs, id, true, false).map_err(|e| status(&e))?;
            self.vfs.truncate(id, size).await.map_err(|e| status(&e))?;
        }
        let mtime = match attr.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(now()),
            set_mtime::SET_TO_CLIENT_TIME(t) => Some(t.seconds as i64),
        };
        if let Some(mtime) = mtime {
            self.vfs.set_mtime(id, mtime).map_err(|e| status(&e))?;
        }
        self.attr_of(id)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        if id >= LITTER_IDS {
            let mut litter = self.litter.lock().unwrap();
            let data = &litter.blob(id)?.data;
            let start = (offset as usize).min(data.len());
            let end = start.saturating_add(count as usize).min(data.len());
            return Ok((data[start..end].to_vec(), end == data.len()));
        }
        let _r = self.handles.use_file(&self.vfs, id, false, false).map_err(|e| status(&e))?;
        let data = self.vfs.read(id, offset, count).await.map_err(|e| status(&e))?;
        let size = self.vfs.getattr(id).map_err(|e| status(&e))?.size;
        let eof = offset + data.len() as u64 >= size;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        if id >= LITTER_IDS {
            let mut litter = self.litter.lock().unwrap();
            let end = offset + data.len() as u64;
            if end > litter.blob(id)?.data.len() as u64 {
                litter.resize(id, end)?;
            }
            let b = litter.blob(id)?;
            b.data[offset as usize..end as usize].copy_from_slice(data);
            b.mtime = now();
            drop(litter);
            return self.attr_of(id);
        }
        let _w = self.handles.use_file(&self.vfs, id, true, false).map_err(|e| status(&e))?;
        self.vfs.write(id, offset, data).await.map_err(|e| status(&e))?;
        self.attr_of(id)
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let id = self.create_file(dirid, filename, false).await?;
        Ok((id, self.setattr(id, attr).await?))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.create_file(dirid, filename, true).await
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let Some(dirname) = name(dirname).filter(|n| !finder_litter(n)) else { return Err(nfsstat3::NFS3ERR_ACCES) };
        let a = self.vfs.mkdir(dirid, dirname).await.map_err(|e| status(&e))?;
        Ok((a.id, self.fattr(&a)))
    }

    /// Both REMOVE and RMDIR end up here.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let Some(filename) = name(filename) else { return Err(nfsstat3::NFS3ERR_NOENT) };
        if finder_litter(filename) {
            let mut litter = self.litter.lock().unwrap();
            let id = litter.ids.remove(&(dirid, filename.to_string())).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            litter.files.remove(&id);
            return Ok(());
        }
        let a = self.vfs.lookup(dirid, filename).await.map_err(|e| status(&e))?;
        let res = match a.kind {
            Kind::Dir => self.vfs.rmdir(dirid, filename).await,
            Kind::File | Kind::Symlink => self.vfs.unlink(dirid, filename).await,
        };
        res.map_err(|e| status(&e))
    }

    async fn rename(&self, from_dirid: fileid3, from_filename: &filename3, to_dirid: fileid3, to_filename: &filename3) -> Result<(), nfsstat3> {
        let (Some(from), Some(to)) = (name(from_filename), name(to_filename)) else { return Err(nfsstat3::NFS3ERR_INVAL) };
        match (finder_litter(from), finder_litter(to)) {
            (false, false) => {}
            (true, true) => {
                let mut litter = self.litter.lock().unwrap();
                let id = litter.ids.remove(&(from_dirid, from.to_string())).ok_or(nfsstat3::NFS3ERR_NOENT)?;
                if let Some(old) = litter.ids.insert((to_dirid, to.to_string()), id) {
                    litter.files.remove(&old);
                }
                return Ok(());
            }
            // Between the NAS and memory: not something macOS does itself.
            _ => return Err(nfsstat3::NFS3ERR_ACCES),
        }
        self.vfs.rename(from_dirid, from, to_dirid, to, false).await.map_err(|e| status(&e))
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        let mut children = self.vfs.readdir(dirid).await.map_err(|e| status(&e))?;
        // Ordered by id, so paging (which resumes after the last id sent)
        // stays right when entries come and go in between.
        children.retain(|(n, a)| !finder_litter(n) && a.id > start_after);
        children.sort_by_key(|(_, a)| a.id);
        let end = children.len() <= max_entries;
        let entries = children.into_iter().take(max_entries).map(|(n, a)| DirEntry { fileid: a.id, name: n.into_bytes().into(), attr: self.fattr(&a) }).collect();
        Ok(ReadDirResult { entries, end })
    }

    /// Links stay on this computer (see `Kind::Symlink`).
    async fn symlink(&self, dirid: fileid3, linkname: &filename3, symlink: &nfspath3, _attr: &sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let Some(linkname) = name(linkname).filter(|n| !finder_litter(n)) else { return Err(nfsstat3::NFS3ERR_ACCES) };
        let Ok(target) = std::str::from_utf8(&symlink.0) else { return Err(nfsstat3::NFS3ERR_INVAL) };
        let a = self.vfs.symlink(dirid, linkname, target).map_err(|e| status(&e))?;
        Ok((a.id, self.fattr(&a)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        if id >= LITTER_IDS {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        self.vfs.readlink(id).map(|t| t.into_bytes().into()).map_err(|e| status(&e))
    }
}

// ----------------------------------------------------------------------
// Mounting

/// Mount options for `mount_nfs`. A hard mount (the default) never fails a
/// read just because the NAS is slow; `intr` lets a stuck program be killed.
/// `locallocks` makes file locks work (apps such as Office need them), `nfc`
/// sends names in the form a Linux NAS uses, `actimeo=1` matches the 1 s the
/// FUSE mount gave the kernel, `nonegnamecache` lets a file just added on
/// the NAS show up at once (FUSE never cached "not found" either).
fn options(port: u16) -> String {
    format!("vers=3,tcp,port={port},mountport={port},locallocks,intr,actimeo=1,nonegnamecache,rsize=1048576,wsize=1048576,nfc")
}

/// What the server, the mount watcher and [`Unmounter`] share.
struct Shared {
    vfs: Arc<Vfs>,
    handles: Arc<Handles>,
    server: Mutex<Option<tokio::task::JoinHandle<()>>>,
    ended: watch::Sender<bool>,
}

impl Shared {
    /// The mount is gone: close every file (which schedules their uploads)
    /// and stop serving.
    fn finish(&self) {
        self.handles.close_idle(&self.vfs, true);
        if let Some(server) = self.server.lock().unwrap().take() {
            server.abort();
        }
        self.ended.send_replace(true);
    }
}

/// Unmounts what [`mount`] mounted.
pub struct Unmounter {
    shared: Arc<Shared>,
    mountpoint: PathBuf,
}

impl Unmounter {
    pub fn unmount(&mut self) -> io::Result<()> {
        // The client sends its last writes to our server while unmounting,
        // so keep the runtime free to answer them.
        let res = tokio::task::block_in_place(|| unmount(&self.mountpoint, false));
        if res.is_ok() {
            self.shared.finish();
        }
        res
    }
}

/// Serves the VFS on a free port of 127.0.0.1 and mounts it on `mountpoint`.
/// Returns a channel that fires when the mount goes away (whoever unmounted
/// it), plus a handle to unmount it ourselves.
pub async fn mount(vfs: Arc<Vfs>, mountpoint: &Path) -> io::Result<(watch::Receiver<bool>, Unmounter)> {
    // SAFETY: plain getters without preconditions.
    let owner = unsafe { (libc::getuid(), libc::getgid()) };
    let handles = Arc::new(Handles::default());
    let fs = NasNfs { vfs: vfs.clone(), handles: handles.clone(), owner, litter: Mutex::default() };
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs).await?;
    let port = listener.get_listen_port();
    let server = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            log::warn!("the NFS server stopped: {e}");
        }
    });
    let (tx, ended) = watch::channel(false);
    let shared = Arc::new(Shared { vfs: vfs.clone(), handles, server: Mutex::new(Some(server)), ended: tx });

    let target = std::fs::canonicalize(mountpoint)?;
    let args = (options(port), target.clone());
    let out = tokio::task::spawn_blocking(move || std::process::Command::new("/sbin/mount_nfs").arg("-o").arg(&args.0).arg("127.0.0.1:/").arg(&args.1).output()).await.map_err(io::Error::other)??;
    if !out.status.success() {
        shared.finish();
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(io::Error::other(format!("mount_nfs failed: {msg}")));
    }
    log::debug!("NFS server on 127.0.0.1:{port}");

    // Closes idle files and notices when the mount goes away (e.g. ejected
    // in the Finder).
    let s = shared.clone();
    let mut done = s.ended.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = done.wait_for(|d| *d) => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
            s.handles.close_idle(&s.vfs, false);
            if !ours_at(&target) {
                log::info!("{} was unmounted", target.display());
                s.finish();
                return;
            }
        }
    });
    Ok((ended, Unmounter { shared, mountpoint: mountpoint.to_path_buf() }))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn unmount(mountpoint: &Path, force: bool) -> io::Result<()> {
    let mnt = c_path(mountpoint)?;
    // SAFETY: `mnt` is a valid C string.
    if unsafe { libc::unmount(mnt.as_ptr(), if force { libc::MNT_FORCE } else { 0 }) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether one of our NFS mounts sits on `target`. Reads the mount table
/// only: asking the mount itself could hang if its server is gone.
fn ours_at(target: &Path) -> bool {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo fills `buf` with `n` entries it owns (valid until the
    // next call on this thread); we only read them here.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return false;
    }
    let mounts = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    let text = |chars: &[libc::c_char]| -> Vec<u8> { chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect() };
    mounts.iter().any(|m| text(&m.f_mntonname) == target.as_os_str().as_bytes() && text(&m.f_fstypename) == b"nfs" && text(&m.f_mntfromname).starts_with(b"127.0.0.1:"))
}

/// Removes our mount left on `mountpoint` by a session that ended without
/// unmounting (its server is gone, so the mount could only hang). Returns
/// whether there was one.
pub fn clean_stale(mountpoint: &Path) -> io::Result<bool> {
    let Ok(target) = std::fs::canonicalize(mountpoint.parent().unwrap_or(mountpoint)).map(|p| p.join(mountpoint.file_name().unwrap_or_default())) else { return Ok(false) };
    if !ours_at(&target) {
        return Ok(false);
    }
    unmount(&target, true)?;
    Ok(true)
}
