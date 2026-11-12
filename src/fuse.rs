//! FUSE front-end (Linux; macOS uses `nfs.rs`): translates kernel requests
//! into VFS calls.
//!
//! Every request is handed to the Tokio runtime and answered from there, so a
//! slow download never blocks the rest of the file system.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, Session, TimeOrNow, WriteFlags,
};
use fuser::{MountOption, SessionUnmounter};
use nasyncloud::vfs::{Attr, Kind, Vfs, VfsError, XATTR_EVICT, XATTR_INFO, XATTR_PIN, XATTR_REFRESH, XATTR_STATUS};
use tokio::runtime::Handle;

/// How long the kernel may cache names and attributes.
const TTL: Duration = Duration::from_secs(1);
const BLOCK: u32 = 4096;
/// Flags for every file open. Linux drops its cached pages on open unless told
/// `FOPEN_KEEP_CACHE`, so a file changed on the NAS is read afresh.
const OPEN_FLAGS: FopenFlags = FopenFlags::empty();

type DirSnapshot = Arc<Vec<(u64, FileType, String)>>;

struct NasFs {
    vfs: Arc<Vfs>,
    rt: Handle,
    owner: (u32, u32),
    /// Listings captured at `opendir`, so paging through a big folder is consistent.
    dirs: Arc<Mutex<HashMap<u64, DirSnapshot>>>,
    next_fh: AtomicU64,
}

fn errno(e: &VfsError) -> Errno {
    match e {
        VfsError::NotFound => Errno::ENOENT,
        VfsError::Exists => Errno::EEXIST,
        VfsError::NotDir => Errno::ENOTDIR,
        VfsError::IsDir => Errno::EISDIR,
        VfsError::NotEmpty => Errno::ENOTEMPTY,
        VfsError::Invalid => Errno::EINVAL,
        VfsError::Offline(_) => Errno::from_i32(libc::EHOSTDOWN),
        VfsError::Busy(_) => Errno::EBUSY,
        VfsError::Denied => Errno::EACCES,
        VfsError::Io(msg) => {
            log::warn!("I/O error: {msg}");
            Errno::EIO
        }
    }
}

fn kind(k: Kind) -> FileType {
    match k {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
    }
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn fattr(a: &Attr, owner: (u32, u32)) -> FileAttr {
    let time = UNIX_EPOCH + Duration::from_secs(a.mtime.max(0) as u64);
    let dir = a.kind == Kind::Dir;
    FileAttr {
        ino: INodeNo(a.id),
        size: a.size,
        // Blocks reflect what is really on this disk: `du` shows the cache use,
        // `ls -s` shows 0 for files that are online-only.
        blocks: a.local_bytes.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind: kind(a.kind),
        perm: if dir { 0o755 } else { 0o644 },
        nlink: if dir { 2 } else { 1 },
        uid: owner.0,
        gid: owner.1,
        rdev: 0,
        blksize: 128 * 1024,
        flags: 0,
    }
}

fn utf8(name: &OsStr) -> Option<String> {
    name.to_str().map(str::to_owned)
}

impl Filesystem for NasFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = utf8(name) else { return reply.error(Errno::ENOENT) };
        let (vfs, owner) = (self.vfs.clone(), self.owner);
        self.rt.spawn(async move {
            match vfs.lookup(parent.0, &name).await {
                Ok(a) => reply.entry(&TTL, &fattr(&a, owner), Generation(0)),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let (vfs, owner) = (self.vfs.clone(), self.owner);
        self.rt.spawn(async move {
            match vfs.getattr_fresh(ino.0).await {
                Ok(a) => reply.attr(&TTL, &fattr(&a, owner)),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // chmod/chown are accepted and ignored: the NAS has its own permissions.
        let mtime = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(t) => secs(t),
            TimeOrNow::Now => secs(SystemTime::now()),
        });
        let (vfs, owner) = (self.vfs.clone(), self.owner);
        self.rt.spawn(async move {
            let res = async {
                if let Some(size) = size {
                    vfs.truncate(ino.0, size).await?;
                }
                if let Some(mtime) = mtime {
                    vfs.set_mtime(ino.0, mtime)?;
                }
                vfs.getattr(ino.0)
            }
            .await;
            match res {
                Ok(a) => reply.attr(&TTL, &fattr(&a, owner)),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(name) = utf8(name) else { return reply.error(Errno::EINVAL) };
        let (vfs, owner) = (self.vfs.clone(), self.owner);
        self.rt.spawn(async move {
            match vfs.mkdir(parent.0, &name).await {
                Ok(a) => reply.entry(&TTL, &fattr(&a, owner), Generation(0)),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = utf8(name) else { return reply.error(Errno::ENOENT) };
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.unlink(parent.0, &name).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = utf8(name) else { return reply.error(Errno::ENOENT) };
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.rmdir(parent.0, &name).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let (Some(name), Some(newname)) = (utf8(name), utf8(newname)) else { return reply.error(Errno::EINVAL) };
        // Linux renameat2 flags: 1 = NOREPLACE; EXCHANGE/WHITEOUT are not supported.
        if flags.bits() & !1 != 0 {
            return reply.error(Errno::EINVAL);
        }
        let no_replace = flags.bits() & 1 != 0;
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.rename(parent.0, &name, newparent.0, &newname, no_replace).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let write = flags.0 & libc::O_ACCMODE != libc::O_RDONLY;
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            // Notice a change made on the NAS before handing out the contents
            // (see `getattr_fresh`).
            let res = match vfs.getattr_fresh(ino.0).await {
                Ok(_) => vfs.open_file(ino.0, write),
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => reply.opened(FileHandle(write as u64), OPEN_FLAGS),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn read(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.read(ino.0, offset, size).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn write(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        let (vfs, data) = (self.vfs.clone(), data.to_vec());
        self.rt.spawn(async move {
            match vfs.write(ino.0, offset, &data).await {
                Ok(n) => reply.written(n),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.vfs.release(ino.0, fh.0 & 1 == 1);
        reply.ok();
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.fsync(ino.0).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        let (vfs, dirs) = (self.vfs.clone(), self.dirs.clone());
        self.rt.spawn(async move {
            match vfs.readdir(ino.0).await {
                Ok(children) => {
                    let mut entries = vec![(ino.0, FileType::Directory, ".".to_string()), (ino.0, FileType::Directory, "..".to_string())];
                    entries.extend(children.into_iter().map(|(name, a)| (a.id, kind(a.kind), name)));
                    dirs.lock().unwrap().insert(fh, Arc::new(entries));
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(entries) = self.dirs.lock().unwrap().get(&fh.0).cloned() else { return reply.error(Errno::EBADF) };
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.dirs.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (total, free) = self.vfs.space();
        let block = BLOCK as u64;
        reply.statfs(total / block, free / block, free / block, 1 << 32, 1 << 31, BLOCK, 255, BLOCK);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some(name) = utf8(name) else { return reply.error(Errno::EINVAL) };
        let (vfs, owner) = (self.vfs.clone(), self.owner);
        self.rt.spawn(async move {
            match vfs.create(parent.0, &name).await {
                Ok(a) => reply.created(&TTL, &fattr(&a, owner), Generation(0), FileHandle(1), FopenFlags::empty()),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    // The `nasyncloud` command line talks to the mount through extended
    // attributes: no socket, and it works on whatever path the user points at.

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let value = match name.to_str() {
            Some(XATTR_STATUS) => self.vfs.status(ino.0).map(|s| serde_json::to_vec(&s).unwrap_or_default()),
            Some(XATTR_INFO) => Ok(serde_json::to_vec(&self.vfs.info()).unwrap_or_default()),
            _ => return reply.error(Errno::NO_XATTR),
        };
        match value {
            Err(e) => reply.error(errno(&e)),
            Ok(v) if size == 0 => reply.size(v.len() as u32),
            Ok(v) if v.len() > size as usize => reply.error(Errno::ERANGE),
            Ok(v) => reply.data(&v),
        }
    }

    fn setxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        let on = value.first().is_some_and(|b| *b == b'1');
        let res = match name.to_str() {
            Some(XATTR_PIN) => self.vfs.pin(ino.0, on),
            Some(XATTR_EVICT) => self.vfs.evict(ino.0).map(|_| ()),
            Some(XATTR_REFRESH) => {
                let vfs = self.vfs.clone();
                self.rt.spawn(async move {
                    match vfs.refresh(ino.0).await {
                        Ok(()) => reply.ok(),
                        Err(e) => reply.error(errno(&e)),
                    }
                });
                return;
            }
            _ => return reply.error(Errno::ENOTSUP),
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        // The control attributes are deliberately not listed, so `cp -a`,
        // rsync and friends never try to copy them.
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

/// Unmounts what [`mount`] mounted.
pub struct Unmounter(SessionUnmounter);

impl Unmounter {
    pub fn unmount(&mut self) -> io::Result<()> {
        self.0.unmount()
    }
}

/// Mounts the file system. Returns a channel that fires when the session ends
/// (someone unmounted it), plus a handle to unmount it ourselves.
pub fn mount(vfs: Arc<Vfs>, rt: Handle, mountpoint: &Path, volume_name: &str) -> io::Result<(tokio::sync::watch::Receiver<bool>, Unmounter)> {
    // SAFETY: plain getters without preconditions.
    let owner = unsafe { (libc::getuid(), libc::getgid()) };
    let fs = NasFs { vfs, rt, owner, dirs: Arc::default(), next_fh: AtomicU64::new(1) };

    let (session, unmounter) = {
        let _ = volume_name;
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::FSName("nasyncloud".into()),
            MountOption::Subtype("nasyncloud".into()),
            MountOption::NoAtime,
            MountOption::NoSuid,
            MountOption::NoDev,
            MountOption::DefaultPermissions,
        ];
        let mut session = Session::new(fs, mountpoint, &config)?;
        let unmounter = Unmounter(session.unmount_callable());
        (session, unmounter)
    };

    // The session loop runs on its own thread (not `spawn()`: that would take
    // the mount away from the unmounter).
    let (tx, ended) = tokio::sync::watch::channel(false);
    std::thread::Builder::new().name("fuse".into()).spawn(move || {
        if let Err(e) = session.run() {
            log::warn!("the FUSE session ended with an error: {e}");
        }
        tx.send_replace(true);
    })?;
    Ok((ended, unmounter))
}
