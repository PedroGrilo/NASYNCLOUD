//! WebDAV backend. Works with UGOS (default ports 5005 http / 5006 https) and
//! any other reasonably standard WebDAV server.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::StreamExt;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, RANGE};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::remote::{Download, Entry, Quota, Remote, RemoteError, Version};

/// Everything except RFC 3986 "unreserved" characters gets percent-encoded.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

const META_TIMEOUT: Duration = Duration::from_secs(60);
const SLOW_META_TIMEOUT: Duration = Duration::from_secs(300);
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_STALL: Duration = Duration::from_secs(120);

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:getetag/></D:prop></D:propfind>"#;

const QUOTA_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:quota-available-bytes/><D:quota-used-bytes/></D:prop></D:propfind>"#;

pub struct WebDav {
    client: reqwest::Client,
    /// Base URL, always ending in `/`.
    base: String,
    /// Decoded path component of the base URL, without trailing slash.
    base_path: String,
    username: String,
    password: String,
}

impl WebDav {
    pub fn new(url: &str, username: &str, password: &str, accept_invalid_certs: bool) -> Result<Self, RemoteError> {
        let parsed = reqwest::Url::parse(url.trim()).map_err(|e| RemoteError::Other(format!("invalid URL {url:?}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(RemoteError::Other(format!("unsupported URL scheme {:?} (use http:// or https://)", parsed.scheme())));
        }
        let mut base = parsed.as_str().split(['?', '#']).next().unwrap_or_default().to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base_path = percent_decode_str(parsed.path()).decode_utf8_lossy().trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .user_agent(concat!("nasyncloud/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .tcp_keepalive(Duration::from_secs(30))
            // We deal with redirects ourselves: following one would turn PROPFIND into GET.
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(accept_invalid_certs)
            .build()
            .map_err(|e| RemoteError::Other(format!("cannot create the HTTP client: {}", chain(&e))))?;
        Ok(Self { client, base, base_path, username: username.to_string(), password: password.to_string() })
    }

    fn url(&self, path: &str, dir: bool) -> String {
        let mut url = self.base.clone();
        for (i, seg) in path.split('/').filter(|s| !s.is_empty()).enumerate() {
            if i > 0 {
                url.push('/');
            }
            url.extend(utf8_percent_encode(seg, SEGMENT));
        }
        if dir && !url.ends_with('/') {
            url.push('/');
        }
        url
    }

    fn request(&self, method: &str, url: &str) -> RequestBuilder {
        let method = Method::from_bytes(method.as_bytes()).expect("valid HTTP method");
        self.client.request(method, url).basic_auth(&self.username, Some(&self.password))
    }

    /// Path (decoded, no trailing slash) the server should report for `path`.
    fn expected_href(&self, path: &str) -> String {
        if path.is_empty() {
            self.base_path.clone()
        } else {
            format!("{}/{}", self.base_path, path.trim_matches('/'))
        }
    }

    async fn propfind(&self, url: &str, depth: &str, body: &'static str) -> Result<(StatusCode, String), RemoteError> {
        let rb = self.request("PROPFIND", url).header("Depth", depth).header(CONTENT_TYPE, "application/xml; charset=utf-8").body(body);
        let resp = send(rb, META_TIMEOUT).await?;
        let status = resp.status();
        let text = tokio::time::timeout(META_TIMEOUT, resp.text()).await.map_err(|_| timed_out("PROPFIND"))?.map_err(net)?;
        Ok((status, text))
    }
}

#[async_trait]
impl Remote for WebDav {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, RemoteError> {
        let (status, text) = self.propfind(&self.url(dir, true), "1", PROPFIND_BODY).await?;
        if status != StatusCode::MULTI_STATUS {
            return Err(status_error(status, &text));
        }
        let (_, children) = split_self(parse_multistatus(&text)?, &self.expected_href(dir));
        Ok(children)
    }

    async fn stat(&self, path: &str) -> Result<Option<Entry>, RemoteError> {
        let mut url = self.url(path, false);
        for attempt in 0..2 {
            let (status, text) = self.propfind(&url, "0", PROPFIND_BODY).await?;
            if status == StatusCode::MULTI_STATUS {
                let (this, mut others) = split_self(parse_multistatus(&text)?, &self.expected_href(path));
                let mut entry = match this.or_else(|| others.pop()) {
                    Some(e) => e,
                    None => return Ok(None),
                };
                entry.name = crate::remote::split(path).1.to_string();
                return Ok(Some(entry));
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            // Some servers redirect a folder URL without trailing slash.
            if status.is_redirection() && attempt == 0 && !url.ends_with('/') {
                url.push('/');
                continue;
            }
            return Err(status_error(status, &text));
        }
        Ok(None)
    }

    async fn open_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Download, RemoteError> {
        let range = match end {
            Some(end) => format!("bytes={start}-{}", end.saturating_sub(1)),
            None => format!("bytes={start}-"),
        };
        let resp = send(self.request("GET", &self.url(path, false)).header(RANGE, range), HEADER_TIMEOUT).await?;
        let status = resp.status();
        let headers = resp.headers();
        let etag = headers.get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let (skip, total) = match status {
            StatusCode::PARTIAL_CONTENT => {
                let total = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit('/').next()).and_then(|t| t.trim().parse().ok());
                (0, total)
            }
            // The server ignored the Range header and is sending the whole file.
            StatusCode::OK => (start, headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())),
            _ => return Err(status_error(status, &body_snippet(resp).await)),
        };
        Ok(Download { etag, total, skip, stream: Box::pin(resp.bytes_stream().map(|r| r.map_err(net))) })
    }

    async fn upload(&self, path: &str, source: &Path, size: u64, mtime: i64) -> Result<(), RemoteError> {
        let file = tokio::fs::File::open(source).await.map_err(|e| RemoteError::Other(format!("cannot open the local copy {}: {e}", source.display())))?;
        // Progress clock for the stall watchdog below.
        let started = Instant::now();
        let last_progress = Arc::new(AtomicU64::new(0));
        let lp = last_progress.clone();
        let stream = ReaderStream::with_capacity(file.take(size), 256 * 1024).inspect(move |_| lp.store(started.elapsed().as_millis() as u64, Ordering::Relaxed));
        let rb = self
            .request("PUT", &self.url(path, false))
            .header(CONTENT_LENGTH, size)
            .header(CONTENT_TYPE, "application/octet-stream")
            // Honoured by ownCloud/Nextcloud-style servers, ignored by the rest.
            .header("X-OC-Mtime", mtime.max(0))
            .body(reqwest::Body::wrap_stream(stream));
        let watchdog = async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let idle = started.elapsed().saturating_sub(Duration::from_millis(last_progress.load(Ordering::Relaxed)));
                if idle > UPLOAD_STALL {
                    break;
                }
            }
        };
        let resp = tokio::select! {
            r = rb.send() => r.map_err(net)?,
            _ = watchdog => return Err(RemoteError::Network("upload stalled".into())),
        };
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(status_error(status, &body_snippet(resp).await))
        }
    }

    async fn mkdir(&self, path: &str) -> Result<(), RemoteError> {
        let resp = send(self.request("MKCOL", &self.url(path, false)), META_TIMEOUT).await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::CONFLICT {
            return Err(RemoteError::Precondition("the parent folder does not exist on the server".into()));
        }
        // 405 & co. usually mean "already there"; some servers insist on a trailing slash.
        match self.stat(path).await {
            Ok(Some(e)) if e.is_dir => return Ok(()),
            Ok(Some(_)) => return Err(RemoteError::Precondition("a file with that name already exists on the server".into())),
            _ => {}
        }
        let resp = send(self.request("MKCOL", &self.url(path, true)), META_TIMEOUT).await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            Err(status_error(status, &body_snippet(resp).await))
        }
    }

    async fn delete(&self, path: &str, is_dir: bool) -> Result<(), RemoteError> {
        if path.is_empty() {
            return Err(RemoteError::Other("refusing to delete the root folder".into()));
        }
        let resp = send(self.request("DELETE", &self.url(path, is_dir)), SLOW_META_TIMEOUT).await?;
        let status = resp.status();
        if status == StatusCode::MULTI_STATUS {
            return Err(RemoteError::Other(format!("the server could not delete everything: {}", body_snippet(resp).await)));
        }
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_error(status, &body_snippet(resp).await))
        }
    }

    async fn rename(&self, from: &str, to: &str, is_dir: bool, overwrite: bool) -> Result<(), RemoteError> {
        let rb = self.request("MOVE", &self.url(from, is_dir)).header("Destination", self.url(to, is_dir)).header("Overwrite", if overwrite { "T" } else { "F" });
        let resp = send(rb, SLOW_META_TIMEOUT).await?;
        let status = resp.status();
        if status == StatusCode::MULTI_STATUS {
            return Err(RemoteError::Other(format!("the server could not move everything: {}", body_snippet(resp).await)));
        }
        match status.as_u16() {
            s if (200..300).contains(&s) => Ok(()),
            412 => Err(RemoteError::Precondition("the destination already exists on the server".into())),
            409 => Err(RemoteError::Precondition("the destination folder does not exist on the server".into())),
            _ => Err(status_error(status, &body_snippet(resp).await)),
        }
    }

    async fn quota(&self) -> Result<Option<Quota>, RemoteError> {
        let (status, text) = self.propfind(&self.url("", true), "0", QUOTA_BODY).await?;
        if status != StatusCode::MULTI_STATUS {
            return Ok(None);
        }
        let Ok(doc) = roxmltree::Document::parse(clean_xml(&text)) else { return Ok(None) };
        let prop = |name: &str| doc.descendants().find(|n| n.is_element() && n.tag_name().name() == name).and_then(|n| n.text()).and_then(|t| t.trim().parse::<u64>().ok());
        Ok(match (prop("quota-used-bytes"), prop("quota-available-bytes")) {
            (Some(used), Some(available)) => Some(Quota { used, available }),
            _ => None,
        })
    }
}

async fn send(rb: RequestBuilder, limit: Duration) -> Result<Response, RemoteError> {
    tokio::time::timeout(limit, rb.send()).await.map_err(|_| timed_out("request"))?.map_err(net)
}

fn timed_out(what: &str) -> RemoteError {
    RemoteError::Network(format!("{what} timed out"))
}

/// Transport-level failure: connection refused, DNS, TLS, reset, timeout...
fn net(e: reqwest::Error) -> RemoteError {
    RemoteError::Network(chain(&e))
}

/// Error message including its causes (reqwest hides the interesting part there).
fn chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

async fn body_snippet(resp: Response) -> String {
    let text = tokio::time::timeout(Duration::from_secs(10), resp.text()).await.ok().and_then(Result::ok).unwrap_or_default();
    text.trim().chars().take(200).collect()
}

fn status_error(status: StatusCode, body: &str) -> RemoteError {
    match status.as_u16() {
        401 => RemoteError::Unauthorized,
        403 => RemoteError::Forbidden,
        404 | 410 => RemoteError::NotFound,
        405 | 409 | 412 | 423 => RemoteError::Precondition(format!("HTTP {status}")),
        416 => RemoteError::Changed,
        507 => RemoteError::Other("no space left on the server".into()),
        code => RemoteError::Http(code, body.trim().chars().take(200).collect()),
    }
}

fn clean_xml(text: &str) -> &str {
    text.trim_start_matches('\u{feff}').trim()
}

/// Parses a `207 Multi-Status` body into `(decoded href path, entry)` pairs.
///
/// Namespaces prefixes vary wildly between servers (`D:`, `d:`, `lp1:`, none),
/// so elements are matched on their local name only.
fn parse_multistatus(text: &str) -> Result<Vec<(String, Entry)>, RemoteError> {
    let doc = roxmltree::Document::parse(clean_xml(text)).map_err(|e| RemoteError::Other(format!("unexpected answer from the server (not WebDAV XML): {e}")))?;
    let is = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name);
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| is(n, "response")) {
        let Some(href) = resp.children().find(|n| is(n, "href")).and_then(|n| n.text()) else { continue };
        let path = href_path(href);
        let mut entry = Entry { name: path.rsplit('/').next().unwrap_or_default().to_string(), is_dir: href.trim().ends_with('/'), version: Version::default() };
        for propstat in resp.children().filter(|n| is(n, "propstat")) {
            // Properties the server does not have come back in a 404 propstat.
            let ok = propstat.children().find(|n| is(n, "status")).and_then(|n| n.text()).is_none_or(|s| s.contains(" 200"));
            if !ok {
                continue;
            }
            for prop in propstat.children().filter(|n| is(n, "prop")).flat_map(|p| p.children()) {
                let text = prop.text().map(str::trim).filter(|t| !t.is_empty());
                match prop.tag_name().name().to_ascii_lowercase().as_str() {
                    "resourcetype" => entry.is_dir = prop.children().any(|n| is(&n, "collection")),
                    "getcontentlength" => entry.version.size = text.and_then(|t| t.parse().ok()).unwrap_or(0),
                    "getetag" => entry.version.etag = text.map(str::to_string),
                    "getlastmodified" => {
                        entry.version.mtime = text
                            .and_then(|t| httpdate::parse_http_date(t).ok())
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_secs() as i64)
                    }
                    _ => {}
                }
            }
        }
        if entry.is_dir {
            entry.version.size = 0;
        }
        out.push((path, entry));
    }
    Ok(out)
}

/// Decoded path of an href, without scheme/host and without trailing slash.
fn href_path(href: &str) -> String {
    let href = href.trim();
    let path = match href.find("://") {
        Some(i) => href[i + 3..].find('/').map_or("", |j| &href[i + 3 + j..]),
        None => href,
    };
    percent_decode_str(path).decode_utf8_lossy().trim_end_matches('/').to_string()
}

/// Separates the entry describing the requested resource itself from its children.
fn split_self(mut all: Vec<(String, Entry)>, expected: &str) -> (Option<Entry>, Vec<Entry>) {
    let expected = expected.trim_end_matches('/');
    let idx = match all.iter().position(|(p, _)| p == expected) {
        Some(i) => Some(i),
        // Behind a reverse proxy the paths may not match ours: by convention the
        // resource itself comes first.
        None if !all.is_empty() => Some(0),
        None => None,
    };
    let this = idx.map(|i| all.remove(i).1);
    let children = all.into_iter().map(|(_, e)| e).filter(|e| !e.name.is_empty() && e.name != "." && e.name != ".." && !e.name.contains('\0')).collect();
    (this, children)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dav(url: &str) -> WebDav {
        WebDav::new(url, "u", "p", false).unwrap()
    }

    #[test]
    fn builds_encoded_urls() {
        let d = dav("http://nas:5005/Fotos");
        assert_eq!(d.url("", true), "http://nas:5005/Fotos/");
        assert_eq!(d.url("a b/ç#?.txt", false), "http://nas:5005/Fotos/a%20b/%C3%A7%23%3F.txt");
        assert_eq!(d.url("x/y", true), "http://nas:5005/Fotos/x/y/");
        assert_eq!(d.expected_href("x/y"), "/Fotos/x/y");
        assert_eq!(dav("https://nas:5006").expected_href(""), "");
        assert!(WebDav::new("ftp://nas", "u", "p", false).is_err());
    }

    #[test]
    fn parses_go_style_listing() {
        // What Go's x/net/webdav (and most NAS firmwares) answer.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?><D:multistatus xmlns:D="DAV:">
<D:response><D:href>/Fotos/</D:href><D:propstat><D:prop><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype><D:getlastmodified>Wed, 11 Nov 2026 10:00:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/Fotos/F%C3%A9rias%202026/</D:href><D:propstat><D:prop><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/Fotos/a%20%26%20b.jpg</D:href><D:propstat><D:prop><D:resourcetype></D:resourcetype><D:getcontentlength>1234</D:getcontentlength><D:getlastmodified>Wed, 11 Nov 2026 10:00:00 GMT</D:getlastmodified><D:getetag>"18a5c3e4d2"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#;
        let (this, children) = split_self(parse_multistatus(xml).unwrap(), "/Fotos");
        assert!(this.unwrap().is_dir);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "Férias 2026");
        assert!(children[0].is_dir);
        assert_eq!(children[1].name, "a & b.jpg");
        assert_eq!(children[1].version, Version { etag: Some("\"18a5c3e4d2\"".into()), mtime: 1794391200, size: 1234 });
    }

    #[test]
    fn parses_apache_style_listing_with_absolute_hrefs_and_404_propstat() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:lp1="DAV:">
  <response><href>http://nas/dav/</href><propstat><prop><lp1:resourcetype><lp1:collection/></lp1:resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response>
  <response><href>http://nas/dav/doc.txt</href>
    <propstat><prop><lp1:resourcetype/><lp1:getcontentlength>7</lp1:getcontentlength><lp1:getetag>W/"7-abc"</lp1:getetag></prop><status>HTTP/1.1 200 OK</status></propstat>
    <propstat><prop><getlastmodified/></prop><status>HTTP/1.1 404 Not Found</status></propstat>
  </response>
</multistatus>"#;
        let (this, children) = split_self(parse_multistatus(xml).unwrap(), "/dav");
        assert!(this.is_some());
        assert_eq!(children, vec![Entry { name: "doc.txt".into(), is_dir: false, version: Version { etag: Some("W/\"7-abc\"".into()), mtime: 0, size: 7 } }]);
    }

    #[test]
    fn falls_back_to_first_entry_when_hrefs_do_not_match() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/internal/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/internal/x</d:href><d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop></d:propstat></d:response></d:multistatus>"#;
        let (this, children) = split_self(parse_multistatus(xml).unwrap(), "/public");
        assert!(this.unwrap().is_dir);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "x");
    }

    #[test]
    fn rejects_html_error_pages() {
        assert!(parse_multistatus("<html><body>Login</body>").is_err());
    }
}
