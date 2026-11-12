//! The long-running part of nasdrive: owns the settings, the mount and the
//! web interface. `nasdrive mount` runs it headless; `nasdrive ui` runs it and
//! opens the browser.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use nasdrive::config::{Config, DEFAULT_UI_PORT};
use nasdrive::remote::{Remote, RemoteError};
use nasdrive::webdav::WebDav;
use serde::Serialize;
use tokio::sync::Notify;

#[cfg(unix)]
pub use crate::unix::Mounted;
#[cfg(windows)]
pub use crate::win::Mounted;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `nasdrive mount`: no browser, exits if the mount goes away.
    Headless,
    /// `nasdrive ui`: opens the browser and keeps running for it.
    App,
    /// `nasdrive serve`: like `App`, without opening the browser. Started by
    /// the desktop app, which shows the interface in its own windows.
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
}

/// What every engine reports about itself (Linux/macOS VFS, Windows Cloud Files).
#[derive(Clone, Debug, Default, Serialize)]
pub struct MountInfo {
    pub online: bool,
    pub pending_uploads: u64,
    pub uploading: u64,
    /// `None` where the operating system manages the space (Windows).
    pub cache_bytes: Option<u64>,
    pub cache_max: Option<u64>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EntryView {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// `online`, `partial`, `cached`, `modified`, `uploading`, `local` or `folder`.
    pub state: String,
    /// `no`, `yes` or `inherited`.
    pub pinned: String,
    pub cached: u64,
    pub files: u64,
    pub files_cached: u64,
    pub pending: u64,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub path: String,
    pub this: EntryView,
    pub entries: Vec<EntryView>,
}

#[derive(Serialize)]
pub struct TestOutcome {
    pub ok: bool,
    pub items: usize,
    pub error: Option<String>,
    pub cert_problem: bool,
}

#[derive(Serialize)]
pub struct SaveOutcome {
    pub ok: bool,
    /// The system keyring refused the password; the interface then asks
    /// whether to keep it in the settings file instead.
    pub keyring_error: Option<String>,
}

pub struct Service {
    pub cfg_path: PathBuf,
    cfg: Mutex<Option<Config>>,
    password_known: AtomicBool,
    mount: Mutex<Option<Arc<Mounted>>>,
    /// Serialises start / stop / reconfigure.
    ctl: tokio::sync::Mutex<()>,
    phase: Mutex<Phase>,
    error: Mutex<Option<String>>,
    pub quit: Notify,
    exit_on_unmount: bool,
}

impl Service {
    pub fn new(cfg_path: PathBuf, mode: Mode) -> Arc<Self> {
        let (cfg, error) = match Config::load(&cfg_path) {
            Ok(c) => (Some(c), None),
            Err(_) if !cfg_path.exists() => (None, None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Arc::new(Self {
            cfg_path,
            cfg: Mutex::new(cfg),
            password_known: AtomicBool::new(false),
            mount: Mutex::new(None),
            ctl: tokio::sync::Mutex::new(()),
            phase: Mutex::new(Phase::Stopped),
            error: Mutex::new(error),
            quit: Notify::new(),
            exit_on_unmount: mode == Mode::Headless,
        })
    }

    pub fn config(&self) -> Option<Config> {
        self.cfg.lock().unwrap().clone()
    }

    pub fn phase(&self) -> Phase {
        *self.phase.lock().unwrap()
    }

    fn set_phase(&self, phase: Phase) {
        *self.phase.lock().unwrap() = phase;
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    fn set_error(&self, error: Option<String>) {
        *self.error.lock().unwrap() = error;
    }

    pub fn password_known(&self) -> bool {
        self.password_known.load(Ordering::SeqCst)
    }

    pub fn current(&self) -> Option<Arc<Mounted>> {
        self.mount.lock().unwrap().clone()
    }

    pub fn mounted(&self) -> Result<Arc<Mounted>> {
        self.current().ok_or_else(|| anyhow!("A NAS não está montada."))
    }

    /// Looks the password up once (keyring calls can be slow), not on every poll.
    pub async fn refresh_password_known(&self) {
        let known = match self.config() {
            Some(c) => tokio::task::spawn_blocking(move || c.password().is_ok()).await.unwrap_or(false),
            None => false,
        };
        self.password_known.store(known, Ordering::SeqCst);
    }

    pub async fn start_mount(self: &Arc<Self>) -> Result<()> {
        let _ctl = self.ctl.lock().await;
        if self.current().is_some() {
            return Ok(());
        }
        let cfg = self.config().ok_or_else(|| anyhow!("Ainda não está configurado."))?;
        self.set_phase(Phase::Starting);
        self.set_error(None);
        let mut attempt = 0;
        let result = loop {
            match Mounted::start(&cfg).await {
                Ok(m) => break Ok(m),
                // A session that just ended may still be letting go of the local index.
                Err(e) if attempt < 15 && format!("{e:#}").to_lowercase().contains("already open") => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => break Err(e),
            }
        };
        match result {
            Ok(m) => {
                let m = Arc::new(m);
                *self.mount.lock().unwrap() = Some(m.clone());
                self.set_phase(Phase::Running);
                self.watch(m);
                Ok(())
            }
            Err(e) => {
                log::error!("{e:#}");
                self.set_error(Some(format!("{e:#}")));
                self.set_phase(Phase::Stopped);
                Err(e)
            }
        }
    }

    /// Notices a mount that went away without us (unmounted from outside).
    fn watch(self: &Arc<Self>, m: Arc<Mounted>) {
        let me = self.clone();
        tokio::spawn(async move {
            m.wait_ended().await;
            let ours = me.mount.lock().unwrap().as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &m));
            if !ours {
                return; // stopped on purpose
            }
            log::warn!("the mount was removed from outside nasdrive");
            me.stop_mount().await;
            me.set_error(Some("A pasta foi desmontada fora do nasdrive.".into()));
            if me.exit_on_unmount {
                me.quit.notify_one();
            }
        });
    }

    pub async fn stop_mount(&self) {
        let _ctl = self.ctl.lock().await;
        let Some(m) = self.mount.lock().unwrap().take() else { return };
        self.set_phase(Phase::Stopping);
        m.stop().await;
        self.set_phase(Phase::Stopped);
    }

    /// Tries the given connection settings (`password` empty = the stored one).
    pub async fn test(&self, url: String, username: String, password: Option<String>, accept_invalid_certs: bool) -> TestOutcome {
        let fail = |msg: String, cert_problem: bool| TestOutcome { ok: false, items: 0, error: Some(msg), cert_problem };
        let password = match password.filter(|p| !p.is_empty()) {
            Some(p) => p,
            None => {
                // Same user (maybe another folder or address): reuse the stored password.
                let Some(c) = self.config().filter(|c| c.username == username) else { return fail("Indica a password.".into(), false) };
                match tokio::task::spawn_blocking(move || c.password()).await {
                    Ok(Ok(p)) => p,
                    _ => return fail("Indica a password.".into(), false),
                }
            }
        };
        let dav = match WebDav::new(&url, &username, &password, accept_invalid_certs) {
            Ok(d) => d,
            Err(e) => return fail(e.to_string(), false),
        };
        match dav.list("").await {
            Ok(entries) => TestOutcome { ok: true, items: entries.len(), error: None, cert_problem: false },
            Err(RemoteError::Network(m)) if !accept_invalid_certs && url.trim().starts_with("https") && m.to_lowercase().contains("cert") => {
                fail(format!("O certificado da NAS não pode ser verificado ({m})."), true)
            }
            Err(RemoteError::Unauthorized) => fail("Utilizador ou password errados.".into(), false),
            Err(RemoteError::NotFound) => fail("Essa pasta não existe na NAS (atenção às maiúsculas).".into(), false),
            Err(RemoteError::Forbidden) => fail("Esse utilizador não tem acesso a essa pasta.".into(), false),
            Err(e) if e.is_offline() => fail(format!("Não foi possível ligar à NAS: {e}"), false),
            Err(e) => fail(e.to_string(), false),
        }
    }

    /// Validates and saves new settings, stores the password, then (re)starts the mount.
    pub async fn save_config(self: &Arc<Self>, mut cfg: Config, password: Option<String>, store_in_file: bool) -> Result<SaveOutcome> {
        if cfg.url.trim().is_empty() || cfg.username.trim().is_empty() || cfg.mountpoint.as_os_str().is_empty() {
            bail!("Preenche o endereço, o utilizador e a pasta local.");
        }
        cfg.url = cfg.url.trim().to_string();
        cfg.username = cfg.username.trim().to_string();
        WebDav::new(&cfg.url, &cfg.username, "", false).map_err(|e| anyhow!("{e}"))?;
        cfg.cache_max_bytes()?;
        let mut password = password.filter(|p| !p.is_empty());
        let mut store_in_file = store_in_file;
        if let Some(old) = self.config() {
            if cfg.cache_dir.is_none() {
                cfg.cache_dir = old.cache_dir.clone();
            }
            if cfg.url == old.url && cfg.username == old.username {
                cfg.password = old.password.clone();
            } else if password.is_none() && cfg.username == old.username {
                // Same user, new address: carry the password over to the new entry.
                let in_file = old.password.is_some();
                if let Ok(Ok(pw)) = tokio::task::spawn_blocking(move || old.password()).await {
                    password = Some(pw);
                    store_in_file |= in_file;
                }
            }
        }
        match password {
            Some(pw) if store_in_file => cfg.password = Some(pw),
            Some(pw) => {
                let c = cfg.clone();
                match tokio::task::spawn_blocking(move || c.store_password(&pw)).await? {
                    Ok(()) => cfg.password = None,
                    Err(e) => return Ok(SaveOutcome { ok: false, keyring_error: Some(format!("{e:#}")) }),
                }
            }
            None => {
                let c = cfg.clone();
                if tokio::task::spawn_blocking(move || c.password().is_err()).await? {
                    bail!("Indica a password.");
                }
            }
        }
        cfg.save(&self.cfg_path)?;
        *self.cfg.lock().unwrap() = Some(cfg);
        self.password_known.store(true, Ordering::SeqCst);
        self.set_error(None);
        log::info!("settings saved");
        let me = self.clone();
        tokio::spawn(async move {
            me.stop_mount().await;
            let _ = me.start_mount().await;
        });
        Ok(SaveOutcome { ok: true, keyring_error: None })
    }
}

/// Runs the service until Ctrl+C, SIGTERM, or "quit" from the interface.
pub fn run(cfg_path: &Path, mode: Mode) -> Result<()> {
    if mode == Mode::Headless {
        Config::load(cfg_path)?; // fail early with the real reason
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let svc = Service::new(cfg_path.to_path_buf(), mode);
        svc.refresh_password_known().await;
        let port = svc.config().map_or(DEFAULT_UI_PORT, |c| c.ui_port);
        let ui = match crate::ui::serve(svc.clone(), port).await {
            Ok(ui) => {
                log::info!("interface on http://127.0.0.1:{} (open it with `nasdrive ui`)", ui.port);
                Some(ui)
            }
            Err(e) if mode != Mode::Headless => return Err(e),
            Err(e) => {
                log::warn!("the interface is not available: {e:#}");
                None
            }
        };
        if svc.config().is_some() {
            if let Err(e) = svc.start_mount().await {
                if mode == Mode::Headless {
                    return Err(e);
                }
            }
        }
        if let (Mode::App, Some(ui)) = (mode, &ui) {
            match crate::ui::open_path(&ui.url()) {
                Ok(()) => println!("nasdrive is running and the interface opened in your browser. Ctrl+C here stops it."),
                Err(_) => println!("Open {} in your browser. Ctrl+C here stops nasdrive.", ui.url()),
            }
        }
        wait_for_exit(&svc).await;
        svc.stop_mount().await;
        Ok(())
    })
}

async fn wait_for_exit(svc: &Service) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = svc.quit.notified() => {}
        _ = terminate() => {}
    }
}

#[cfg(unix)]
async fn terminate() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(windows)]
async fn terminate() {
    std::future::pending::<()>().await
}
