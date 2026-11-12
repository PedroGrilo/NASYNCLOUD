//! WebDAV client against a real (in-process) WebDAV server.

mod common;

use common::{pattern, TestServer, PASS, USER};
use futures_util::StreamExt;
use nasdrive::remote::{Remote, RemoteError};
use nasdrive::webdav::WebDav;

fn client(url: &str) -> WebDav {
    WebDav::new(url, USER, PASS, false).unwrap()
}

async fn read_all(dav: &WebDav, path: &str, start: u64, end: Option<u64>) -> Vec<u8> {
    let mut dl = dav.open_range(path, start, end).await.unwrap();
    assert_eq!(dl.skip, 0);
    let mut out = Vec::new();
    while let Some(chunk) = dl.stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn list_stat_and_names_with_odd_characters() {
    let srv = TestServer::start().await;
    srv.write("Férias 2026/a & b #1?.txt", b"hello");
    srv.write("Férias 2026/sub/x", b"");
    srv.write("top.bin", &pattern(1000, 1));
    let dav = client(&srv.url);

    let mut root = dav.list("").await.unwrap();
    root.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(root.iter().map(|e| (e.name.as_str(), e.is_dir, e.version.size)).collect::<Vec<_>>(), vec![("Férias 2026", true, 0), ("top.bin", false, 1000)]);
    assert!(root[1].version.mtime > 1_700_000_000);

    let mut sub = dav.list("Férias 2026").await.unwrap();
    sub.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(sub.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["a & b #1?.txt", "sub"]);

    let st = dav.stat("Férias 2026/a & b #1?.txt").await.unwrap().unwrap();
    assert_eq!((st.name.as_str(), st.is_dir, st.version.size), ("a & b #1?.txt", false, 5));
    assert!(dav.stat("Férias 2026").await.unwrap().unwrap().is_dir);
    assert!(dav.stat("nope").await.unwrap().is_none());
    assert!(matches!(dav.list("nope").await, Err(RemoteError::NotFound)));

    // A base URL pointing at a sub-folder works the same way.
    let sub_dav = client(&format!("{}/F%C3%A9rias%202026", srv.url));
    assert_eq!(sub_dav.list("").await.unwrap().len(), 2);
    assert_eq!(sub_dav.stat("sub/x").await.unwrap().unwrap().version.size, 0);
}

#[tokio::test]
async fn ranged_downloads() {
    let srv = TestServer::start().await;
    let data = pattern(300_000, 7);
    srv.write("big.bin", &data);
    let dav = client(&srv.url);
    assert_eq!(read_all(&dav, "big.bin", 0, None).await, data);
    assert_eq!(read_all(&dav, "big.bin", 1000, Some(2000)).await, &data[1000..2000]);
    assert_eq!(read_all(&dav, "big.bin", 299_990, None).await, &data[299_990..]);
    let dl = dav.open_range("big.bin", 10, Some(20)).await.unwrap();
    assert_eq!(dl.total, Some(300_000));
    assert!(matches!(dav.open_range("missing", 0, None).await, Err(RemoteError::NotFound)));
}

#[tokio::test]
async fn upload_mkdir_rename_delete() {
    let srv = TestServer::start().await;
    let dav = client(&srv.url);
    let local = tempfile::tempdir().unwrap();
    let data = pattern(700_000, 3);
    let src = local.path().join("src");
    std::fs::write(&src, &data).unwrap();

    dav.mkdir("docs").await.unwrap();
    dav.mkdir("docs").await.unwrap(); // idempotent
    dav.mkdir("docs/deep").await.unwrap();
    assert!(matches!(dav.mkdir("no/parent").await, Err(RemoteError::Precondition(_))));

    dav.upload("docs/deep/ficheiro grande.bin", &src, data.len() as u64, 1_700_000_000).await.unwrap();
    assert_eq!(srv.read("docs/deep/ficheiro grande.bin"), data);
    // Only the first `size` bytes are sent, and empty files work.
    dav.upload("docs/part.bin", &src, 10, 0).await.unwrap();
    assert_eq!(srv.read("docs/part.bin"), &data[..10]);
    dav.upload("docs/empty", &src, 0, 0).await.unwrap();
    assert_eq!(srv.read("docs/empty"), b"");

    dav.rename("docs/part.bin", "docs/renamed.bin", false, false).await.unwrap();
    assert!(!srv.exists("docs/part.bin") && srv.exists("docs/renamed.bin"));
    assert!(matches!(dav.rename("docs/renamed.bin", "docs/empty", false, false).await, Err(RemoteError::Precondition(_))));
    dav.rename("docs/renamed.bin", "docs/empty", false, true).await.unwrap();
    assert_eq!(srv.read("docs/empty"), &data[..10]);
    assert!(matches!(dav.rename("docs/ghost", "docs/x", false, false).await, Err(RemoteError::NotFound)));
    dav.rename("docs/deep", "moved", true, false).await.unwrap();
    assert!(srv.exists("moved/ficheiro grande.bin"));

    dav.delete("docs/empty", false).await.unwrap();
    dav.delete("docs/empty", false).await.unwrap(); // already gone is fine
    dav.delete("moved", true).await.unwrap();
    assert!(!srv.exists("moved"));
    assert!(dav.delete("", true).await.is_err());
}

#[tokio::test]
async fn auth_and_connectivity_errors() {
    let srv = TestServer::start().await;
    let bad = WebDav::new(&srv.url, USER, "wrong", false).unwrap();
    assert!(matches!(bad.list("").await, Err(RemoteError::Unauthorized)));

    let dav = client(&srv.url);
    dav.list("").await.unwrap();
    srv.set_down(true);
    let err = dav.list("").await.unwrap_err();
    assert!(err.is_offline(), "{err:?}");
    let nowhere = client("http://127.0.0.1:9");
    assert!(nowhere.list("").await.unwrap_err().is_offline());
}

/// The NAS must receive a Content-Length (not chunked encoding): some WebDAV
/// servers reject chunked uploads.
#[tokio::test]
async fn uploads_send_content_length() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
        sock.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        head
    });
    let local = tempfile::tempdir().unwrap();
    let src = local.path().join("f");
    std::fs::write(&src, pattern(5000, 9)).unwrap();
    client(&url).upload("f", &src, 5000, 0).await.unwrap();
    let head = server.await.unwrap();
    assert!(head.contains("content-length: 5000"), "{head}");
    assert!(!head.contains("transfer-encoding"), "{head}");
    assert!(head.starts_with("put /f http/1.1"), "{head}");
}
