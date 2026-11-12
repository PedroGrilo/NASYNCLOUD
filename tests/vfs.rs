//! Behaviour of the on-demand engine against a real (in-process) WebDAV server.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{pattern, wait_for, TestServer, PASS, USER};
use nasdrive::vfs::{NodeId, Pin, Vfs, VfsError, VfsOptions, ROOT};
use nasdrive::webdav::WebDav;

fn options(dir: &Path) -> VfsOptions {
    VfsOptions {
        state_dir: dir.to_path_buf(),
        cache_max: 1 << 30,
        dir_ttl: Duration::ZERO,
        poll_interval: Duration::from_secs(1),
        upload_delay: Duration::from_millis(50),
        parallel_uploads: 2,
        readahead: 256 << 10,
        atomic_upload: true,
    }
}

async fn open(srv: &TestServer, dir: &Path, tweak: impl FnOnce(&mut VfsOptions)) -> Arc<Vfs> {
    let mut opts = options(dir);
    tweak(&mut opts);
    Vfs::open(opts, Arc::new(WebDav::new(&srv.url, USER, PASS, false).unwrap())).await.unwrap()
}

async fn id(vfs: &Arc<Vfs>, path: &str) -> NodeId {
    let mut cur = ROOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = vfs.lookup(cur, part).await.unwrap_or_else(|e| panic!("lookup {part:?} in {path:?}: {e}")).id;
    }
    cur
}

async fn names(vfs: &Arc<Vfs>, dir: NodeId) -> Vec<String> {
    vfs.readdir(dir).await.unwrap().into_iter().map(|(n, _)| n).collect()
}

async fn read_all(vfs: &Arc<Vfs>, id: NodeId) -> Vec<u8> {
    let size = vfs.getattr(id).unwrap().size;
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let chunk = vfs.read(id, out.len() as u64, 128 << 10).await.unwrap();
        assert!(!chunk.is_empty());
        out.extend(chunk);
    }
    out
}

/// Creates (or replaces) a file the way an application would.
async fn put(vfs: &Arc<Vfs>, dir: NodeId, name: &str, data: &[u8]) -> NodeId {
    let id = match vfs.lookup(dir, name).await {
        Ok(a) => {
            vfs.open_file(a.id, true).unwrap();
            vfs.truncate(a.id, 0).await.unwrap();
            a.id
        }
        Err(_) => vfs.create(dir, name).await.unwrap().id,
    };
    for (i, chunk) in data.chunks(100_000).enumerate() {
        assert_eq!(vfs.write(id, (i * 100_000) as u64, chunk).await.unwrap() as usize, chunk.len());
    }
    vfs.release(id, true);
    id
}

/// Makes the engine notice whether the server is reachable.
async fn poke(vfs: &Arc<Vfs>) {
    let _ = vfs.refresh(ROOT).await;
}

fn assert_send<T: Send>(t: T) -> T {
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_on_demand_and_caches() {
    let srv = TestServer::start().await;
    let big = pattern(3 << 20, 1);
    srv.write("docs/big.bin", &big);
    srv.write("docs/small.txt", b"hello");
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;

    assert_eq!(names(&vfs, ROOT).await, ["docs"]);
    let docs = id(&vfs, "docs").await;
    let listing = vfs.readdir(docs).await.unwrap();
    assert_eq!(listing.iter().map(|(n, a)| (n.as_str(), a.size, a.local_bytes)).collect::<Vec<_>>(), [("big.bin", 3 << 20, 0), ("small.txt", 5, 0)]);
    assert_eq!(srv.count("GET"), 0, "browsing must not download anything");

    // A read in the middle fetches only that neighbourhood.
    let big_id = id(&vfs, "docs/big.bin").await;
    vfs.open_file(big_id, false).unwrap();
    assert_eq!(assert_send(vfs.read(big_id, 1 << 20, 4096)).await.unwrap(), &big[1 << 20..(1 << 20) + 4096]);
    let st = vfs.status(big_id).unwrap();
    assert_eq!(st.state, "partial");
    assert!(st.cached >= 4096 && st.cached < 2 << 20, "cached {}", st.cached);

    // Reading it all gives the right bytes and ends up fully cached.
    assert_eq!(read_all(&vfs, big_id).await, big);
    assert!(wait_for(5, || vfs.status(big_id).unwrap().state == "cached").await);
    assert_eq!(vfs.getattr(big_id).unwrap().local_bytes, 3 << 20);
    srv.clear_log();
    assert_eq!(read_all(&vfs, big_id).await, big);
    assert_eq!(srv.count("GET"), 0, "cached data must not be fetched again");
    vfs.release(big_id, false);

    let small = id(&vfs, "docs/small.txt").await;
    assert_eq!(vfs.read(small, 0, 100).await.unwrap(), b"hello");
    assert_eq!(vfs.read(small, 5, 100).await.unwrap(), b"");
    assert!(matches!(vfs.lookup(docs, "nope").await, Err(VfsError::NotFound)));
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_are_uploaded_in_the_background() {
    let srv = TestServer::start().await;
    let existing = pattern(200_000, 2);
    srv.write("existing.bin", &existing);
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;

    // New file.
    let data = pattern(350_000, 3);
    let f = put(&vfs, ROOT, "novo ficheiro.bin", &data).await;
    assert_eq!(read_all(&vfs, f).await, data);
    assert!(wait_for(10, || srv.exists("novo ficheiro.bin") && srv.read("novo ficheiro.bin") == data).await);
    assert!(wait_for(5, || vfs.status(f).unwrap().state == "cached" && vfs.pending_uploads() == 0).await);

    // New folders, nested, with a file inside.
    let a = vfs.mkdir(ROOT, "a").await.unwrap().id;
    let b = assert_send(vfs.mkdir(a, "b")).await.unwrap().id;
    put(&vfs, b, "f.txt", b"deep").await;
    assert!(wait_for(10, || srv.exists("a/b/f.txt") && srv.read("a/b/f.txt") == b"deep").await);

    // Editing an existing file in place downloads it first, then uploads the result.
    let e = id(&vfs, "existing.bin").await;
    vfs.open_file(e, true).unwrap();
    assert_eq!(assert_send(vfs.write(e, 100, b"XYZ")).await.unwrap(), 3);
    vfs.release(e, true);
    let mut expected = existing.clone();
    expected[100..103].copy_from_slice(b"XYZ");
    assert!(wait_for(10, || srv.read("existing.bin") == expected).await);

    // Replacing content (truncate + write) and an empty file.
    put(&vfs, ROOT, "existing.bin", b"short now").await;
    assert!(wait_for(10, || srv.read("existing.bin") == b"short now").await);
    put(&vfs, ROOT, "empty", b"").await;
    assert!(wait_for(10, || srv.exists("empty")).await);

    assert!(wait_for(5, || vfs.pending_uploads() == 0).await);
    assert!(srv.names("").iter().all(|n| !n.contains("nasdrive-part")), "{:?}", srv.names(""));
    // Junk files stay local.
    put(&vfs, ROOT, ".DS_Store", b"junk").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!srv.exists(".DS_Store"));
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn works_offline_and_syncs_when_back() {
    let srv = TestServer::start().await;
    srv.write("a.txt", b"cached");
    srv.write("b.txt", b"not cached");
    srv.write("old/keep.txt", b"k");
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;
    let a = id(&vfs, "a.txt").await;
    let b = id(&vfs, "b.txt").await;
    let old = id(&vfs, "old").await;
    assert_eq!(read_all(&vfs, a).await, b"cached");

    srv.set_down(true);
    assert!(matches!(vfs.read(b, 0, 10).await, Err(VfsError::Offline(_))));
    assert!(!vfs.is_online());
    assert_eq!(read_all(&vfs, a).await, b"cached");
    // Browsing what we already know still works; unknown folders cannot be listed.
    assert_eq!(names(&vfs, ROOT).await, ["a.txt", "b.txt", "old"]);
    assert!(vfs.readdir(old).await.is_err());

    // Changes made offline are queued...
    let c = put(&vfs, ROOT, "c.txt", b"made offline").await;
    let d = vfs.mkdir(ROOT, "d").await.unwrap().id;
    put(&vfs, d, "e.txt", b"nested").await;
    put(&vfs, ROOT, "a.txt", b"edited offline").await;
    // ...including the "write a temp file and rename it over the original" dance.
    put(&vfs, ROOT, "b.txt.tmp", b"saved via rename").await;
    vfs.rename(ROOT, "b.txt.tmp", ROOT, "b.txt", false).await.unwrap();
    assert_eq!(vfs.status(c).unwrap().state, "modified");
    assert_eq!(vfs.pending_uploads(), 5);
    // Deleting something that lives on the server needs the server.
    assert!(matches!(vfs.unlink(old, "keep.txt").await, Err(VfsError::Offline(_)) | Err(VfsError::NotFound)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!srv.exists("c.txt"));

    srv.set_down(false);
    poke(&vfs).await;
    assert!(wait_for(10, || vfs.pending_uploads() == 0).await, "{:?}", vfs.info());
    assert_eq!(srv.read("c.txt"), b"made offline");
    assert_eq!(srv.read("d/e.txt"), b"nested");
    assert_eq!(srv.read("a.txt"), b"edited offline");
    assert_eq!(srv.read("b.txt"), b"saved via rename");
    assert_eq!(srv.names(""), ["a.txt", "b.txt", "c.txt", "d", "old"]);
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_changes_and_conflicts() {
    let srv = TestServer::start().await;
    srv.write("f.txt", b"v1");
    srv.write("gone.txt", b"bye");
    srv.write("h.txt", b"hhh");
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;
    let f = id(&vfs, "f.txt").await;
    assert_eq!(read_all(&vfs, f).await, b"v1");
    let h = id(&vfs, "h.txt").await;
    assert_eq!(read_all(&vfs, h).await, b"hhh");
    id(&vfs, "gone.txt").await;

    // Someone else changes and deletes files on the NAS.
    srv.write("f.txt", b"version2");
    std::fs::remove_file(srv.path("gone.txt")).unwrap();
    srv.write("new.txt", b"n");
    vfs.refresh(ROOT).await.unwrap();
    assert_eq!(names(&vfs, ROOT).await, ["f.txt", "h.txt", "new.txt"]);
    assert_eq!(vfs.status(f).unwrap().state, "online", "stale cache must be dropped");
    assert_eq!(read_all(&vfs, f).await, b"version2");

    // Both sides change the same file while we are offline: keep both.
    srv.set_down(true);
    poke(&vfs).await;
    put(&vfs, ROOT, "f.txt", b"local edit").await;
    put(&vfs, ROOT, "h.txt", b"edited here, deleted there").await;
    srv.write("f.txt", b"remote edit!!");
    std::fs::remove_file(srv.path("h.txt")).unwrap();
    srv.set_down(false);
    poke(&vfs).await;
    assert!(wait_for(10, || vfs.pending_uploads() == 0).await, "{:?}", vfs.info());
    let on_server = srv.names("");
    let conflict = on_server.iter().find(|n| n.starts_with("f (conflict ") && n.ends_with(").txt")).unwrap_or_else(|| panic!("no conflict copy in {on_server:?}"));
    assert_eq!(srv.read(conflict), b"local edit");
    assert_eq!(srv.read("f.txt"), b"remote edit!!");
    assert_eq!(srv.read("h.txt"), b"edited here, deleted there", "an edit wins over a deletion");
    vfs.refresh(ROOT).await.unwrap();
    assert_eq!(names(&vfs, ROOT).await, [conflict.as_str(), "f.txt", "h.txt", "new.txt"]);
    let f2 = id(&vfs, "f.txt").await;
    assert_eq!(read_all(&vfs, f2).await, b"remote edit!!");
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinning_and_eviction() {
    let srv = TestServer::start().await;
    for i in 0..3 {
        srv.write(&format!("p/file{i}.bin"), &pattern(100_000, i));
    }
    srv.write("p/s/deep.bin", &pattern(50_000, 9));
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;
    let p = id(&vfs, "p").await;

    // Pinning a folder downloads everything below it, including later arrivals.
    vfs.pin(p, true).unwrap();
    assert!(wait_for(10, || { let s = vfs.status(p).unwrap(); s.files == 4 && s.files_cached == 4 }).await, "{:?}", vfs.status(p));
    assert_eq!(vfs.status(p).unwrap().pinned, Pin::Yes);
    let deep = id(&vfs, "p/s/deep.bin").await;
    assert_eq!(vfs.status(deep).unwrap().pinned, Pin::Inherited);
    srv.write("p/late.bin", &pattern(70_000, 5));
    assert!(wait_for(10, || { let s = vfs.status(p).unwrap(); s.files == 5 && s.files_cached == 5 }).await, "the poller should pick up new files: {:?}", vfs.status(p));
    srv.clear_log();
    assert_eq!(read_all(&vfs, deep).await, pattern(50_000, 9));
    assert_eq!(srv.count("GET"), 0);

    // "Free up space" on something pinned by a parent is refused; on the folder it unpins and frees.
    assert!(matches!(vfs.evict(deep), Err(VfsError::Busy(_))));
    assert_eq!(vfs.evict(p).unwrap(), 420_000);
    let s = vfs.status(p).unwrap();
    assert_eq!((s.cached, s.files_cached, s.pinned), (0, 0, Pin::No));
    assert_eq!(vfs.info().cache_bytes, 0);
    vfs.shutdown().await;
    drop(vfs);

    // With a small cache limit the least recently used file makes room.
    let dir2 = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir2.path(), |o| o.cache_max = 150_000).await;
    let f0 = id(&vfs, "p/file0.bin").await;
    let f1 = id(&vfs, "p/file1.bin").await;
    read_all(&vfs, f0).await;
    tokio::time::sleep(Duration::from_millis(1100)).await; // access times have 1s resolution
    read_all(&vfs, f1).await;
    assert!(wait_for(10, || vfs.info().cache_bytes <= 150_000).await, "{:?}", vfs.info());
    assert_eq!(vfs.status(f0).unwrap().state, "online");
    assert_eq!(vfs.status(f1).unwrap().state, "cached");
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_and_delete() {
    let srv = TestServer::start().await;
    srv.write("r/a.txt", b"A");
    srv.write("r/b.txt", b"B");
    let dir = tempfile::tempdir().unwrap();
    let vfs = open(&srv, dir.path(), |_| {}).await;
    let r = id(&vfs, "r").await;
    id(&vfs, "r/a.txt").await;

    assert_send(vfs.rename(r, "a.txt", r, "c.txt", false)).await.unwrap();
    assert_eq!(srv.names("r"), ["b.txt", "c.txt"]);
    assert!(matches!(vfs.rename(r, "c.txt", r, "b.txt", true).await, Err(VfsError::Exists)));
    vfs.rename(r, "c.txt", r, "b.txt", false).await.unwrap();
    assert_eq!(srv.names("r"), ["b.txt"]);
    assert_eq!(srv.read("r/b.txt"), b"A");
    assert_eq!(names(&vfs, r).await, ["b.txt"]);

    // Moving into a folder that only exists locally creates it on the way.
    let fresh = vfs.mkdir(ROOT, "fresh").await.unwrap().id;
    vfs.rename(r, "b.txt", fresh, "moved.txt", false).await.unwrap();
    assert_eq!(srv.read("fresh/moved.txt"), b"A");

    // Renaming a folder takes its content along, uploaded or not.
    put(&vfs, fresh, "pending.txt", b"P").await;
    vfs.rename(ROOT, "fresh", ROOT, "final", false).await.unwrap();
    assert!(wait_for(10, || vfs.pending_uploads() == 0).await, "{:?}", vfs.info());
    assert_eq!(srv.names("final"), ["moved.txt", "pending.txt"]);
    assert!(!srv.exists("fresh"));
    let fin = id(&vfs, "final").await;

    assert_send(vfs.unlink(fin, "moved.txt")).await.unwrap();
    assert!(!srv.exists("final/moved.txt"));
    assert!(matches!(vfs.rmdir(ROOT, "final").await, Err(VfsError::NotEmpty)));
    vfs.unlink(fin, "pending.txt").await.unwrap();
    // The server got a file we have not seen: the folder must not be wiped.
    srv.write("final/surprise.txt", b"!");
    assert!(matches!(assert_send(vfs.rmdir(ROOT, "final")).await, Err(VfsError::NotEmpty)));
    assert_eq!(names(&vfs, fin).await, ["surprise.txt"]);
    vfs.unlink(fin, "surprise.txt").await.unwrap();
    vfs.rmdir(ROOT, "final").await.unwrap();
    assert!(!srv.exists("final"));

    // A file deleted before it was ever uploaded leaves nothing behind.
    let t = vfs.create(r, "temp.swp").await.unwrap().id;
    vfs.write(t, 0, b"x").await.unwrap();
    vfs.unlink(r, "temp.swp").await.unwrap();
    vfs.release(t, true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(srv.names("r"), Vec::<String>::new());
    assert_eq!(vfs.pending_uploads(), 0);
    vfs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_survives_a_restart() {
    let srv = TestServer::start().await;
    let data = pattern(500_000, 4);
    srv.write("keep/data.bin", &data);
    let dir = tempfile::tempdir().unwrap();
    {
        let vfs = open(&srv, dir.path(), |_| {}).await;
        let d = id(&vfs, "keep/data.bin").await;
        assert_eq!(read_all(&vfs, d).await, data);
        vfs.pin(id(&vfs, "keep").await, true).unwrap();
        srv.set_down(true);
        poke(&vfs).await;
        put(&vfs, ROOT, "pending.txt", b"written before the restart").await;
        vfs.shutdown().await;
        assert!(wait_for(5, || Arc::strong_count(&vfs) == 1).await);
    }
    // Start again with the NAS still unreachable: everything known is there.
    srv.clear_log();
    let vfs = open(&srv, dir.path(), |_| {}).await;
    assert_eq!(names(&vfs, ROOT).await, ["keep", "pending.txt"]);
    let d = id(&vfs, "keep/data.bin").await;
    assert_eq!(vfs.status(d).unwrap().state, "cached");
    assert_eq!(vfs.status(d).unwrap().pinned, Pin::Inherited);
    assert_eq!(read_all(&vfs, d).await, data);
    let p = id(&vfs, "pending.txt").await;
    assert_eq!(read_all(&vfs, p).await, b"written before the restart");
    assert_eq!(vfs.pending_uploads(), 1);
    assert_eq!(srv.count("GET"), 0);

    srv.set_down(false);
    poke(&vfs).await;
    assert!(wait_for(10, || srv.exists("pending.txt") && vfs.pending_uploads() == 0).await, "{:?}", vfs.info());
    assert_eq!(srv.read("pending.txt"), b"written before the restart");
    vfs.shutdown().await;
}
