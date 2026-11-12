//! Configuration file and credential handling.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

pub const PASSWORD_ENV: &str = "NASDRIVE_PASSWORD";
/// Default port of the local web interface.
pub const DEFAULT_UI_PORT: u16 = 8730;
#[cfg(feature = "keyring")]
const KEYRING_SERVICE: &str = "nasdrive";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// WebDAV address of the folder to mount, e.g. `http://192.168.1.50:5005/Documentos`.
    pub url: String,
    pub username: String,
    /// Plain-text password. Prefer the system keyring (`nasdrive setup`) or the
    /// `NASDRIVE_PASSWORD` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Where the NAS shows up: an empty folder on Linux/macOS, the sync folder on Windows.
    pub mountpoint: PathBuf,
    /// Accept self-signed HTTPS certificates (the UGOS default certificate is one).
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Where cached file contents and the local index live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Upper bound for cached (non-pinned) data, e.g. "20G". Linux/macOS only:
    /// on Windows the system manages the space itself.
    #[serde(default = "default_cache_max")]
    pub cache_max_size: String,
    /// How long a folder listing is trusted before asking the server again.
    #[serde(default = "default_dir_ttl")]
    pub dir_ttl_secs: u64,
    /// How often pinned ("keep offline") folders are re-checked for changes.
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Quiet time after a file is closed before it is uploaded.
    #[serde(default = "default_upload_delay")]
    pub upload_delay_secs: u64,
    #[serde(default = "default_parallel")]
    pub parallel_uploads: usize,
    /// How far ahead of the reader a streaming download may run.
    #[serde(default = "default_readahead")]
    pub readahead_mb: u64,
    /// Upload to a temporary name and rename at the end, so an interrupted
    /// upload never leaves a truncated file in place of the good one.
    #[serde(default = "default_true")]
    pub atomic_upload: bool,
    /// Port of the local web interface (http://127.0.0.1:PORT).
    #[serde(default = "default_ui_port")]
    pub ui_port: u16,
}

fn default_cache_max() -> String {
    "20G".into()
}
fn default_dir_ttl() -> u64 {
    30
}
fn default_poll() -> u64 {
    300
}
fn default_upload_delay() -> u64 {
    2
}
fn default_parallel() -> usize {
    2
}
fn default_readahead() -> u64 {
    8
}
fn default_true() -> bool {
    true
}
fn default_ui_port() -> u16 {
    DEFAULT_UI_PORT
}

impl Config {
    pub fn new(url: String, username: String, mountpoint: PathBuf) -> Self {
        Self {
            url,
            username,
            password: None,
            mountpoint,
            accept_invalid_certs: false,
            cache_dir: None,
            cache_max_size: default_cache_max(),
            dir_ttl_secs: default_dir_ttl(),
            poll_interval_secs: default_poll(),
            upload_delay_secs: default_upload_delay(),
            parallel_uploads: default_parallel(),
            readahead_mb: default_readahead(),
            atomic_upload: true,
            ui_port: DEFAULT_UI_PORT,
        }
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(project_dirs()?.config_dir().join("config.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {} (run `nasdrive setup` first)", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("invalid configuration in {}", path.display()))?;
        cfg.cache_max_bytes()?;
        if cfg.url.trim().is_empty() || cfg.username.trim().is_empty() {
            bail!("`url` and `username` must be set in {}", path.display());
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn mountpoint(&self) -> PathBuf {
        expand_tilde(&self.mountpoint)
    }

    /// Per-remote state directory, so two configurations never share a cache.
    pub fn state_dir(&self) -> Result<PathBuf> {
        let base = match &self.cache_dir {
            Some(dir) => expand_tilde(dir),
            None => project_dirs()?.cache_dir().to_path_buf(),
        };
        Ok(base.join(format!("{:016x}", fnv1a(format!("{}\n{}", self.url.trim_end_matches('/'), self.username).as_bytes()))))
    }

    pub fn cache_max_bytes(&self) -> Result<u64> {
        parse_size(&self.cache_max_size).with_context(|| format!("invalid cache_max_size {:?} (examples: \"500M\", \"20G\")", self.cache_max_size))
    }

    fn keyring_account(&self) -> String {
        format!("{}@{}", self.username, self.url.trim_end_matches('/'))
    }

    /// Password lookup order: environment, configuration file, system keyring.
    pub fn password(&self) -> Result<String> {
        if let Ok(pw) = std::env::var(PASSWORD_ENV) {
            if !pw.is_empty() {
                return Ok(pw);
            }
        }
        if let Some(pw) = &self.password {
            return Ok(pw.clone());
        }
        #[cfg(feature = "keyring")]
        {
            match keyring::Entry::new(KEYRING_SERVICE, &self.keyring_account()).and_then(|e| e.get_password()) {
                Ok(pw) => return Ok(pw),
                Err(e) => log::debug!("keyring lookup failed: {e}"),
            }
        }
        Err(anyhow!("no password found: run `nasdrive setup`, or set the {PASSWORD_ENV} environment variable"))
    }

    /// Stores the password in the system keyring. Fails when there is none
    /// (headless Linux, for instance); the caller then falls back to the file.
    pub fn store_password(&self, password: &str) -> Result<()> {
        #[cfg(feature = "keyring")]
        {
            let entry = keyring::Entry::new(KEYRING_SERVICE, &self.keyring_account())?;
            entry.set_password(password)?;
            // Read it back: some stores accept writes they cannot serve later.
            if entry.get_password()? != password {
                bail!("the keyring did not return the stored password");
            }
            Ok(())
        }
        #[cfg(not(feature = "keyring"))]
        {
            let _ = password;
            bail!("built without keyring support")
        }
    }
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "nasdrive").ok_or_else(|| anyhow!("cannot determine the home directory"))
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(dirs) = BaseDirs::new() {
            return dirs.home_dir().join(rest);
        }
    }
    path.to_path_buf()
}

/// Parses sizes such as `500M`, `20G`, `1.5T` or plain bytes (binary multiples).
pub fn parse_size(text: &str) -> Result<u64> {
    let t = text.trim().to_ascii_uppercase();
    let t = t.trim_end_matches("IB").trim_end_matches('B');
    let (num, mult) = match t.chars().last() {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        Some('T') => (&t[..t.len() - 1], 1 << 40),
        _ => (t, 1),
    };
    let value: f64 = num.trim().parse().map_err(|_| anyhow!("not a number: {num:?}"))?;
    if !value.is_finite() || value < 0.0 {
        bail!("size must be positive");
    }
    Ok((value * mult as f64) as u64)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20G").unwrap(), 20 << 30);
        assert_eq!(parse_size("500 MB").unwrap(), 500 << 20);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 << 29);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(12), "12 B");
    }

    #[test]
    fn defaults_fill_in_missing_keys() {
        let cfg: Config = toml::from_str("url = \"http://nas:5005/x\"\nusername = \"pedro\"\nmountpoint = \"~/NAS\"\n").unwrap();
        assert_eq!(cfg.dir_ttl_secs, 30);
        assert!(cfg.atomic_upload);
        assert_eq!(cfg.cache_max_bytes().unwrap(), 20 << 30);
        let again: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(again.url, cfg.url);
    }
}
