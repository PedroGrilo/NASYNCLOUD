//! Keeping the local picture of the server up to date: folder listings,
//! pinned ("keep offline") trees, connectivity and cache size.

use super::*;
use crate::remote::Entry;

impl Vfs {
    pub(super) fn local_changes(&self, id: NodeId) -> bool {
        self.st().has_local_changes(id)
    }

    /// Fetches the listing of `dir` from the server and merges it in.
    pub(super) async fn refresh_dir(self: &Arc<Self>, dir: NodeId) -> Result<()> {
        let lock = {
            let mut st = self.st();
            st.dir_mut(dir)?.refresh_lock.get_or_insert_with(Default::default).clone()
        };
        // One refresh per folder at a time; latecomers reuse its result.
        let _guard = lock.lock().await;
        for _ in 0..3 {
            let (path, gen) = {
                let st = self.st();
                let n = st.dir(dir)?;
                if n.remote.is_none() || (n.listed && n.listed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300))) {
                    return Ok(());
                }
                (st.path(dir).ok_or(VfsError::NotFound)?, n.mut_gen)
            };
            match self.remote.list(&path).await {
                Ok(entries) => {
                    self.set_online(true);
                    self.sweep_parts(&path, &entries);
                    // Not applied when something changed locally meanwhile: ask again.
                    if self.merge_listing(&mut self.st(), dir, gen, entries) {
                        return Ok(());
                    }
                }
                Err(RemoteError::NotFound) if dir != ROOT => {
                    // Gone on the server: the parent's next listing sorts it out.
                    let mut st = self.st();
                    let parent = st.get(dir)?.parent;
                    if let Some(p) = st.nodes.get_mut(&parent) {
                        p.listed_at = None;
                    }
                    return Err(VfsError::NotFound);
                }
                Err(e) => return self.net(Err(e)),
            }
        }
        Ok(())
    }

    /// Removes upload leftovers (ours, from interrupted sessions) older than a day.
    fn sweep_parts(self: &Arc<Self>, dir: &str, entries: &[Entry]) {
        let now = unix_now();
        let old: Vec<String> = entries.iter().filter(|e| !e.is_dir && remote::is_part_file(&e.name) && now - e.version.mtime > 86_400).map(|e| remote::join(dir, &e.name)).collect();
        if !old.is_empty() {
            let me = self.clone();
            tokio::spawn(async move {
                for path in old {
                    let _ = me.remote.delete(&path, false).await;
                }
            });
        }
    }

    /// Looks at a folder on the server right now. `true` when it holds nothing
    /// (or is gone); otherwise what it holds is merged in and `false` returned.
    pub(super) async fn remote_dir_empty(self: &Arc<Self>, id: NodeId, path: &str) -> Result<bool> {
        match self.remote.list(path).await {
            Ok(entries) => {
                if entries.iter().all(|e| remote::is_part_file(&e.name)) {
                    return Ok(true);
                }
                let mut st = self.st();
                let gen = st.dir(id)?.mut_gen;
                self.merge_listing(&mut st, id, gen, entries);
                Ok(false)
            }
            Err(RemoteError::NotFound) => Ok(true),
            Err(e) => self.net(Err(e)),
        }
    }

    /// Applies a server listing to the tree. Returns `false` (and changes
    /// nothing) when the folder was modified locally since `gen` was read,
    /// because the listing may then predate that change.
    pub(super) fn merge_listing(&self, st: &mut State, dir: NodeId, gen: u64, entries: Vec<Entry>) -> bool {
        let Some(d) = st.nodes.get(&dir) else { return true };
        if d.mut_gen != gen {
            return false;
        }
        let existing = d.children.clone();
        let pinned = st.pin_of(dir) != Pin::No;
        let mut seen = HashSet::new();
        let mut changed = false;
        for e in entries {
            if !valid_name(&e.name) {
                continue;
            }
            let kind = if e.is_dir { Kind::Dir } else { Kind::File };
            seen.insert(e.name.clone());
            if let Some(&cid) = existing.get(&e.name) {
                let Some(n) = st.nodes.get(&cid) else { continue };
                if n.kind == kind {
                    changed |= self.update_from_server(st, cid, &e.version);
                    continue;
                }
                // A file turned into a folder or vice versa.
                if st.has_local_changes(cid) {
                    continue;
                }
                self.drop_subtree(st, cid);
            }
            let id = st.alloc_id();
            let mut n = Node::new(id, dir, &e.name, kind);
            n.size = e.version.size;
            n.mtime = e.version.mtime;
            n.remote = Some(e.version);
            st.insert(n);
            changed = true;
        }
        for (name, cid) in existing {
            if seen.contains(&name) {
                continue;
            }
            let Some(n) = st.nodes.get(&cid) else { continue };
            if n.remote.is_none() {
                continue; // only exists here so far
            }
            if st.has_local_changes(cid) {
                // Deleted on the server but edited here: the edits win.
                self.detach_from_server(st, cid);
            } else {
                self.drop_subtree(st, cid);
            }
        }
        if let Some(d) = st.nodes.get_mut(&dir) {
            d.listed = true;
            d.listed_at = Some(Instant::now());
        }
        st.touch(dir);
        self.flush_wake.notify_one();
        if changed && pinned {
            self.pin_wake.notify_one();
        }
        true
    }

    /// Updates one known node from the server's current version. Returns
    /// whether the content changed.
    fn update_from_server(&self, st: &mut State, id: NodeId, v: &Version) -> bool {
        let Some(n) = st.nodes.get_mut(&id) else { return false };
        if n.kind == Kind::Dir {
            if n.remote.is_none() {
                n.remote = Some(v.clone());
                st.touch(id);
            }
            return false;
        }
        // With local changes pending, differences are settled when uploading.
        let outdated = !n.dirty && n.remote.as_ref().is_some_and(|base| !base.same(v));
        if !outdated {
            return false;
        }
        log::debug!("{:?} changed on the server", n.name);
        self.drop_cache(st, id);
        if let Some(n) = st.nodes.get_mut(&id) {
            n.size = v.size;
            n.mtime = v.mtime;
            n.remote = Some(v.clone());
        }
        true
    }

    /// The server no longer has this item, but there are local changes in it.
    /// Keeps what was changed here (to be uploaded again) and drops the rest.
    /// Returns whether the node was kept.
    pub(super) fn detach_from_server(&self, st: &mut State, id: NodeId) -> bool {
        let Some(n) = st.nodes.get(&id) else { return false };
        let mut keep = n.local_change();
        if n.kind == Kind::Dir {
            let children: Vec<NodeId> = n.children.values().copied().collect();
            for c in children {
                keep |= self.detach_from_server(st, c);
            }
        }
        if !keep {
            self.drop_subtree(st, id);
            return false;
        }
        if let Some(n) = st.nodes.get_mut(&id) {
            n.remote = None;
            match n.kind {
                Kind::File => n.dirty = true,
                Kind::Dir => n.listed = true,
            }
        }
        st.touch(id);
        self.schedule(st, id, Duration::ZERO);
        true
    }

    // ------------------------------------------------------------------
    // Background workers

    /// Keeps pinned files and folders fully downloaded and current.
    pub(super) async fn pinner(self: Arc<Self>) {
        while !self.stopped() {
            if self.is_online() {
                let roots: Vec<NodeId> = self.st().nodes.values().filter(|n| n.pinned && !n.unlinked).map(|n| n.id).collect();
                for root in roots {
                    self.sync_pinned(root).await;
                }
            }
            tokio::select! {
                _ = self.pin_wake.notified() => {}
                _ = tokio::time::sleep(self.opts.poll_interval) => {}
                _ = self.stop_notify.notified() => break,
            }
        }
    }

    async fn sync_pinned(self: &Arc<Self>, root: NodeId) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if self.stopped() || !self.is_online() {
                return;
            }
            enum Todo {
                List,
                Fetch,
                Nothing,
            }
            let (todo, children) = {
                let st = self.st();
                let Some(n) = st.nodes.get(&id) else { continue };
                if n.unlinked || st.pin_of(id) == Pin::No {
                    continue;
                }
                match n.kind {
                    Kind::Dir if n.remote.is_some() && n.listed_at.is_none_or(|t| t.elapsed() >= self.opts.dir_ttl) => (Todo::List, Vec::new()),
                    Kind::Dir => (Todo::Nothing, n.children.values().copied().collect()),
                    Kind::File if !n.complete() && !n.dirty && n.remote.is_some() => (Todo::Fetch, Vec::new()),
                    Kind::File => (Todo::Nothing, Vec::new()),
                }
            };
            match todo {
                Todo::List => {
                    if let Err(e) = self.refresh_dir(id).await {
                        log::debug!("could not refresh a pinned folder: {e}");
                    }
                    if let Some(n) = self.st().nodes.get(&id) {
                        stack.extend(n.children.values().copied());
                    }
                }
                Todo::Fetch => {
                    if let Err(e) = self.ensure_full(id).await {
                        log::warn!("could not download a pinned file: {e}");
                    }
                }
                Todo::Nothing => stack.extend(children),
            }
        }
    }

    /// While offline, keeps knocking until the NAS answers again. While
    /// online, refreshes the free-space figures now and then.
    pub(super) async fn prober(self: Arc<Self>) {
        let mut last_quota: Option<Instant> = None;
        while !self.stopped() {
            if !self.is_online() {
                match self.remote.stat("").await {
                    Err(e) if e.is_offline() => {}
                    _ => self.set_online(true), // it answers (even if with an error)
                }
            } else if last_quota.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                last_quota = Some(Instant::now());
                match self.remote.quota().await {
                    Ok(q) => *self.quota.lock().unwrap() = q,
                    Err(e) => self.note_error(&e),
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                _ = self.stop_notify.notified() => break,
            }
        }
    }

    pub(super) async fn evictor(self: Arc<Self>) {
        while !self.stopped() {
            tokio::select! {
                _ = self.evict_wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = self.stop_notify.notified() => break,
            }
            self.evict_lru();
        }
    }

    /// When the cache is over its limit, drops the least recently used files
    /// that are safe to drop (not pinned, modified, open or in transfer).
    pub(super) fn evict_lru(&self) {
        let mut st = self.st();
        if st.cache_bytes <= self.opts.cache_max {
            return;
        }
        let target = self.opts.cache_max / 10 * 9;
        let mut candidates: Vec<(i64, NodeId)> = st
            .nodes
            .values()
            .filter(|n| n.kind == Kind::File && !n.dirty && !n.unlinked && n.opens == 0 && n.dls.is_empty() && n.upload.is_none() && !n.cached.is_empty())
            .map(|n| (n.atime, n.id))
            .collect();
        candidates.retain(|(_, id)| st.pin_of(*id) == Pin::No);
        candidates.sort_unstable();
        let mut freed = 0;
        for (_, id) in candidates {
            if st.cache_bytes <= target {
                break;
            }
            freed += self.drop_cache(&mut st, id);
        }
        if freed > 0 {
            log::info!("cache over its limit: freed {}", crate::config::format_size(freed));
            self.flush_wake.notify_one();
        }
    }
}
