//! Linux/macOS: the FUSE mount (`Mounted`), plus the command-line helpers
//! that talk to a running mount through extended attributes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crate::fuse::Unmounter;
use nasdrive::config::{format_size, Config};
use nasdrive::vfs::{Info, NodeId, NodeStatus, Pin, Vfs, VfsOptions, ROOT, XATTR_EVICT, XATTR_INFO, XATTR_STATUS};
use tokio::sync::watch;

use crate::service::{EntryView, Listing, MountInfo};

pub struct Mounted {
    vfs: Arc<Vfs>,
    mountpoint: PathBuf,
    unmounter: Mutex<Option<Unmounter>>,
    /// Becomes `true` when the FUSE session is over, whoever ended it.
    ended: watch::Receiver<bool>,
    stopping: AtomicBool,
}

impl Mounted {
    pub async fn start(cfg: &Config) -> Result<Self> {
        let mountpoint = cfg.mountpoint();
        let opts = VfsOptions::from_config(cfg)?;
        std::fs::create_dir_all(&opts.state_dir).with_context(|| format!("cannot create {}", opts.state_dir.display()))?;
        prepare_mountpoint(&mountpoint)?;
        let (dav, online) = crate::connect(cfg).await?;
        if !online {
            log::warn!("starting with what is already on this computer");
        }
        let vfs = Vfs::open(opts, Arc::new(dav)).await?;
        let volume = cfg.url.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty() && !s.contains(':')).unwrap_or("NAS").to_string();
        let (ended, unmounter) = match crate::fuse::mount(vfs.clone(), tokio::runtime::Handle::current(), &mountpoint, &volume) {
            Ok(m) => m,
            Err(e) => {
                vfs.shutdown().await;
                return Err(e).with_context(|| format!("cannot mount on {} (is FUSE installed? Linux: the `fuse3` package; macOS: macFUSE)", mountpoint.display()));
            }
        };
        log::info!("{} is mounted on {}", cfg.url, mountpoint.display());
        Ok(Self { vfs, mountpoint, unmounter: Mutex::new(Some(unmounter)), ended, stopping: AtomicBool::new(false) })
    }

    pub async fn wait_ended(&self) {
        let mut rx = self.ended.clone();
        let _ = rx.wait_for(|done| *done).await;
    }

    /// Unmounts (unless already gone), lets pending uploads finish, saves the index.
    pub async fn stop(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let done = *self.ended.borrow();
        if !done {
            log::info!("unmounting {}", self.mountpoint.display());
            let unmounter = self.unmounter.lock().unwrap().take();
            if let Some(mut u) = unmounter {
                if let Err(e) = u.unmount() {
                    log::warn!("could not unmount cleanly: {e}");
                }
            }
            let mut rx = self.ended.clone();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|done| *done)).await;
            if is_mounted(&self.mountpoint) {
                log::warn!("{} is still in use; detaching it", self.mountpoint.display());
                let _ = force_unmount(&self.mountpoint);
            }
        }
        drain(&self.vfs).await;
        self.vfs.shutdown().await;
    }

    pub fn info(&self) -> MountInfo {
        let i = self.vfs.info();
        MountInfo { online: i.online, pending_uploads: i.pending_uploads, uploading: i.uploading, cache_bytes: Some(i.cache_bytes), cache_max: Some(i.cache_max), errors: i.errors }
    }

    async fn resolve(&self, rel: &str) -> Result<NodeId> {
        let mut cur = ROOT;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            cur = self.vfs.lookup(cur, part).await.map_err(|e| anyhow!("{part}: {e}"))?.id;
        }
        Ok(cur)
    }

    pub async fn browse(&self, rel: &str) -> Result<Listing> {
        let id = self.resolve(rel).await?;
        let children = self.vfs.readdir(id).await?;
        let this = view(rel.rsplit('/').next().unwrap_or_default(), self.vfs.getattr(id)?.mtime, self.vfs.status(id)?);
        let entries = children.into_iter().filter_map(|(name, attr)| Some(view(&name, attr.mtime, self.vfs.status(attr.id).ok()?))).collect();
        Ok(Listing { path: rel.to_string(), this, entries })
    }

    pub async fn pin(&self, rel: &str, on: bool) -> Result<()> {
        let id = self.resolve(rel).await?;
        self.vfs.pin(id, on)?;
        Ok(())
    }

    pub async fn free(&self, rel: &str) -> Result<u64> {
        let id = self.resolve(rel).await?;
        Ok(self.vfs.evict(id)?)
    }

    pub async fn refresh(&self, rel: &str) -> Result<()> {
        let id = self.resolve(rel).await?;
        self.vfs.refresh(id).await?;
        Ok(())
    }
}

fn view(name: &str, mtime: i64, s: NodeStatus) -> EntryView {
    let pinned = match s.pinned {
        Pin::No => "no",
        Pin::Yes => "yes",
        Pin::Inherited => "inherited",
    };
    EntryView { name: name.to_string(), dir: s.dir, size: s.size, mtime, state: s.state, pinned: pinned.into(), cached: s.cached, files: s.files, files_cached: s.files_cached, pending: s.pending, error: s.error }
}

/// Gives pending uploads a chance to finish before the mount goes away.
async fn drain(vfs: &Arc<Vfs>) {
    let mut left = vfs.pending_uploads();
    if left == 0 {
        return;
    }
    if !vfs.is_online() {
        log::warn!("{left} change(s) are not on the NAS yet (it is unreachable); they will be sent next time");
        return;
    }
    log::info!("sending {left} pending change(s) to the NAS — Ctrl+C to stop (they would resume next time)");
    let mut progress = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
        let info = vfs.info();
        if info.pending_uploads == 0 {
            log::info!("everything is on the NAS");
            return;
        }
        if (info.pending_uploads as usize) < left || info.uploading > 0 {
            left = info.pending_uploads as usize;
            progress = Instant::now();
        }
        if !info.online || progress.elapsed() > Duration::from_secs(30) {
            break;
        }
    }
    log::warn!("{} change(s) still pending; they will be sent next time", vfs.pending_uploads());
}

fn is_mounted(mountpoint: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(mountpoint), mountpoint.parent().map(std::fs::metadata)) {
        (Ok(m), Some(Ok(p))) => m.dev() != p.dev(),
        (Err(e), _) => matches!(e.raw_os_error(), Some(libc::ENOTCONN) | Some(libc::EIO)),
        _ => false,
    }
}

fn prepare_mountpoint(mountpoint: &Path) -> Result<()> {
    match std::fs::read_dir(mountpoint) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!("{} is not empty: it is either already mounted or not a suitable folder", mountpoint.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(mountpoint).with_context(|| format!("cannot create {}", mountpoint.display()))?;
        }
        // A previous session died without unmounting.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTCONN) | Some(libc::EIO)) => {
            log::warn!("cleaning up a stale mount on {}", mountpoint.display());
            force_unmount(mountpoint)?;
        }
        Err(e) => return Err(e).with_context(|| format!("cannot use {}", mountpoint.display())),
    }
    Ok(())
}

pub fn force_unmount(mountpoint: &Path) -> Result<()> {
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") { &[&["umount"], &["diskutil", "unmount", "force"]] } else { &[&["fusermount3", "-u"], &["fusermount", "-u"], &["fusermount3", "-uz"], &["umount", "-l"]] };
    let mut last = String::new();
    for cmd in attempts {
        match Command::new(cmd[0]).args(&cmd[1..]).arg(mountpoint).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => last = String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Err(e) => last = format!("{}: {e}", cmd[0]),
        }
    }
    bail!("could not unmount {}: {last}", mountpoint.display())
}

// ----------------------------------------------------------------------
// Command line helpers (`nasdrive pin/unpin/free/status/ls/refresh`)

fn not_ours(path: &Path) -> anyhow::Error {
    anyhow!("{} is not inside a running nasdrive mount", path.display())
}

fn get_status(path: &Path) -> Result<NodeStatus> {
    let raw = xattr::get(path, XATTR_STATUS).map_err(|_| not_ours(path))?.ok_or_else(|| not_ours(path))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn set_all(paths: &[PathBuf], attr: &str, value: &[u8], done: &str) -> Result<()> {
    if paths.is_empty() {
        bail!("give at least one path inside the mount");
    }
    for path in paths {
        get_status(path)?;
        xattr::set(path, attr, value).with_context(|| format!("{}", path.display()))?;
        println!("{}: {done}", path.display());
    }
    Ok(())
}

pub fn free(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("give at least one path inside the mount");
    }
    for path in paths {
        let before = get_status(path)?.cached;
        xattr::set(path, XATTR_EVICT, b"1").map_err(|e| match e.raw_os_error() {
            Some(libc::EBUSY) => anyhow!("{} is kept offline by a parent folder; unpin that folder first", path.display()),
            _ => anyhow!("{}: {e}", path.display()),
        })?;
        let after = get_status(path)?.cached;
        println!("{}: freed {}", path.display(), format_size(before.saturating_sub(after)));
    }
    Ok(())
}

fn describe(s: &NodeStatus) -> String {
    let pin = match s.pinned {
        Pin::Yes => "pinned",
        Pin::Inherited => "pinned (by a parent)",
        Pin::No => "",
    };
    let mut line = if s.dir {
        format!("folder     {}/{} files local, {} of {}", s.files_cached, s.files, format_size(s.cached), format_size(s.size))
    } else {
        format!("{:<10} {} of {} local", s.state, format_size(s.cached), format_size(s.size))
    };
    for extra in [pin.to_string(), if s.dir && s.pending > 0 { format!("{} waiting to upload", s.pending) } else { String::new() }, s.error.as_ref().map(|e| format!("error: {e}")).unwrap_or_default()] {
        if !extra.is_empty() {
            line.push_str(", ");
            line.push_str(&extra);
        }
    }
    line
}

pub fn status(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        println!("{}: {}", path.display(), describe(&get_status(path)?));
    }
    Ok(())
}

pub fn info(mountpoint: &Path) -> Result<()> {
    let raw = xattr::get(mountpoint, XATTR_INFO).ok().flatten().ok_or_else(|| anyhow!("nothing is mounted on {} (start it with `nasdrive mount` or `nasdrive ui`)", mountpoint.display()))?;
    let info: Info = serde_json::from_slice(&raw)?;
    println!("mount:    {}", mountpoint.display());
    println!("NAS:      {}", if info.online { "reachable" } else { "UNREACHABLE (working offline)" });
    println!("cache:    {} of {} ({} known items)", format_size(info.cache_bytes), format_size(info.cache_max), info.nodes);
    println!("pending:  {} change(s) waiting to upload, {} uploading now", info.pending_uploads, info.uploading);
    for e in info.errors {
        println!("problem:  {e}");
    }
    Ok(())
}

pub fn ls(dir: &Path) -> Result<()> {
    get_status(dir)?;
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| Some(e.ok()?.path())).collect();
    names.sort();
    for path in names {
        let s = get_status(&path)?;
        let pin = if s.pinned == Pin::No { ' ' } else { '*' };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        println!("{:<9}{pin} {:>10}  {name}{}", if s.dir { "folder" } else { s.state.as_str() }, format_size(s.size), if s.dir { "/" } else { "" });
    }
    Ok(())
}
