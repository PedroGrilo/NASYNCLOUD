//! Moving bytes: on-demand range downloads and background uploads.

use std::sync::atomic::AtomicU8;

use futures_util::StreamExt;

use super::*;
use crate::activity::Direction;
use crate::remote::norm_etag;

/// A reader this close ahead of a running stream waits for it instead of
/// opening another connection.
const SEEK_AHEAD: u64 = 2 << 20;
const MAX_STREAMS: usize = 4;
const IDLE_CLOSE: Duration = Duration::from_secs(5);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(90);

/// One ranged download stream feeding a file's cache.
/// An upload shown in the activity list. Unless it ends with `ok` or `fail`,
/// it disappears (it will be retried, and shown again then).
struct UploadAct<'a> {
    activity: &'a crate::activity::Activity,
    id: u64,
    over: bool,
}

impl UploadAct<'_> {
    fn ok(mut self, bytes: u64) {
        self.activity.progress(self.id, bytes);
        self.activity.end(self.id, None);
        self.over = true;
    }

    fn fail(mut self, error: String) {
        self.activity.end(self.id, Some(error));
        self.over = true;
    }
}

impl Drop for UploadAct<'_> {
    fn drop(&mut self) {
        if !self.over {
            self.activity.discard_if_empty(self.id);
        }
    }
}

pub(super) struct Dl {
    start: u64,
    /// Where the stream stops (next cached range, or end of file).
    end: u64,
    /// Everything in `[start, pos)` has been stored.
    pos: AtomicU64,
    /// How far readers have asked for so far (`u64::MAX`: fetch it all).
    want: AtomicU64,
    wake: Notify,
    done: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<VfsError>>,
}

impl Dl {
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// A reader got as far as `upto`: keep the stream ahead of it.
    pub(super) fn touch(&self, upto: u64) {
        if self.start <= upto && upto <= self.end && self.want.fetch_max(upto, Ordering::SeqCst) < upto {
            self.wake.notify_one();
        }
    }
}

const PH_TMP: u8 = 1;
const PH_MOVE: u8 = 2;
const PH_DIRECT: u8 = 3;
const PH_DONE: u8 = 4;

/// What a cancelled upload may have left behind on the server.
#[derive(Default)]
pub(super) struct UploadShared {
    phase: AtomicU8,
    tmp: Mutex<Option<String>>,
    dest: Mutex<Option<String>>,
}

pub(super) struct UploadHandle {
    shared: Arc<UploadShared>,
    task: JoinHandle<()>,
}

impl Vfs {
    /// Notes the "we seem to be offline" side effect of a failed request.
    pub(super) fn net<T>(&self, r: std::result::Result<T, RemoteError>) -> Result<T> {
        r.map_err(|e| {
            self.note_error(&e);
            e.into()
        })
    }

    // ------------------------------------------------------------------
    // Downloads

    /// Makes sure some stream is going to deliver the byte at `gap`, and
    /// returns it together with a receiver that fires on every bit of progress.
    pub(super) fn demand(self: &Arc<Self>, st: &mut State, id: NodeId, gap: u64, upto: u64, greedy: bool) -> Result<(Arc<Dl>, watch::Receiver<u64>)> {
        let path = st.path(id);
        let online = self.is_online();
        let n = st.file_mut(id)?;
        let progress = n.progress.get_or_insert_with(|| Arc::new(watch::channel(0u64).0)).clone();
        let rx = progress.subscribe();
        let want = if greedy { u64::MAX } else { upto };
        if let Some(dl) = n.dls.iter().find(|d| !d.done.load(Ordering::SeqCst) && d.start <= gap && gap < d.end && gap <= d.pos.load(Ordering::SeqCst) + SEEK_AHEAD) {
            dl.want.fetch_max(want, Ordering::SeqCst);
            dl.wake.notify_one();
            return Ok((dl.clone(), rx));
        }
        let version = n.remote.clone().ok_or_else(|| VfsError::Io("the content is not available on this machine".into()))?;
        let path = path.ok_or(VfsError::NotFound)?;
        if !online {
            return Err(VfsError::Offline("this file is not available offline".into()));
        }
        let end = n.cached.next_start_after(gap).unwrap_or(n.size).min(n.size);
        let dl = Arc::new(Dl {
            start: gap,
            end,
            pos: AtomicU64::new(gap),
            want: AtomicU64::new(want),
            wake: Notify::new(),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        if n.dls.len() >= MAX_STREAMS {
            n.dls.remove(0).cancel();
        }
        n.dls.push(dl.clone());
        let file = self.cache_file(n)?;
        let gen = n.cache_gen;
        let (me, task_dl) = (self.clone(), dl.clone());
        tokio::spawn(async move { me.run_download(id, task_dl, path, version, file, gen, progress).await });
        Ok((dl, rx))
    }

    /// Waits until the stream made progress (or ended). The caller then
    /// re-checks whether its bytes arrived.
    pub(super) async fn wait_progress(&self, dl: &Dl, rx: &mut watch::Receiver<u64>) -> Result<()> {
        match tokio::time::timeout(PROGRESS_TIMEOUT, rx.changed()).await {
            Err(_) => Err(VfsError::Offline("timed out waiting for data".into())),
            Ok(_) => {
                if dl.done.load(Ordering::SeqCst) {
                    if let Some(e) = dl.error.lock().unwrap().clone() {
                        return Err(e);
                    }
                }
                Ok(())
            }
        }
    }

    /// Downloads whatever is missing of a file (pinning, or before editing it in place).
    pub(super) async fn ensure_full(self: &Arc<Self>, id: NodeId) -> Result<()> {
        loop {
            let (dl, mut rx) = {
                let mut st = self.st();
                let n = st.file(id)?;
                let size = n.size;
                match n.cached.first_gap(0, size) {
                    None => return Ok(()),
                    Some(gap) => self.demand(&mut st, id, gap, size, true)?,
                }
            };
            self.wait_progress(&dl, &mut rx).await?;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_download(self: Arc<Self>, id: NodeId, dl: Arc<Dl>, path: String, version: Version, file: Arc<File>, gen: u32, progress: Arc<watch::Sender<u64>>) {
        let act = self.activity.begin(Direction::Download, &path, version.size);
        let res = self.download(id, &dl, &path, &version, &file, gen, &progress, act).await;
        match &res {
            // Nothing to show for a read that stopped at once or a file that changed meanwhile.
            Ok(()) | Err(RemoteError::Changed) => self.activity.discard_if_empty(act),
            Err(e) => self.activity.end(act, Some(e.to_string())),
        }
        let mut recheck = None;
        {
            let mut st = self.st();
            if let Some(n) = st.nodes.get_mut(&id) {
                n.dls.retain(|d| !Arc::ptr_eq(d, &dl));
                if n.opens == 0 && n.dls.is_empty() {
                    n.file = None;
                }
                if matches!(res, Err(RemoteError::Changed | RemoteError::NotFound)) {
                    recheck = Some(n.parent);
                }
                st.touch(id);
            }
        }
        if let Err(e) = res {
            log::debug!("download of {path} stopped: {e}");
            self.note_error(&e);
            *dl.error.lock().unwrap() = Some(e.into());
        }
        dl.done.store(true, Ordering::SeqCst);
        progress.send_modify(|v| *v += 1);
        self.flush_wake.notify_one();
        if let Some(parent) = recheck {
            // The file is not what we thought it was: look at the folder again.
            if let Some(p) = self.st().nodes.get_mut(&parent) {
                p.listed_at = None;
            }
            let _ = self.refresh_dir(parent).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn download(&self, id: NodeId, dl: &Dl, path: &str, version: &Version, file: &File, gen: u32, progress: &watch::Sender<u64>, act: u64) -> std::result::Result<(), RemoteError> {
        let mut d = self.remote.open_range(path, dl.start, Some(dl.end)).await?;
        if let (Some(a), Some(b)) = (&d.etag, &version.etag) {
            if norm_etag(a) != norm_etag(b) {
                return Err(RemoteError::Changed);
            }
        }
        if d.total.is_some_and(|t| t != version.size) {
            return Err(RemoteError::Changed);
        }
        self.set_online(true);
        let mut pos = dl.start;
        let mut skip = d.skip;
        while pos < dl.end {
            // Back-pressure: stay only `readahead` in front of the readers,
            // and hang up when nobody has asked for more in a while.
            loop {
                if dl.cancelled.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let want = dl.want.load(Ordering::SeqCst);
                if want == u64::MAX || pos < want.saturating_add(self.opts.readahead) {
                    break;
                }
                if tokio::time::timeout(IDLE_CLOSE, dl.wake.notified()).await.is_err() {
                    return Ok(());
                }
            }
            let chunk = match tokio::time::timeout(CHUNK_TIMEOUT, d.stream.next()).await {
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
            let take = (dl.end - pos).min(data.len() as u64);
            if take == 0 {
                continue;
            }
            {
                let mut st = self.st();
                let Some(n) = st.nodes.get_mut(&id) else { return Ok(()) };
                if n.cache_gen != gen || n.dirty {
                    return Ok(());
                }
                if !n.cached.contains(pos, pos + take) {
                    // Under the lock: must not interleave with a local write.
                    file.write_all_at(&data[..take as usize], pos).map_err(|e| RemoteError::Other(format!("local cache: {e}")))?;
                    let added = n.cached.insert(pos, pos + take);
                    n.unsynced = true;
                    st.cache_bytes += added;
                    self.activity.progress(act, added);
                    if st.cache_bytes > self.opts.cache_max {
                        self.evict_wake.notify_one();
                    }
                }
            }
            pos += take;
            dl.pos.store(pos, Ordering::SeqCst);
            progress.send_modify(|v| *v += 1);
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Uploads

    pub(super) async fn uploader(self: Arc<Self>) {
        while !self.stopped() {
            let wait = self.dispatch();
            tokio::select! {
                _ = self.upload_wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
                _ = self.stop_notify.notified() => break,
            }
        }
    }

    /// Starts every job that is due (within the parallelism limit) and returns
    /// how long to sleep until the next one.
    fn dispatch(self: &Arc<Self>) -> Duration {
        let mut next = Duration::from_secs(2);
        if !self.is_online() {
            return next;
        }
        let mut guard = self.st();
        let st = &mut *guard;
        let now = Instant::now();
        let mut due = Vec::new();
        for (id, job) in &st.jobs {
            if job.due <= now {
                due.push(*id);
            } else {
                next = next.min(job.due - now);
            }
        }
        // Lower ids first: folders are always created before what goes in them.
        due.sort_unstable();
        for id in due {
            if st.uploading >= self.opts.parallel_uploads {
                break;
            }
            let Some(n) = st.nodes.get_mut(&id) else {
                st.jobs.remove(&id);
                continue;
            };
            if n.upload.is_some() {
                continue;
            }
            let nothing_to_do = match n.kind {
                Kind::File => n.writers > 0 || !n.local_change(),
                Kind::Dir => n.remote.is_some(),
            };
            if nothing_to_do || n.unlinked {
                st.jobs.remove(&id);
                continue;
            }
            let shared = Arc::new(UploadShared::default());
            let (me, sh) = (self.clone(), shared.clone());
            let task = tokio::spawn(async move { me.run_job(id, sh).await });
            n.upload = Some(UploadHandle { shared, task });
            st.uploading += 1;
        }
        next
    }

    async fn run_job(self: Arc<Self>, id: NodeId, sh: Arc<UploadShared>) {
        let kind = self.st().nodes.get(&id).map(|n| n.kind);
        let res = match kind {
            None => Ok(()),
            Some(Kind::Dir) => self.ensure_remote_dir(id).await,
            Some(Kind::File) => self.upload_file(id, &sh).await,
        };
        {
            let mut guard = self.st();
            let st = &mut *guard;
            st.uploading = st.uploading.saturating_sub(1);
            let mut again = false;
            if let Some(n) = st.nodes.get_mut(&id) {
                n.upload = None;
                n.error = res.as_ref().err().map(|e| e.to_string());
                // Modified again while it was being sent?
                again = n.local_change() && !n.unlinked && n.writers == 0 && !is_local_only_name(&n.name);
                st.stale.insert(id);
            }
            match &res {
                Ok(()) if again => {
                    st.jobs.insert(id, Job { due: Instant::now() + self.opts.upload_delay, attempts: 0 });
                }
                Ok(()) => {
                    st.jobs.remove(&id);
                }
                Err(e) => {
                    let attempts = st.jobs.get(&id).map_or(0, |j| j.attempts) + 1;
                    let backoff = Duration::from_secs((1u64 << attempts.min(9)).min(300));
                    st.jobs.insert(id, Job { due: Instant::now() + backoff, attempts });
                    log::warn!("could not sync {:?} (attempt {attempts}, retrying in {}s): {e}", st.path(id).unwrap_or_default(), backoff.as_secs());
                }
            }
        }
        if matches!(res, Err(VfsError::Offline(_))) {
            self.set_online(false);
        }
        self.upload_wake.notify_one();
        self.flush_wake.notify_one();
    }

    fn part_name(&self) -> String {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        format!(".{:x}-{:x}-{:x}{}", std::process::id(), nanos, self.part_seq.fetch_add(1, Ordering::SeqCst), remote::PART_SUFFIX)
    }

    async fn upload_file(self: &Arc<Self>, id: NodeId, sh: &UploadShared) -> Result<()> {
        for _ in 0..4 {
            let (parent, path, base, gen, size, mtime, src, trusted) = {
                let st = self.st();
                let Some(n) = st.nodes.get(&id) else { return Ok(()) };
                if n.kind != Kind::File || n.unlinked || !n.local_change() || n.writers > 0 {
                    return Ok(());
                }
                let Some(path) = st.path(id) else { return Ok(()) };
                (n.parent, path, n.remote.clone(), n.local_gen, n.size, n.mtime, cache_path(&self.data_dir, id, n.cache_gen), n.overwrite_ok)
            };
            self.ensure_remote_dir(parent).await?;
            // Did the file change on the server since the version we started from?
            if !trusted {
                if let Some(cur) = self.net(self.remote.stat(&path).await)? {
                    let ours = !cur.is_dir && base.as_ref().is_some_and(|b| b.same(&cur.version));
                    if !ours {
                        self.make_conflict_copy(id)?;
                        continue;
                    }
                }
            }
            if size == 0 && !src.exists() {
                File::create(&src)?;
            }
            *sh.dest.lock().unwrap() = Some(path.clone());
            let changed_under_us = || self.st().nodes.get(&id).is_none_or(|n| n.local_gen != gen);
            let atomic = self.atomic_upload.load(Ordering::SeqCst);
            let target = if atomic { remote::join(remote::split(&path).0, &self.part_name()) } else { path.clone() };
            if atomic {
                *sh.tmp.lock().unwrap() = Some(target.clone());
            }
            sh.phase.store(if atomic { PH_TMP } else { PH_DIRECT }, Ordering::SeqCst);
            let act = UploadAct { activity: &self.activity, id: self.activity.begin(Direction::Upload, &path, size), over: false };
            if let Err(e) = self.remote.upload(&target, &src, size, mtime).await {
                if atomic {
                    let _ = self.remote.delete(&target, false).await;
                } else if let Some(n) = self.st().nodes.get_mut(&id) {
                    // The copy on the server is now a half-written one of ours.
                    n.overwrite_ok = true;
                }
                if changed_under_us() {
                    return Ok(()); // edited while uploading: the next round sends the new content
                }
                if matches!(e, RemoteError::Precondition(_)) {
                    self.recheck_dir(parent).await;
                }
                act.fail(e.to_string());
                return self.net(Err(e));
            }
            if atomic {
                sh.phase.store(PH_MOVE, Ordering::SeqCst);
                if let Err(e) = self.remote.rename(&target, &path, false, true).await {
                    let _ = self.remote.delete(&target, false).await;
                    if matches!(e, RemoteError::Http(405 | 501, _) | RemoteError::Precondition(_) | RemoteError::Forbidden) {
                        log::warn!("the server cannot rename over an existing file ({e}); uploading in place from now on");
                        self.atomic_upload.store(false, Ordering::SeqCst);
                        continue;
                    }
                    act.fail(e.to_string());
                    return self.net(Err(e));
                }
                *sh.tmp.lock().unwrap() = None;
            }
            sh.phase.store(PH_DONE, Ordering::SeqCst);
            let version = self.net(self.remote.stat(&path).await)?.map(|e| e.version).ok_or_else(|| VfsError::Io("the uploaded file is not on the server".into()))?;
            let mut st = self.st();
            let Some(n) = st.nodes.get_mut(&id) else { return Ok(()) };
            if version.size != size && n.local_gen == gen {
                return Err(VfsError::Io(format!("size mismatch after upload ({} instead of {size} bytes)", version.size)));
            }
            n.remote = Some(version);
            n.overwrite_ok = false;
            if n.local_gen == gen {
                n.dirty = false;
            }
            let parent = n.parent;
            st.touch(id);
            if let Some(p) = st.nodes.get_mut(&parent) {
                p.mut_gen += 1;
            }
            log::info!("uploaded {path} ({})", crate::config::format_size(size));
            act.ok(size);
            return Ok(());
        }
        Err(VfsError::Io("could not settle on a name for the upload".into()))
    }

    /// Creates on the server every folder from the root down to `dir` that
    /// only exists here so far.
    pub(super) async fn ensure_remote_dir(self: &Arc<Self>, dir: NodeId) -> Result<()> {
        loop {
            let (id, path, parent) = {
                let st = self.st();
                let mut top = None;
                let mut cur = dir;
                let mut depth = 0;
                while cur != ROOT {
                    let n = st.dir(cur)?;
                    if n.unlinked || depth > 1024 {
                        return Err(VfsError::NotFound);
                    }
                    if n.remote.is_none() {
                        top = Some((cur, n.parent));
                    }
                    cur = n.parent;
                    depth += 1;
                }
                match top {
                    None => return Ok(()),
                    Some((id, parent)) => (id, st.path(id).ok_or(VfsError::NotFound)?, parent),
                }
            };
            if let Err(e) = self.remote.mkdir(&path).await {
                if matches!(e, RemoteError::Precondition(_)) {
                    self.recheck_dir(parent).await;
                }
                return self.net(Err(e));
            }
            let mut st = self.st();
            // Renamed while we were talking to the server? Then go again.
            if st.path(id).as_deref() != Some(path.as_str()) {
                continue;
            }
            if let Some(n) = st.nodes.get_mut(&id) {
                n.remote.get_or_insert_with(Version::default);
                st.touch(id);
            }
            if let Some(p) = st.nodes.get_mut(&parent) {
                p.mut_gen += 1;
            }
            log::info!("created folder {path} on the NAS");
        }
    }

    /// A request failed because a parent folder seems to be missing on the
    /// server: check, and if so turn it back into a local-only folder so it
    /// gets created again.
    async fn recheck_dir(self: &Arc<Self>, dir: NodeId) {
        if dir == ROOT {
            return;
        }
        let Some(path) = self.st().path(dir) else { return };
        if let Ok(None) = self.remote.stat(&path).await {
            let mut st = self.st();
            if st.nodes.get(&dir).is_some_and(|n| n.remote.is_some()) {
                log::warn!("folder {path} disappeared from the NAS; it will be created again for the pending changes");
                self.detach_from_server(&mut st, dir);
            }
        }
    }

    /// Stops an upload in flight. Returns the destination path when a (maybe
    /// partial) copy of ours could already be sitting there.
    pub(super) async fn cancel_upload(&self, up: UploadHandle) -> Option<String> {
        up.task.abort();
        let aborted = matches!(up.task.await, Err(e) if e.is_cancelled());
        if !aborted {
            return None; // it finished on its own and already cleaned up
        }
        {
            let mut st = self.st();
            st.uploading = st.uploading.saturating_sub(1);
        }
        let tmp = up.shared.tmp.lock().unwrap().take();
        if let Some(tmp) = tmp {
            let _ = self.remote.delete(&tmp, false).await;
        }
        self.upload_wake.notify_one();
        match up.shared.phase.load(Ordering::SeqCst) {
            PH_MOVE | PH_DIRECT | PH_DONE => up.shared.dest.lock().unwrap().clone(),
            _ => None,
        }
    }

    /// Cleans up after `cancel_upload` reported a possible copy at `partial`
    /// for a node that is about to move elsewhere.
    pub(super) async fn after_cancelled_upload(&self, id: NodeId, partial: &str) {
        let on_server = match self.st().nodes.get_mut(&id) {
            Some(n) => {
                n.overwrite_ok = true;
                n.remote.is_some()
            }
            None => false,
        };
        if !on_server {
            // It never existed there before: do not leave a stray copy behind.
            let _ = self.remote.delete(partial, false).await;
        }
    }

    /// The file changed both here and on the server. Keep both: the local
    /// version gets a new name, the server's version reappears under the old one.
    fn make_conflict_copy(&self, id: NodeId) -> Result<()> {
        let mut guard = self.st();
        let st = &mut *guard;
        let (parent, old) = {
            let n = st.get(id)?;
            (n.parent, n.name.clone())
        };
        let new = {
            let siblings = &st.dir(parent)?.children;
            remote::conflict_name(&old, unix_now(), |c| siblings.contains_key(c))
        };
        let p = st.dir_mut(parent)?;
        p.children.remove(&old);
        p.children.insert(new.clone(), id);
        p.mut_gen += 1;
        p.listed_at = None; // so the server's version shows up again
        let n = st.get_mut(id)?;
        n.name = new.clone();
        n.remote = None;
        n.dirty = true;
        n.overwrite_ok = false;
        st.touch(id);
        log::warn!("{old:?} changed both here and on the NAS; keeping the local version as {new:?}");
        Ok(())
    }
}
