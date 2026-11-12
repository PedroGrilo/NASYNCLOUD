//! `nasdrive` command line.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use nasdrive::config::{Config, PASSWORD_ENV};
use nasdrive::remote::{Remote, RemoteError};
use nasdrive::webdav::WebDav;

#[cfg(unix)]
mod fuse;
mod logging;
mod service;
mod ui;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod win;

#[derive(Parser)]
#[command(
    name = "nasdrive",
    version,
    about = "Files on demand for a WebDAV NAS: browse everything, download only what you open.",
    long_about = "Files on demand for a WebDAV NAS: browse everything, download only what you open.\n\nWithout a command, opens the interface (and starts nasdrive if it is not running)."
)]
struct Cli {
    /// Configuration file (default: the per-user configuration folder).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// More detailed log output.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the interface in the browser, starting nasdrive if it is not running (the default).
    Ui,
    /// Ask for the NAS address and credentials in the terminal and save them.
    Setup,
    /// Mount the NAS, keep it in sync and serve the interface, without opening a browser (runs until Ctrl+C).
    Mount,
    /// Like `ui`, but without opening the browser (used by the desktop app).
    Serve,
    /// Unmount a mount left behind (Linux/macOS).
    #[cfg(unix)]
    Unmount,
    /// Keep files or folders available offline.
    #[cfg(unix)]
    Pin { paths: Vec<PathBuf> },
    /// Stop keeping files or folders offline (the local copy stays until space is needed).
    #[cfg(unix)]
    Unpin { paths: Vec<PathBuf> },
    /// Free up space: drop the local copy of files or folders now.
    #[cfg(unix)]
    Free { paths: Vec<PathBuf> },
    /// Show the sync state of paths, or of the whole mount when none is given.
    #[cfg(unix)]
    Status { paths: Vec<PathBuf> },
    /// List a folder with the state of each entry.
    #[cfg(unix)]
    Ls { dir: Option<PathBuf> },
    /// Re-read folders from the NAS right now.
    #[cfg(unix)]
    Refresh { paths: Vec<PathBuf> },
    /// Remove the sync folder registration from Windows (the files on the NAS are untouched).
    #[cfg(windows)]
    Unregister,
}

fn main() {
    let cli = Cli::parse();
    logging::init(cli.verbose);
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let path = match cli.config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let command = cli.command.unwrap_or(Command::Ui);
    #[cfg(unix)]
    if !matches!(command, Command::Mount | Command::Ui | Command::Serve) {
        // Let `nasdrive ls | head` end quietly instead of panicking on a closed pipe.
        // SAFETY: restoring the default disposition of a signal is always sound.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match command {
        Command::Ui => ui::open_or_run(&path),
        Command::Setup => setup(&path),
        Command::Mount => service::run(&path, service::Mode::Headless),
        Command::Serve => service::run(&path, service::Mode::Background),
        #[cfg(unix)]
        Command::Unmount => unix::force_unmount(&Config::load(&path)?.mountpoint()),
        #[cfg(unix)]
        Command::Pin { paths } => unix::set_all(&paths, nasdrive::vfs::XATTR_PIN, b"1", "kept offline"),
        #[cfg(unix)]
        Command::Unpin { paths } => unix::set_all(&paths, nasdrive::vfs::XATTR_PIN, b"0", "no longer kept offline"),
        #[cfg(unix)]
        Command::Free { paths } => unix::free(&paths),
        #[cfg(unix)]
        Command::Status { paths } if paths.is_empty() => unix::info(&Config::load(&path)?.mountpoint()),
        #[cfg(unix)]
        Command::Status { paths } => unix::status(&paths),
        #[cfg(unix)]
        Command::Ls { dir } => unix::ls(&dir.unwrap_or_else(|| PathBuf::from("."))),
        #[cfg(unix)]
        Command::Refresh { paths } => unix::set_all(&paths, nasdrive::vfs::XATTR_REFRESH, b"1", "refreshed"),
        #[cfg(windows)]
        Command::Unregister => win::unregister(&Config::load(&path)?),
    }
}

fn ask(question: &str, default: Option<&str>) -> Result<String> {
    loop {
        match default {
            Some(d) => print!("{question} [{d}]: "),
            None => print!("{question}: "),
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("no input");
        }
        match (line.trim(), default) {
            ("", Some(d)) => return Ok(d.to_string()),
            ("", None) => continue,
            (answer, _) => return Ok(answer.to_string()),
        }
    }
}

fn confirm(question: &str) -> Result<bool> {
    Ok(matches!(ask(&format!("{question} (y/n)"), Some("n"))?.to_lowercase().as_str(), "y" | "yes" | "s" | "sim"))
}

pub(crate) fn default_mountpoint() -> String {
    if cfg!(windows) {
        std::env::var("USERPROFILE").map(|p| format!("{p}\\NAS")).unwrap_or_else(|_| "C:\\NAS".into())
    } else {
        "~/NAS".into()
    }
}

/// Interactive first-time configuration, including a connection test.
fn setup(path: &Path) -> Result<()> {
    println!("Settings are saved in {}\n", path.display());
    let old = Config::load(path).ok();
    println!("WebDAV address of the folder to mount. On a UGREEN NAS: enable WebDAV in");
    println!("Control Panel > File Service, then use http://NAS_IP:5005/SharedFolder (or https://NAS_IP:5006/...).");
    let url = ask("Address", old.as_ref().map(|c| c.url.as_str()))?;
    let username = ask("Username", old.as_ref().map(|c| c.username.as_str()))?;
    let password = rpassword::prompt_password("Password: ")?;
    let mountpoint = ask("Local folder where the NAS should appear", Some(&old.as_ref().map_or_else(default_mountpoint, |c| c.mountpoint.display().to_string())))?;
    let mut cfg = old.unwrap_or_else(|| Config::new(String::new(), String::new(), PathBuf::new()));
    cfg.url = url;
    cfg.username = username;
    cfg.mountpoint = PathBuf::from(mountpoint);
    cfg.password = None;

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    loop {
        let dav = WebDav::new(&cfg.url, &cfg.username, &password, cfg.accept_invalid_certs)?;
        match rt.block_on(dav.list("")) {
            Ok(entries) => {
                println!("Connection OK: {} item(s) in that folder.", entries.len());
                break;
            }
            Err(RemoteError::Network(msg)) if !cfg.accept_invalid_certs && cfg.url.starts_with("https") && msg.to_lowercase().contains("cert") => {
                println!("The NAS presented a certificate that cannot be verified ({msg}).");
                println!("That is expected with the self-signed certificate UGOS ships by default.");
                if !confirm("Trust this NAS anyway?")? {
                    bail!("certificate not trusted");
                }
                cfg.accept_invalid_certs = true;
            }
            Err(e) => bail!("could not connect to {}: {e}", cfg.url),
        }
    }
    match cfg.store_password(&password) {
        Ok(()) => println!("Password stored in the system keyring."),
        Err(e) => {
            println!("The system keyring is not available ({e:#}).");
            if confirm("Store the password in the settings file instead (readable only by your user)?")? {
                cfg.password = Some(password);
            } else {
                println!("Then set the {PASSWORD_ENV} environment variable before running `nasdrive mount`.");
            }
        }
    }
    cfg.save(path)?;
    println!("\nAll set. Start it with `nasdrive ui` (opens the interface) or `nasdrive mount` (no interface window).");
    Ok(())
}

/// Builds the WebDAV client and checks that the configured folder is usable.
/// Returns `false` in the second slot when the NAS cannot be reached right now.
#[allow(dead_code)]
async fn connect(cfg: &Config) -> Result<(WebDav, bool)> {
    let dav = WebDav::new(&cfg.url, &cfg.username, &cfg.password()?, cfg.accept_invalid_certs)?;
    match dav.stat("").await {
        Ok(Some(e)) if e.is_dir => Ok((dav, true)),
        Ok(_) => bail!("{} is not a folder on the server", cfg.url),
        Err(e) if e.is_offline() => {
            log::warn!("the NAS is unreachable right now ({e})");
            Ok((dav, false))
        }
        Err(e) => Err(e).with_context(|| format!("cannot access {}", cfg.url)),
    }
}
