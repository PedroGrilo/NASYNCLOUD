//! End-to-end test of the Windows engine (`src/win.rs`, `src/win/folders.rs`):
//! the real `nasyncloud serve` on a real sync folder (Cloud Files API), against
//! `examples/fake_nas.rs`. What Linux and macOS get from `tests/e2e.sh`.
//!
//! It runs with `cargo test` on Windows (and only there), by itself on the
//! `windows-latest` runner of GitHub Actions (`.github/workflows/ci.yml`):
//!
//!   cargo test --test e2e_windows -- --nocapture
//!
//! Everything lives in `target/tmp/e2e-windows`; the NAS listens on 5025 and
//! 5026, the interface on 18750 and 18751 (not the ports of `tests/e2e.sh`).
//! The test itself plays the other programs: it reads and writes in the sync
//! folder, and holds files and folders open while the service works on them.
//!
//! The part that moves the Desktop onto the NAS (`SHSetKnownFolderPath`)
//! changes the folders of the user running the test, and leaves what was on
//! the Desktop in a second sync folder next to it (`nasyncloud-e2e-mnt-b`).
//! So it only runs with `NASYNCLOUD_E2E_KNOWN_FOLDERS=1`: set by the
//! workflows, on a machine that is thrown away afterwards. Never set it on a
//! computer someone uses. On GitHub Actions the test fails without it, so
//! that this part is never skipped unnoticed.
#![cfg(windows)]

use std::fs;
use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cloud_filter::placeholder::Placeholder;
use serde_json::{json, Value};

const USER: &str = "pedro";
const PASS: &str = "segredo";
const TOKEN_HEADER: &str = "x-nasyncloud-token";
/// What gates the part that moves the user's Desktop.
const KNOWN_FOLDERS_ENV: &str = "NASYNCLOUD_E2E_KNOWN_FOLDERS";

const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_LIST_DIRECTORY: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const SHARE_ALL: u32 = 7;

/// The verdicts, in the style of `tests/e2e.sh`: every check runs, and the
/// test fails at the end if any did.
#[derive(Default)]
struct Checks {
    passed: u32,
    failed: Vec<String>,
}

impl Checks {
    fn ok(&mut self, name: &str) {
        self.passed += 1;
        println!("  ok   {name}");
    }

    fn bad(&mut self, name: &str, detail: &str) {
        self.failed.push(name.to_string());
        println!("  FAIL {name}{}{detail}", if detail.is_empty() { "" } else { " -- " });
    }

    fn check(&mut self, name: &str, verdict: Result<(), String>) -> bool {
        match &verdict {
            Ok(()) => self.ok(name),
            Err(detail) => self.bad(name, detail),
        }
        verdict.is_ok()
    }

    fn is(&mut self, name: &str, cond: bool) -> bool {
        self.check(name, if cond { Ok(()) } else { Err(String::new()) })
    }
}

/// Tries `probe` until it says yes, for at most `secs`. Returns the last "no".
fn wait_for(secs: u64, mut probe: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match probe() {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(format!("{e} (after {secs}s)")),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn expect(cond: bool, otherwise: impl FnOnce() -> String) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(otherwise())
    }
}

/// A child process that does not outlive the test.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nasyncloud"))
}

/// `cargo test` builds the examples next to the binary.
fn fake_nas_bin() -> PathBuf {
    bin().parent().expect("the binary is in a folder").join("examples").join("fake_nas.exe")
}

fn work_dir() -> PathBuf {
    // Not under %TEMP%: sync roots there are known to confuse the lookup of registrations.
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-windows")
}

fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The stand-in NAS: `fake_nas` over a folder, with its requests in a log file.
struct Nas {
    dir: PathBuf,
    log: PathBuf,
    url: String,
    _proc: Proc,
}

impl Nas {
    fn start(dir: PathBuf, port: u16, closed_top: bool) -> Self {
        let log = dir.with_extension("log");
        let mut cmd = Command::new(fake_nas_bin());
        if closed_top {
            cmd.arg("--closed-top");
        }
        let stderr = fs::File::create(&log).expect("create the NAS log");
        let proc = cmd.arg(&dir).arg(format!("127.0.0.1:{port}")).arg(USER).arg(PASS).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn();
        Self { dir, log, url: format!("http://127.0.0.1:{port}"), _proc: Proc(proc.unwrap_or_else(|e| panic!("cannot start {}: {e}", fake_nas_bin().display()))) }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel.replace('/', "\\"))
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.path(rel)).ok()
    }

    /// `rel` is on the NAS with exactly this content.
    fn has(&self, rel: &str, content: &[u8]) -> Result<(), String> {
        match self.read(rel) {
            Some(data) if data == content => Ok(()),
            Some(data) => Err(format!("{rel} on the NAS has {} byte(s), expected {}", data.len(), content.len())),
            None => Err(format!("{rel} is not on the NAS")),
        }
    }

    /// How many requests so far start with `prefix` (`"GET /Docs"`).
    fn requests(&self, prefix: &str) -> usize {
        fs::read_to_string(&self.log).unwrap_or_default().lines().filter(|l| l.starts_with(prefix)).count()
    }
}

/// The local API of a running service.
struct Api {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Api {
    fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        self.rt.block_on(async {
            let mut req = self.client.request(method, format!("{}{path}", self.base)).header(TOKEN_HEADER, &self.token);
            if let Some(body) = body {
                req = req.body(body.to_string());
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let text = resp.text().await.map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_str(&text).map_err(|e| format!("HTTP {status}: {text} ({e})"))?;
            if status.is_success() {
                Ok(value)
            } else {
                Err(format!("HTTP {status}: {value}"))
            }
        })
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.call(reqwest::Method::GET, path, None)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.call(reqwest::Method::POST, path, Some(body))
    }

    /// One entry of `/api/browse`: `Docs/a.txt` is the entry `a.txt` of `Docs`.
    fn entry(&self, rel: &str) -> Result<Value, String> {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let dir = percent_encoding::utf8_percent_encode(dir, percent_encoding::NON_ALPHANUMERIC);
        let listing = self.get(&format!("/api/browse?path={dir}"))?;
        listing["entries"].as_array().into_iter().flatten().find(|e| e["name"] == name).cloned().ok_or_else(|| format!("{rel} is not in the listing"))
    }

    /// `rel` has this state (`online`, `cached`, `modified`...) in `/api/browse`.
    fn state_is(&self, rel: &str, state: &str) -> Result<(), String> {
        let entry = self.entry(rel)?;
        expect(entry["state"] == state, || format!("{rel} is {} (error: {}), expected {state}", entry["state"], entry["error"]))
    }

    fn info(&self) -> Value {
        self.get("/api/state").map(|s| s["info"].clone()).unwrap_or(Value::Null)
    }

    /// Nothing waiting to be sent and nothing that failed.
    fn settled(&self) -> Result<(), String> {
        let info = self.info();
        expect(info["pending_uploads"] == 0 && info["errors"].as_array().is_some_and(Vec::is_empty), || format!("pending_uploads {}, errors {}", info["pending_uploads"], info["errors"]))
    }

    fn no_errors(&self) -> Result<(), String> {
        let info = self.info();
        expect(info["errors"].as_array().is_some_and(Vec::is_empty), || format!("info.errors: {}", info["errors"]))
    }
}

/// `nasyncloud serve` with its own settings, log and sync folder.
struct Service {
    cfg: PathBuf,
    log: PathBuf,
    root: PathBuf,
    api: Api,
    proc: Proc,
}

impl Service {
    /// Settings, cache and log go in `dir`; `root` is the sync folder.
    fn start(dir: &Path, name: &str, root: PathBuf, nas_url: &str, ui_port: u16) -> Result<Self, String> {
        let cfg = dir.join(format!("cfg-{name}")).join("config.toml");
        let log = dir.join(format!("service-{name}.log"));
        fs::create_dir_all(cfg.parent().expect("has a parent")).map_err(|e| e.to_string())?;
        // Literal TOML strings: Windows paths are full of backslashes.
        let text = format!("url = '{nas_url}'\nusername = '{USER}'\nmountpoint = '{}'\ncache_dir = '{}'\ndir_ttl_secs = 5\npoll_interval_secs = 15\nui_port = {ui_port}\nprevent_sleep = false\n", root.display(), dir.join(format!("cache-{name}")).display());
        fs::write(&cfg, text).map_err(|e| e.to_string())?;
        let out = fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let child = Command::new(bin()).arg("--config").arg(&cfg).arg("-v").arg("serve").env("NASYNCLOUD_PASSWORD", PASS).stdin(Stdio::null()).stdout(out).stderr(err).spawn().map_err(|e| format!("cannot start {}: {e}", bin().display()))?;
        let proc = Proc(child);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())?;
        let mut api = Api { rt, client, base: format!("http://127.0.0.1:{ui_port}"), token: String::new() };
        // `ui.json` (the token) is written when the interface starts listening.
        wait_for(60, || {
            let ui: Value = serde_json::from_slice(&fs::read(cfg.with_file_name("ui.json")).map_err(|e| format!("ui.json: {e}"))?).map_err(|e| format!("ui.json: {e}"))?;
            api.token = ui["token"].as_str().unwrap_or_default().to_string();
            api.get("/api/ping").map(|_| ())
        })?;
        wait_for(60, || {
            let state = api.get("/api/state")?;
            expect(state["phase"] == "running", || format!("phase {}, error {}", state["phase"], state["error"]))
        })?;
        Ok(Self { cfg, log, root, api, proc })
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel.replace('/', "\\"))
    }

    /// Asks the service to quit (as the app does) and waits for it to be gone.
    fn stop(mut self) -> Result<(), String> {
        self.api.post("/api/quit", json!({}))?;
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match self.proc.0.try_wait().map_err(|e| e.to_string())? {
                Some(status) => return expect(status.success(), || format!("the service ended with {status}")),
                None if Instant::now() >= deadline => return Err("the service was still running 45 s after /api/quit".into()),
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }

    /// `nasyncloud unregister`: the folder stops being a sync folder (and
    /// the known folders in it go back to their usual place).
    fn unregister(cfg: &Path) -> Result<String, String> {
        let out = Command::new(bin()).arg("--config").arg(cfg).arg("unregister").env("NASYNCLOUD_PASSWORD", PASS).output().map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        expect(out.status.success(), || format!("unregister ended with {}: {text}", out.status))?;
        Ok(text)
    }
}

/// What Windows says about an item of the sync folder: `None` for an ordinary
/// file or folder (not adopted), else whether it is marked as in sync.
fn in_sync(path: &Path) -> Result<Option<bool>, String> {
    let file = fs::OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Placeholder::from(file).info().map(|i| i.map(|i| i.is_in_sync())).map_err(|e| e.to_string())
}

/// The placeholder flags of an item, for the record (not a verdict).
fn flags(path: &Path) -> String {
    match fs::OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path) {
        Ok(file) => format!("{:?}", Placeholder::from(file).state()),
        Err(e) => e.to_string(),
    }
}

/// The item is a synced one for Windows (a placeholder, marked in sync).
fn adopted(path: &Path) -> Result<(), String> {
    match in_sync(path)? {
        Some(true) => Ok(()),
        Some(false) => Err(format!("{} is a placeholder, but not marked in sync", path.display())),
        None => Err(format!("{} is still an ordinary item (not adopted)", path.display())),
    }
}

/// Repeatable bytes that do not compress into a pattern.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

/// Names in a folder, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).map(|it| it.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

/// Every file below `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            out.extend(walk(&entry.path()));
        } else {
            out.push(entry.path());
        }
    }
    out
}

/// A local file or folder got to the NAS and Windows has it as synced.
fn synced(c: &mut Checks, what: &str, svc: &Service, nas: &Nas, rel: &str, content: Option<&[u8]>) {
    match content {
        Some(content) => {
            c.check(&format!("{what}: it reaches the NAS"), wait_for(30, || nas.has(rel, content)));
            c.check(&format!("{what}: it is marked as synced (cached)"), wait_for(30, || svc.api.state_is(rel, "cached")));
        }
        None => {
            c.check(&format!("{what}: it is created on the NAS"), wait_for(30, || expect(nas.path(rel).is_dir(), || format!("{rel} is not a folder on the NAS"))));
        }
    }
    c.check(&format!("{what}: Windows has it as a synced item"), wait_for(30, || adopted(&svc.path(rel))));
}

/// Something the test keeps open, the way another program would.
type Held = Box<dyn std::any::Any>;

fn open_as(path: &Path, access: u32, share: u32, flags: u32) -> Result<fs::File, String> {
    fs::OpenOptions::new().access_mode(access).share_mode(share).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | flags).open(path).map_err(|e| format!("cannot hold {}: {e}", path.display()))
}

/// A viewer: reads, and lets everyone else do anything (what Explorer does).
fn hold_shared(path: &Path) -> Result<Held, String> {
    Ok(Box::new(open_as(path, GENERIC_READ, SHARE_ALL, 0)?))
}

/// A program that has the item open and does not let others write to it.
fn hold_deny_write(path: &Path) -> Result<Held, String> {
    Ok(Box::new(open_as(path, GENERIC_READ, 1, 0)?))
}

/// A program that holds a shared lock on the first byte of the file (others
/// can read it, nobody can write it). Windows grants no oplock on a file
/// with a byte-range lock in it: `CfOpenFileWithOplock` fails with
/// 0x8007012C, while the content can still be read and sent.
fn hold_byte_lock(path: &Path) -> Result<Held, String> {
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_FAIL_IMMEDIATELY};
    let file = open_as(path, GENERIC_READ | GENERIC_WRITE, SHARE_ALL, 0)?;
    // SAFETY: a synchronous handle; the OVERLAPPED only carries the offset (0) and outlives the call.
    let locked = unsafe {
        let mut at: windows_sys::Win32::System::IO::OVERLAPPED = std::mem::zeroed();
        LockFileEx(file.as_raw_handle(), LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &mut at)
    };
    expect(locked != 0, || format!("LockFileEx on {} failed: {}", path.display(), std::io::Error::last_os_error()))?;
    Ok(Box::new(file))
}

/// A program holding an exclusive oplock on the file, which lets go of it as
/// soon as someone else needs the content (what indexers and antivirus do).
/// `CfOpenFileWithOplock` will not disturb it: it fails with 0x80070322, for
/// as long as nobody else opens the file.
struct Oplock {
    handle: usize,
    pending: usize,
    waiter: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Oplock {
    fn drop(&mut self) {
        // Still held (nobody needed the file): give the request up. It names
        // our own OVERLAPPED, so nothing else can be cancelled by mistake.
        // SAFETY: the OVERLAPPED is never freed.
        unsafe { windows_sys::Win32::System::IO::CancelIoEx(self.handle as _, self.pending as *const _) };
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
    }
}

fn hold_oplock(path: &Path) -> Result<Held, String> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ, OPLOCK_LEVEL_CACHE_WRITE, REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST};
    use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
    const ERROR_IO_PENDING: i32 = 997;
    let size = std::mem::size_of::<REQUEST_OPLOCK_INPUT_BUFFER>();
    let mut last = String::new();
    // Granted only while nobody else has the file open: a scanner looking at
    // the new file for a moment is waited out.
    for _ in 0..20 {
        let file = open_as(path, GENERIC_READ, SHARE_ALL, FILE_FLAG_OVERLAPPED)?;
        // The system writes to these when the oplock breaks: never freed.
        let input: &'static REQUEST_OPLOCK_INPUT_BUFFER = Box::leak(Box::new(REQUEST_OPLOCK_INPUT_BUFFER {
            StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
            StructureLength: size as u16,
            RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE | OPLOCK_LEVEL_CACHE_WRITE,
            Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
        }));
        let output: &'static mut [u8; 64] = Box::leak(Box::new([0; 64]));
        // SAFETY: an all-zero OVERLAPPED is a valid one.
        let pending: &'static mut OVERLAPPED = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        let pending_at = pending as *mut OVERLAPPED as usize;
        // SAFETY: the buffers and the OVERLAPPED live for the rest of the process.
        let ok = unsafe { DeviceIoControl(file.as_raw_handle(), FSCTL_REQUEST_OPLOCK, (input as *const REQUEST_OPLOCK_INPUT_BUFFER).cast(), size as u32, output.as_mut_ptr().cast(), output.len() as u32, std::ptr::null_mut(), pending) };
        let error = std::io::Error::last_os_error();
        if ok == 0 && error.raw_os_error() == Some(ERROR_IO_PENDING) {
            let handle = file.as_raw_handle() as usize;
            let waiter = std::thread::spawn(move || {
                let mut bytes = 0u32;
                // Returns when the oplock breaks (someone else opens the file) or the request is cancelled.
                // SAFETY: `file` is open until the end of this thread; the OVERLAPPED is never freed.
                let broken = unsafe { GetOverlappedResult(handle as _, pending_at as *const OVERLAPPED, &mut bytes, 1) } != 0;
                if broken {
                    println!("       (the oplock was broken: another program needed the file, and the holder lets go)");
                }
                drop(file); // closing the handle acknowledges the break
            });
            return Ok(Box::new(Oplock { handle, pending: pending_at, waiter: Some(waiter) }));
        }
        last = error.to_string();
        drop(file);
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("no exclusive oplock on {}: {last}", path.display()))
}

/// Explorer showing a folder: a handle that lists it, sharing everything,
/// with a change notification pending.
fn hold_watch(path: &Path) -> Result<Held, String> {
    use windows_sys::Win32::Storage::FileSystem::{ReadDirectoryChangesW, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME};
    let dir = open_as(path, FILE_LIST_DIRECTORY | SYNCHRONIZE, SHARE_ALL, FILE_FLAG_OVERLAPPED)?;
    // The system writes to these when the request ends, which may be after
    // the handle is closed: they are never freed.
    let buffer: &'static mut [u32; 1024] = Box::leak(Box::new([0; 1024]));
    // SAFETY: an all-zero OVERLAPPED is a valid one.
    let pending: &'static mut windows_sys::Win32::System::IO::OVERLAPPED = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
    // SAFETY: the buffer and the OVERLAPPED live for the rest of the process.
    let ok = unsafe { ReadDirectoryChangesW(dir.as_raw_handle(), buffer.as_mut_ptr().cast(), 4096, 0, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME, std::ptr::null_mut(), pending, None) };
    expect(ok != 0, || format!("ReadDirectoryChangesW on {} failed: {}", path.display(), std::io::Error::last_os_error()))?;
    Ok(Box::new(dir))
}

/// Creates a file and keeps it open as `hold` does while it is sent and adopted.
fn held_file(c: &mut Checks, svc: &Service, nas: &Nas, rel: &str, how: &str, hold: fn(&Path) -> Result<Held, String>) {
    let path = svc.path(rel);
    let content = format!("aberto por outro programa ({how})\n");
    let kept = match fs::write(&path, &content).map_err(|e| e.to_string()).and_then(|()| hold(&path)) {
        Ok(kept) => kept,
        Err(e) => return c.bad(&format!("file {how}: the test holds it"), &e),
    };
    synced(c, &format!("file {how}"), svc, nas, rel, Some(content.as_bytes()));
    c.check(&format!("file {how}: no errors"), svc.api.no_errors());
    drop(kept);
}

/// Creates a folder and keeps it open as `hold` does while it is created on
/// the NAS and adopted, and while a file is put in it.
fn held_folder(c: &mut Checks, svc: &Service, nas: &Nas, rel: &str, how: &str, hold: fn(&Path) -> Result<Held, String>) {
    let path = svc.path(rel);
    let kept = match fs::create_dir(&path).map_err(|e| e.to_string()).and_then(|()| hold(&path)) {
        Ok(kept) => kept,
        Err(e) => return c.bad(&format!("folder {how}: the test holds it"), &e),
    };
    let inside = format!("{rel}/dentro.txt");
    if let Err(e) = fs::write(svc.path(&inside), "dentro de uma pasta aberta\n") {
        c.bad(&format!("folder {how}: a file can be put in it"), &e.to_string());
    }
    synced(c, &format!("folder {how}"), svc, nas, rel, None);
    synced(c, &format!("folder {how}, the file in it"), svc, nas, &inside, Some(b"dentro de uma pasta aberta\n"));
    c.check(&format!("folder {how}: no errors"), svc.api.no_errors());
    drop(kept);
}

/// An ordinary NAS, where things can be created at the top: browsing,
/// reading on demand, sending what is created here (also while other programs
/// have it open), changes made on the NAS, pin and free, rename and delete.
fn open_nas(dir: &Path, c: &mut Checks) -> Result<(), String> {
    let nas_dir = dir.join("nas-a");
    let film = noise(3 << 20, 7);
    let numbers: String = (1..=20_000).map(|n| format!("{n}\n")).collect();
    fs::create_dir_all(nas_dir.join("Docs").join("sub")).map_err(|e| e.to_string())?;
    fs::create_dir_all(nas_dir.join("Media")).map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("Docs").join("relatório final (v2).txt"), "relatório\n").map_err(|e| e.to_string())?;
    for name in ["numeros.txt", "acrescentar.txt", "emendar.txt", "reescrever.txt"] {
        fs::write(nas_dir.join("Docs").join(name), &numbers).map_err(|e| e.to_string())?;
    }
    fs::write(nas_dir.join("Docs").join("sub").join("nota.txt"), "nota\n").map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("Media").join("filme.bin"), &film).map_err(|e| e.to_string())?;
    // Never opened by the test: only pinned, through the interface.
    fs::create_dir_all(nas_dir.join("Fixar").join("dentro")).map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("Fixar").join("a.txt"), "para ter sempre aqui\n").map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("Fixar").join("dentro").join("b.txt"), "numa pasta que ninguém abriu\n").map_err(|e| e.to_string())?;
    let nas = Nas::start(nas_dir, 5025, false);

    println!("== start (an ordinary NAS)");
    let svc = match Service::start(dir, "a", dir.join("mnt-a"), &nas.url, 18750) {
        Ok(svc) => svc,
        Err(e) => {
            c.bad("the service starts and connects the sync folder", &e);
            println!("{}", tail(&dir.join("service-a.log"), 40));
            return Ok(());
        }
    };
    c.ok("the service starts and connects the sync folder");
    let state = svc.api.get("/api/state")?;
    c.check("state: running, online, Windows", expect(state["phase"] == "running" && state["info"]["online"] == true && state["platform"] == "windows", || state.to_string()));

    println!("== browse and read on demand");
    // Before any other program opens the folder: Windows does not ask the
    // service for the content of a folder the service itself lists.
    let listed = |rel: &str| -> Result<Vec<String>, String> {
        let listing = svc.api.get(&format!("/api/browse?path={}", percent_encoding::utf8_percent_encode(rel, percent_encoding::NON_ALPHANUMERIC)))?;
        let mut names: Vec<String> = listing["entries"].as_array().into_iter().flatten().map(|e| e["name"].as_str().unwrap_or("?").to_string()).collect();
        names.sort();
        Ok(names)
    };
    println!("       (the sync folder before anything lists it: {})", flags(&svc.root));
    c.check("the interface lists the top before anyone opened the folder", listed("").and_then(|l| expect(l == ["Docs", "Fixar", "Media"], || format!("{l:?}"))));
    println!("       (the sync folder now: {}; Docs: {})", flags(&svc.root), flags(&svc.path("Docs")));
    c.check("...and a folder nobody opened yet", listed("Docs/sub").and_then(|l| expect(l == ["nota.txt"], || format!("{l:?}"))));
    let top = names(&svc.root);
    c.check("top-level listing", expect(top == ["Docs", "Fixar", "Media"], || format!("{top:?}")));
    c.is("names with accents and spaces", svc.path("Docs/relatório final (v2).txt").is_file());
    let len = fs::metadata(svc.path("Media/filme.bin")).map(|m| m.len()).unwrap_or(0);
    c.check("size without download", expect(len == film.len() as u64, || format!("{len} byte(s)")));
    c.check("a file of the NAS shows up as a placeholder (online only)", svc.api.state_is("Media/filme.bin", "online"));
    c.check("browsing downloads nothing", expect(nas.requests("GET /") == 0, || tail(&nas.log, 5)));
    c.is("small file: the content arrives when it is read", fs::read(svc.path("Docs/relatório final (v2).txt")).is_ok_and(|d| d == "relatório\n".as_bytes()));
    c.is("large file: the content arrives when it is read", fs::read(svc.path("Media/filme.bin")).is_ok_and(|d| d == film));
    c.check("...it was downloaded from the NAS", expect(nas.requests("GET /Media/filme.bin") >= 1, || tail(&nas.log, 5)));
    c.check("...and is cached afterwards", wait_for(10, || svc.api.state_is("Media/filme.bin", "cached")));

    println!("== new files and folders");
    fs::write(svc.path("novo.txt"), "criado no computador\n").map_err(|e| e.to_string())?;
    synced(c, "new file at the top", &svc, &nas, "novo.txt", Some(b"criado no computador\n"));
    fs::write(svc.path("Docs/dentro.txt"), "dentro de uma pasta da NAS\n").map_err(|e| e.to_string())?;
    synced(c, "new file in a folder of the NAS", &svc, &nas, "Docs/dentro.txt", Some(b"dentro de uma pasta da NAS\n"));
    fs::create_dir_all(svc.path("Projetos/app/src")).map_err(|e| e.to_string())?;
    fs::write(svc.path("Projetos/app/src/main.rs"), "fn main() {}\n").map_err(|e| e.to_string())?;
    synced(c, "new folder", &svc, &nas, "Projetos", None);
    synced(c, "new folder inside it", &svc, &nas, "Projetos/app/src", None);
    synced(c, "file inside the new folders", &svc, &nas, "Projetos/app/src/main.rs", Some(b"fn main() {}\n"));

    println!("== changes to files of the NAS");
    let longer = format!("{numbers}linha extra\n");
    let append = |rel: &str| fs::OpenOptions::new().append(true).open(svc.path(rel)).and_then(|mut f| f.write_all(b"linha extra\n")).map_err(|e| e.to_string());
    // A handle that can only append (FILE_APPEND_DATA) is what programs
    // written in Go or Rust add to a file with. On an online-only file,
    // Windows downloads the file and then fails that write with error 1450
    // (seen on the runner, Windows Server 2025 build 26100; the engine has
    // done its part by then). Whatever Windows does, nothing may be lost
    // quietly: the program is told, the file is intact, the next try works.
    match append("Docs/acrescentar.txt") {
        Ok(()) => c.ok("append to an online-only file (a handle that can only append)"),
        Err(e) => {
            println!("       (Windows refused the append to an online-only file: {e})");
            c.is("append to an online-only file, refused by Windows: the file is intact", fs::read(svc.path("Docs/acrescentar.txt")).is_ok_and(|d| d == numbers.as_bytes()));
            c.check("...and the next try works", append("Docs/acrescentar.txt"));
        }
    }
    synced(c, "appended, online-only", &svc, &nas, "Docs/acrescentar.txt", Some(longer.as_bytes()));
    let amend = fs::OpenOptions::new().read(true).write(true).open(svc.path("Docs/emendar.txt")).and_then(|mut f| f.write_all(b"X")).map_err(|e| e.to_string());
    if c.check("write at the start of an online-only file", amend) {
        synced(c, "amended, online-only", &svc, &nas, "Docs/emendar.txt", Some(format!("X{}", &numbers[1..]).as_bytes()));
    }
    if c.check("replace the content of an online-only file", fs::write(svc.path("Docs/reescrever.txt"), "de novo\n").map_err(|e| e.to_string())) {
        synced(c, "replaced, online-only", &svc, &nas, "Docs/reescrever.txt", Some(b"de novo\n"));
    }
    c.is("a file is read (and so kept here)", fs::read(svc.path("Docs/numeros.txt")).is_ok_and(|d| d == numbers.as_bytes()));
    if c.check("append to a file that is already here", append("Docs/numeros.txt")) {
        synced(c, "appended, already here", &svc, &nas, "Docs/numeros.txt", Some(longer.as_bytes()));
    }
    let parts: Vec<PathBuf> = walk(&nas.dir).into_iter().filter(|p| nasyncloud::remote::is_part_file(&p.file_name().unwrap_or_default().to_string_lossy())).collect();
    c.check("no temporary upload files left on the NAS", expect(parts.is_empty(), || format!("{parts:?}")));
    c.check("nothing left to send and no errors", wait_for(30, || svc.api.settled()));

    println!("== while other programs have them open");
    held_file(c, &svc, &nas, "Docs/aberto.txt", "open in a viewer (sharing everything)", hold_shared);
    held_file(c, &svc, &nas, "Docs/so-leitura.txt", "open in a program that denies writing", hold_deny_write);
    held_file(c, &svc, &nas, "Docs/trancado.txt", "with a byte-range lock in it", hold_byte_lock);
    held_file(c, &svc, &nas, "Docs/com-oplock.txt", "under another program's exclusive oplock", hold_oplock);
    held_folder(c, &svc, &nas, "Aberta", "shown by Explorer (watched, sharing everything)", hold_watch);
    held_folder(c, &svc, &nas, "Docs/Reservada", "open in a program that denies writing", hold_deny_write);
    c.check("nothing left to send and no errors", wait_for(30, || svc.api.settled()));

    println!("== changes made on the NAS by someone else");
    c.is("a file in a sub-folder is read", fs::read(svc.path("Docs/sub/nota.txt")).is_ok_and(|d| d == b"nota\n"));
    let refresh = |rel: &str| svc.api.post("/api/refresh", json!({ "path": rel })).map(|_| ());
    fs::write(nas.path("Docs/chegou-depois.txt"), "novo na NAS\n").map_err(|e| e.to_string())?;
    refresh("Docs")?;
    c.check("a file added on the NAS shows up", wait_for(30, || expect(svc.path("Docs/chegou-depois.txt").is_file(), || names(&svc.path("Docs")).join(", "))));
    let edited = "alterado na NAS, com outro tamanho\n";
    fs::write(nas.path("Docs/sub/nota.txt"), edited).map_err(|e| e.to_string())?;
    refresh("Docs/sub")?;
    // Sizes only (no handle on the content): the engine needs the file to itself to replace it.
    c.check("a file edited on the NAS changes here", wait_for(40, || expect(fs::metadata(svc.path("Docs/sub/nota.txt")).is_ok_and(|m| m.len() == edited.len() as u64), || "still the old size".into())));
    c.is("...with the new content", fs::read(svc.path("Docs/sub/nota.txt")).is_ok_and(|d| d == edited.as_bytes()));
    fs::remove_file(nas.path("Docs/chegou-depois.txt")).map_err(|e| e.to_string())?;
    refresh("Docs")?;
    c.check("a file removed on the NAS goes away here", wait_for(30, || expect(!svc.path("Docs/chegou-depois.txt").exists(), || "still here".into())));
    fs::create_dir_all(nas.path("Docs/pasta-nova")).and_then(|()| fs::write(nas.path("Docs/pasta-nova/x.txt"), "x\n")).map_err(|e| e.to_string())?;
    refresh("Docs")?;
    c.check("a folder added on the NAS shows up", wait_for(30, || expect(svc.path("Docs/pasta-nova").is_dir(), || names(&svc.path("Docs")).join(", "))));
    let inside = names(&svc.path("Docs/pasta-nova"));
    c.check("...with what is in it", expect(inside == ["x.txt"], || format!("{inside:?}")));
    fs::remove_dir_all(nas.path("Docs/pasta-nova")).map_err(|e| e.to_string())?;
    refresh("Docs")?;
    c.check("a folder removed on the NAS goes away here", wait_for(30, || expect(!svc.path("Docs/pasta-nova").exists(), || "still here".into())));

    println!("== free up space / always keep on this device");
    let freed = svc.api.post("/api/free", json!({ "path": "Media" }));
    c.check("free up space drops the local copy", freed.and_then(|r| expect(r["freed"].as_u64().unwrap_or(0) >= film.len() as u64, || r.to_string())));
    c.check("...the file is online only again", svc.api.state_is("Media/filme.bin", "online"));
    c.check("always keep on this device is accepted", svc.api.post("/api/pin", json!({ "path": "Media", "on": true })).map(|_| ()));
    c.check("...the file is downloaded again", wait_for(40, || svc.api.state_is("Media/filme.bin", "cached")));
    c.is("...with the same content", fs::read(svc.path("Media/filme.bin")).is_ok_and(|d| d == film));
    c.check("...and can be let go of", svc.api.post("/api/pin", json!({ "path": "Media", "on": false })).map(|_| ()));
    c.check("always keep a folder nobody opened", svc.api.post("/api/pin", json!({ "path": "Fixar", "on": true })).map(|_| ()));
    c.check("...what is in it is downloaded", wait_for(40, || svc.api.state_is("Fixar/a.txt", "cached")));
    c.check("...and what is in its folders too", wait_for(40, || svc.api.state_is("Fixar/dentro/b.txt", "cached")));
    c.is("...with the content of the NAS", fs::read(svc.path("Fixar/dentro/b.txt")).is_ok_and(|d| d == "numa pasta que ninguém abriu\n".as_bytes()));

    println!("== rename and delete");
    let gone = |rel: &str| expect(!nas.path(rel).exists(), || format!("{rel} is still on the NAS"));
    c.check("rename a file", fs::rename(svc.path("Docs/dentro.txt"), svc.path("Docs/renomeado.txt")).map_err(|e| e.to_string()));
    c.check("...it is renamed on the NAS", wait_for(10, || nas.has("Docs/renomeado.txt", b"dentro de uma pasta da NAS\n").and(gone("Docs/dentro.txt"))));
    c.check("move a folder into another", fs::rename(svc.path("Projetos/app"), svc.path("Docs/app")).map_err(|e| e.to_string()));
    c.check("...it is moved on the NAS", wait_for(10, || nas.has("Docs/app/src/main.rs", b"fn main() {}\n").and(gone("Projetos/app"))));
    c.check("delete a file", fs::remove_file(svc.path("Docs/renomeado.txt")).map_err(|e| e.to_string()));
    c.check("...it is deleted on the NAS", wait_for(10, || gone("Docs/renomeado.txt")));
    c.check("delete a folder", fs::remove_dir_all(svc.path("Projetos")).map_err(|e| e.to_string()));
    c.check("...it is deleted on the NAS", wait_for(10, || gone("Projetos")));
    let (here, there) = (names(&svc.root), names(&nas.dir));
    c.check("the folder and the NAS show the same", expect(here == there, || format!("here {here:?}, on the NAS {there:?}")));
    c.check("nothing left to send and no errors", wait_for(30, || svc.api.settled()));

    println!("== stop and start again");
    let (cfg, log) = (svc.cfg.clone(), svc.log.clone());
    let sent = nas.requests("PUT ");
    c.check("the service stops when asked to", svc.stop());
    match Service::start(dir, "a", dir.join("mnt-a"), &nas.url, 18750) {
        Ok(svc) => {
            c.ok("the service starts again on the same folder");
            // Long enough for the first pass over the folders (2 s after the start).
            std::thread::sleep(Duration::from_secs(8));
            c.check("nothing is sent again", expect(nas.requests("PUT ") == sent, || tail(&nas.log, 5)));
            c.check("synced files are still synced", svc.api.state_is("novo.txt", "cached"));
            c.check("nothing left to send and no errors", svc.api.settled());
            c.check("the service stops again", svc.stop());
        }
        Err(e) => c.bad("the service starts again on the same folder", &e),
    }
    let conflicts: Vec<PathBuf> = walk(&nas.dir).into_iter().filter(|p| p.to_string_lossy().contains("(conflict")).collect();
    c.check("no conflict copies on the NAS", expect(conflicts.is_empty(), || format!("{conflicts:?}")));
    let failed: Vec<String> = fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains("retrying in") || l.contains("could not populate") || l.contains("panicked")).map(str::to_string).collect();
    c.check("the service never failed at anything it tried", expect(failed.is_empty(), || failed.join("\n       ")));
    c.check("unregister: the folder stops being a sync folder", Service::unregister(&cfg).map(|_| ()));
    Ok(())
}

/// Where Windows says the Desktop is, asked by this process (not by the service).
fn desktop_path() -> PathBuf {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};
    let mut p: *mut u16 = std::ptr::null_mut();
    // SAFETY: an out-parameter call; on success `p` is a NUL-terminated wide string, freed below.
    unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut p);
        let path = if hr >= 0 && !p.is_null() { PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(p, (0..).take_while(|&i| *p.add(i) != 0).count()))) } else { PathBuf::new() };
        CoTaskMemFree(p as *const _);
        path
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let clean = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_lowercase();
    clean(a) == clean(b)
}

/// Whatever happens to the test, the Desktop does not stay in a folder that
/// is about to be deleted: `unregister` points it back at its usual place.
struct PutFoldersBack(PathBuf);

impl Drop for PutFoldersBack {
    fn drop(&mut self) {
        if let Err(e) = Service::unregister(&self.0) {
            println!("  (could not put the known folders back: {e})");
        }
    }
}

/// A NAS that takes nothing at its top, as UGOS does with the address of the
/// NAS itself: the Desktop is refused there (and nothing moves), and goes
/// into one of the folders the NAS has. Moves the Desktop of the user running
/// the test: only on a machine that is thrown away.
fn closed_top(dir: &Path, c: &mut Checks) -> Result<(), String> {
    let nas_dir = dir.join("nas-b");
    fs::create_dir_all(nas_dir.join("home").join("mais")).map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("home").join("outro.txt"), "já estava na NAS\n").map_err(|e| e.to_string())?;
    fs::create_dir_all(nas_dir.join("Docs")).map_err(|e| e.to_string())?;
    fs::write(nas_dir.join("Docs").join("a.txt"), "na NAS\n").map_err(|e| e.to_string())?;
    let nas = Nas::start(nas_dir, 5026, true);
    // On the volume of the Desktop, next to it, as the sync folder of a real
    // installation is (`C:\Users\name\NAS`): what is on the Desktop is then
    // moved by renaming it, not by copying.
    let original = desktop_path();
    let root = original.parent().ok_or("the Desktop has no parent folder")?.join("nasyncloud-e2e-mnt-b");
    let _ = fs::remove_dir_all(&root);

    println!("== start (a NAS with a closed top, as UGOS)");
    let svc = match Service::start(dir, "b", root, &nas.url, 18751) {
        Ok(svc) => svc,
        Err(e) => {
            c.bad("the service starts and connects the sync folder", &e);
            println!("{}", tail(&dir.join("service-b.log"), 40));
            return Ok(());
        }
    };
    c.ok("the service starts and connects the sync folder");
    let _put_back = PutFoldersBack(svc.cfg.clone());
    let desktop = |folders: &Value| folders["folders"].as_array().into_iter().flatten().find(|f| f["key"] == "desktop").cloned().unwrap_or(Value::Null);
    let places = |folders: &Value| -> Vec<String> { folders["places"].as_array().into_iter().flatten().map(|p| p.as_str().unwrap_or("?").to_string()).collect() };

    println!("== the Desktop on the NAS");
    let folders = svc.api.get("/api/folders")?;
    c.check("the Desktop starts in its usual place", expect(desktop(&folders)["on_nas"] == false && same_path(Path::new(desktop(&folders)["path"].as_str().unwrap_or_default()), &original), || folders.to_string()));
    c.check("the top and the folders of the NAS are offered", expect(places(&folders) == ["", "Docs", "home"], || folders["places"].to_string()));
    fs::write(original.join("e2e-ficheiro.txt"), "estava no Ambiente de trabalho\n").map_err(|e| e.to_string())?;
    fs::create_dir_all(original.join("e2e-pasta")).map_err(|e| e.to_string())?;
    fs::write(original.join("e2e-pasta").join("dentro.txt"), "dentro de uma pasta\n").map_err(|e| e.to_string())?;
    let before = names(&original);
    // What `move_items` leaves behind: the shell's own files and links.
    let movable = before.iter().filter(|n| !matches!(n.to_ascii_lowercase().as_str(), "desktop.ini" | "thumbs.db") && !fs::symlink_metadata(original.join(n)).is_ok_and(|m| m.file_type().is_symlink())).count();

    let report = svc.api.post("/api/folders", json!({ "key": "desktop", "on": true, "place": "" }))?;
    c.check("the top refuses the Desktop: refused, nothing moved", expect(report["refused"] == true && report["moved"] == 0, || report.to_string()));
    c.check("...the files are still where they were", expect(names(&original) == before, || format!("{:?}", names(&original))));
    c.check("...nothing was created on the NAS or in the sync folder", expect(!nas.path("Ambiente de trabalho").exists() && !svc.path("Ambiente de trabalho").exists(), || format!("NAS {:?}, here {:?}", names(&nas.dir), names(&svc.root))));
    let folders = svc.api.get("/api/folders")?;
    c.check("...Windows still has the Desktop in its usual place", expect(desktop(&folders)["on_nas"] == false && same_path(&desktop_path(), &original), || format!("{} / {}", desktop(&folders), desktop_path().display())));
    c.check("...and the top is no longer offered", expect(places(&folders) == ["Docs", "home"], || folders["places"].to_string()));

    let report = svc.api.post("/api/folders", json!({ "key": "desktop", "on": true, "place": "home" }))?;
    c.check("a folder of the NAS takes the Desktop: everything moved", expect(report["refused"] == false && report["moved"] == movable && report["left"].as_array().is_some_and(Vec::is_empty), || format!("{report} (expected {movable} item(s) moved)")));
    let there = "home/Ambiente de trabalho";
    let folders = svc.api.get("/api/folders")?;
    let view = desktop(&folders);
    c.check("...the interface shows it on the NAS, in that folder", expect(view["on_nas"] == true && view["place"] == "home" && view["refused"] == false, || view.to_string()));
    c.check("...and Windows has the Desktop there", expect(same_path(&desktop_path(), &svc.path(there)), || desktop_path().display().to_string()));
    synced(c, "the Desktop folder", &svc, &nas, there, None);
    let around = names(&svc.path("home"));
    c.check("...next to what the NAS already had in that folder", expect(around == ["Ambiente de trabalho", "mais", "outro.txt"], || format!("{around:?}")));
    synced(c, "a file that was on the Desktop", &svc, &nas, &format!("{there}/e2e-ficheiro.txt"), Some(b"estava no Ambiente de trabalho\n"));
    synced(c, "a folder that was on the Desktop", &svc, &nas, &format!("{there}/e2e-pasta"), None);
    synced(c, "the file in that folder", &svc, &nas, &format!("{there}/e2e-pasta/dentro.txt"), Some(b"dentro de uma pasta\n"));
    fs::write(desktop_path().join("e2e-novo.txt"), "guardado no Ambiente de trabalho\n").map_err(|e| e.to_string())?;
    synced(c, "a file saved to the Desktop afterwards", &svc, &nas, &format!("{there}/e2e-novo.txt"), Some(b"guardado no Ambiente de trabalho\n"));
    c.check("nothing left to send and no errors", wait_for(60, || svc.api.settled()));

    let report = svc.api.post("/api/folders", json!({ "key": "desktop", "on": false }))?;
    c.check("the Desktop goes back to its usual place", expect(report["refused"] == false && same_path(&desktop_path(), &original), || format!("{report} / {}", desktop_path().display())));
    c.check("...and its files stay on the NAS", nas.has(&format!("{there}/e2e-ficheiro.txt"), b"estava no Ambiente de trabalho\n"));
    let (cfg, log) = (svc.cfg.clone(), svc.log.clone());
    c.check("the service stops when asked to", svc.stop());
    let failed: Vec<String> = fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains("retrying in") || l.contains("could not populate") || l.contains("panicked")).map(str::to_string).collect();
    c.check("the service never failed at anything it tried", expect(failed.is_empty(), || failed.join("\n       ")));
    c.check("unregister: the folder stops being a sync folder", Service::unregister(&cfg).map(|_| ()));
    c.check("...with the Desktop still in its usual place", expect(same_path(&desktop_path(), &original), || desktop_path().display().to_string()));
    Ok(())
}

#[test]
fn windows_engine_end_to_end() {
    let dir = work_dir();
    // What an interrupted run left behind: registrations, then the folder.
    for name in ["a", "b"] {
        let cfg = dir.join(format!("cfg-{name}")).join("config.toml");
        if cfg.exists() {
            let _ = Service::unregister(&cfg);
        }
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create the work folder");
    assert!(fake_nas_bin().exists(), "{} is missing: `cargo test` builds it, `cargo build --example fake_nas` too", fake_nas_bin().display());
    println!("(work folder: {})", dir.display());

    let mut c = Checks::default();
    if let Err(e) = open_nas(&dir, &mut c) {
        c.bad("an ordinary NAS: could not go on", &e);
    }
    if std::env::var(KNOWN_FOLDERS_ENV).is_ok_and(|v| v == "1") {
        if let Err(e) = closed_top(&dir, &mut c) {
            c.bad("a NAS with a closed top: could not go on", &e);
        }
    } else if std::env::var_os("GITHUB_ACTIONS").is_some() {
        c.bad("the Desktop on a NAS with a closed top", &format!("not run: the workflow must set {KNOWN_FOLDERS_ENV}=1"));
    } else {
        println!("== skipped: the Desktop on a NAS with a closed top (it moves the Desktop of whoever runs it: {KNOWN_FOLDERS_ENV}=1, only on a machine that is thrown away)");
    }

    println!("== {} passed, {} failed", c.passed, c.failed.len());
    if !c.failed.is_empty() {
        for name in ["a", "b"] {
            println!("-- the end of service-{name}.log\n{}", tail(&dir.join(format!("service-{name}.log")), 60));
        }
        panic!("{} check(s) failed:\n  {}", c.failed.len(), c.failed.join("\n  "));
    }
}
