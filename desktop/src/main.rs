//! nasdrive in the tray (Windows) / menu bar (macOS), OneDrive-style.
//!
//! A thin shell: it runs the `nasdrive` binary next to it (`nasdrive serve`),
//! and shows the pages that one serves on 127.0.0.1: the compact panel
//! (`/panel`) next to the tray icon, and the full interface in a window.
//! Pages ask the shell for things by navigating to `/app/<action>`, which
//! is intercepted here (no IPC needed for pages served by another process).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, Rect, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

const PANEL_W: f64 = 360.0;
const PANEL_H: f64 = 560.0;

// ---------------------------------------------------------------- the nasdrive service

/// What `nasdrive serve` writes next to its settings: where its interface is.
#[derive(Deserialize, Clone)]
struct UiFile {
    port: u16,
    token: String,
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    // Same as nasdrive's own config.rs.
    directories::ProjectDirs::from("", "", "nasdrive")
}

fn ui_file() -> Option<UiFile> {
    let raw = std::fs::read(project_dirs()?.config_dir().join("ui.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn log_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = directories::BaseDirs::new() {
        return home.home_dir().join("Library/Logs/nasdrive.log");
    }
    project_dirs().map_or_else(std::env::temp_dir, |d| d.data_local_dir().to_path_buf()).join("nasdrive.log")
}

/// A tiny HTTP/1.1 request to the local service; returns the status code.
fn call(ui: &UiFile, method: &str, path: &str) -> Option<u16> {
    let addr = SocketAddr::from(([127, 0, 0, 1], ui.port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let body = if method == "POST" { "{}" } else { "" };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nx-nasdrive-token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        ui.port,
        ui.token,
        body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf).ok()?;
    std::str::from_utf8(&buf[..n]).ok()?.split(' ').nth(1)?.parse().ok()
}

fn alive() -> Option<UiFile> {
    ui_file().filter(|ui| call(ui, "GET", "/api/ping") == Some(200))
}

/// The `nasdrive` binary shipped next to this one.
fn service_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_file_name(if cfg!(windows) { "nasdrive.exe" } else { "nasdrive" })
}

enum StartError {
    MacFuseMissing,
    Failed(String),
}

/// Makes sure `nasdrive serve` is running (ours or one started before) and
/// answering; returns where its interface is.
fn ensure_service(child: &Mutex<Option<Child>>) -> Result<UiFile, StartError> {
    if let Some(ui) = alive() {
        return Ok(ui);
    }
    // nasdrive links against macFUSE's libfuse: without it, it cannot even start.
    if cfg!(target_os = "macos") && !std::path::Path::new("/usr/local/lib/libfuse.2.dylib").exists() {
        return Err(StartError::MacFuseMissing);
    }
    let log = log_path();
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = OpenOptions::new().create(true).append(true).open(&log).map_err(|e| StartError::Failed(format!("{}: {e}", log.display())))?;
    let err = out.try_clone().map_err(|e| StartError::Failed(e.to_string()))?;
    let mut cmd = Command::new(service_binary());
    cmd.arg("serve").stdin(Stdio::null()).stdout(out).stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console window
    }
    let mut proc = cmd.spawn().map_err(|e| StartError::Failed(format!("{}: {e}", service_binary().display())))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(ui) = alive() {
            *child.lock().unwrap() = Some(proc);
            return Ok(ui);
        }
        if let Ok(Some(status)) = proc.try_wait() {
            return Err(StartError::Failed(format!("nasdrive ended ({status})")));
        }
        if Instant::now() > deadline {
            let _ = proc.kill();
            return Err(StartError::Failed("nasdrive did not answer".into()));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

// ---------------------------------------------------------------- shell state

struct Shell {
    /// The `nasdrive serve` we started, if we did.
    child: Mutex<Option<Child>>,
    /// Serializes starts, so two clicks do not launch two services.
    starting: Mutex<()>,
    ui: Mutex<Option<UiFile>>,
    /// The tray icon's place on screen, from its last click.
    last_rect: Mutex<Option<Rect>>,
    /// When the panel last hid itself on losing focus: a click on the tray
    /// icon blurs it first, and that same click must not show it again.
    hidden_at: Mutex<Option<Instant>>,
}

fn shell(app: &AppHandle) -> tauri::State<'_, Shell> {
    app.state::<Shell>()
}

/// What the panel should load now, plus data for the local error page.
fn panel_target(app: &AppHandle) -> (WebviewUrl, Option<String>) {
    let sh = shell(app);
    let _one_at_a_time = sh.starting.lock().unwrap();
    match ensure_service(&sh.child) {
        Ok(ui) => {
            let url = format!("http://127.0.0.1:{}/panel#t={}&shell=1", ui.port, ui.token);
            *sh.ui.lock().unwrap() = Some(ui);
            (WebviewUrl::External(url.parse().expect("panel URL")), None)
        }
        Err(StartError::MacFuseMissing) => (WebviewUrl::App("index.html".into()), Some(r#"window.__nasdrive = { reason: "macfuse" };"#.into())),
        Err(StartError::Failed(msg)) => {
            eprintln!("nasdrive: {msg}");
            let info = serde_json::json!({ "reason": "failed", "log": log_path().display().to_string() });
            (WebviewUrl::App("index.html".into()), Some(format!("window.__nasdrive = {info};")))
        }
    }
}

fn panel(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("panel")
}

fn build_panel(app: &AppHandle) -> Option<WebviewWindow> {
    let (url, init) = panel_target(app);
    let nav = app.clone();
    let mut b = WebviewWindowBuilder::new(app, "panel", url)
        .title("nasdrive")
        .inner_size(PANEL_W, PANEL_H)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(cfg!(target_os = "macos"))
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .on_navigation(move |url| on_navigation(&nav, url));
    if let Some(js) = init {
        b = b.initialization_script(&js);
    }
    let win = match b.build() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("nasdrive: cannot create the panel: {e}");
            return None;
        }
    };
    let blur = app.clone();
    win.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            if let Some(p) = panel(&blur) {
                if p.is_visible().unwrap_or(false) {
                    let _ = p.hide();
                    *shell(&blur).hidden_at.lock().unwrap() = Some(Instant::now());
                }
            }
        }
    });
    Some(win)
}

/// Puts the panel next to the tray icon: under it when the icon is at the top
/// of the screen (macOS menu bar), above it when at the bottom (Windows taskbar).
fn place_panel(win: &WebviewWindow, rect: Option<Rect>) {
    let scale = win.scale_factor().unwrap_or(1.0);
    let (w, h, gap) = (PANEL_W * scale, PANEL_H * scale, 6.0 * scale);
    let monitor = win.primary_monitor().ok().flatten();
    let (mx, my, mw, mh) = monitor.as_ref().map_or((0.0, 0.0, 1920.0 * scale, 1080.0 * scale), |m| {
        (m.position().x as f64, m.position().y as f64, m.size().width as f64, m.size().height as f64)
    });
    let (x, y) = match rect {
        Some(r) => {
            let p = r.position.to_physical::<f64>(scale);
            let s = r.size.to_physical::<f64>(scale);
            let top = p.y < my + mh / 2.0;
            (p.x + s.width / 2.0 - w / 2.0, if top { p.y + s.height + gap } else { p.y - h - gap })
        }
        // No click yet: the usual corner for each system.
        None if cfg!(target_os = "macos") => (mx + mw - w - 12.0 * scale, my + 30.0 * scale),
        None => (mx + mw - w - 12.0 * scale, my + mh - h - 56.0 * scale),
    };
    let x = x.clamp(mx + gap, (mx + mw - w - gap).max(mx));
    let y = y.clamp(my, (my + mh - h).max(my));
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

fn show_panel(app: &AppHandle) {
    let rect = *shell(app).last_rect.lock().unwrap();
    let Some(win) = panel(app).or_else(|| build_panel(app)) else { return };
    place_panel(&win, rect);
    let _ = win.show();
    let _ = win.set_focus();
}

fn toggle_panel(app: &AppHandle) {
    let just_hidden = shell(app).hidden_at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(300));
    match panel(app) {
        Some(w) if w.is_visible().unwrap_or(false) => {
            let _ = w.hide();
        }
        _ if just_hidden => {}
        _ => show_panel(app),
    }
}

fn hide_panel(app: &AppHandle) {
    if let Some(w) = panel(app) {
        let _ = w.hide();
    }
}

/// Pages ask for things by navigating to `/app/<action>?...`.
fn on_navigation(app: &AppHandle, url: &Url) -> bool {
    let Some(action) = url.path().strip_prefix("/app/").map(str::to_string) else { return true };
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let app = app.clone();
    // Act outside the webview's own callback.
    std::thread::spawn(move || match action.as_str() {
        "main" => show_main(&app, query.get("view").map_or("", String::as_str)),
        "hide" => hide_panel(&app),
        "quit" => quit(&app),
        "retry" | "restart" => restart(&app),
        "web" => open_web(query.get("url").map_or("", String::as_str)),
        _ => {}
    });
    false
}

// ---------------------------------------------------------------- full window, restart, quit

/// The full interface in a normal window (`view`: estado, ficheiros, atividade, definicoes).
fn show_main(app: &AppHandle, view: &str) {
    hide_panel(app);
    let view: String = view.chars().filter(char::is_ascii_lowercase).collect();
    let ui = shell(app).ui.lock().unwrap().clone().or_else(alive);
    let Some(ui) = ui else { return show_panel(app) };
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(format!("location.hash = '#/{view}'"));
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let url = format!("http://127.0.0.1:{}/#t={}&view={view}", ui.port, ui.token);
    let nav = app.clone();
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse().expect("main URL")))
        .title("nasdrive")
        .inner_size(1100.0, 760.0)
        .min_inner_size(420.0, 560.0)
        .center()
        .on_navigation(move |url| on_navigation(&nav, url))
        .build();
    match built {
        Ok(w) => {
            // While the window is open, show nasdrive in the Dock and app switcher.
            #[cfg(target_os = "macos")]
            {
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
                let closed = app.clone();
                w.on_window_event(move |e| {
                    if let WindowEvent::Destroyed = e {
                        let _ = closed.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    }
                });
            }
            let _ = w.set_focus();
        }
        Err(e) => eprintln!("nasdrive: cannot open the window: {e}"),
    }
}

/// Starts the service again if needed and reloads the panel.
fn restart(app: &AppHandle) {
    if let Some(w) = panel(app) {
        let _ = w.destroy();
    }
    show_panel(app);
}

fn open_web(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(windows)]
    let _ = Command::new("explorer").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

/// Stops the service (it finishes pending uploads and unmounts first), then exits.
fn quit(app: &AppHandle) {
    hide_panel(app);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    if let Some(ui) = shell(app).ui.lock().unwrap().clone().or_else(ui_file) {
        let _ = call(&ui, "POST", "/api/quit");
    }
    let child = shell(app).child.lock().unwrap().take();
    if let Some(mut c) = child {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline && !matches!(c.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    app.exit(0);
}

// ---------------------------------------------------------------- tray, menus, main

fn tray_icon() -> tauri::image::Image<'static> {
    // macOS: a black "template" glyph the system tints for light/dark menu bars.
    #[cfg(target_os = "macos")]
    let bytes: &[u8] = include_bytes!("../icons/tray-template.png");
    #[cfg(not(target_os = "macos"))]
    let bytes: &[u8] = include_bytes!("../icons/tray.png");
    tauri::image::Image::from_bytes(bytes).expect("tray icon")
}

/// macOS app menu, shown while the full window is open. The Edit menu is what
/// makes copy and paste work in the settings fields.
#[cfg(target_os = "macos")]
fn app_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri::menu::Submenu;
    let quit = MenuItem::with_id(app, "quit", "Sair do nasdrive", true, Some("CmdOrCtrl+Q"))?;
    let main = Submenu::with_items(app, "nasdrive", true, &[&PredefinedMenuItem::hide(app, Some("Esconder"))?, &PredefinedMenuItem::separator(app)?, &quit])?;
    let edit = Submenu::with_items(
        app,
        "Editar",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("Desfazer"))?,
            &PredefinedMenuItem::redo(app, Some("Refazer"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some("Cortar"))?,
            &PredefinedMenuItem::copy(app, Some("Copiar"))?,
            &PredefinedMenuItem::paste(app, Some("Colar"))?,
            &PredefinedMenuItem::select_all(app, Some("Selecionar tudo"))?,
        ],
    )?;
    let window = Submenu::with_items(app, "Janela", true, &[&PredefinedMenuItem::minimize(app, Some("Minimizar"))?, &PredefinedMenuItem::close_window(app, Some("Fechar"))?])?;
    Menu::with_items(app, &[&main, &edit, &window])
}

fn on_menu(app: &AppHandle, id: &str) {
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || match id.as_str() {
        "open" => show_main(&app, ""),
        "panel" => show_panel(&app),
        "quit" => quit(&app),
        _ => {}
    });
}

fn main() {
    let builder = tauri::Builder::default().manage(Shell {
        child: Mutex::new(None),
        starting: Mutex::new(()),
        ui: Mutex::new(None),
        last_rect: Mutex::new(None),
        hidden_at: Mutex::new(None),
    });
    #[cfg(target_os = "macos")]
    let builder = builder.menu(app_menu);
    let app = builder
        .on_menu_event(|app, e| on_menu(app, e.id().as_ref()))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let open = MenuItem::with_id(app, "open", "Abrir o nasdrive", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Sair do nasdrive", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(tray_icon())
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip("nasdrive")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = e {
                        let app = tray.app_handle().clone();
                        *shell(&app).last_rect.lock().unwrap() = Some(rect);
                        std::thread::spawn(move || toggle_panel(&app));
                    }
                })
                .build(app)?;
            // Start the service now and keep the panel ready. The first time
            // (nothing configured yet), go straight to the settings.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let first_time = project_dirs().is_some_and(|d| !d.config_dir().join("config.toml").exists());
                if build_panel(&handle).is_some() {
                    if first_time {
                        show_main(&handle, "definicoes");
                    } else if std::env::args().any(|a| a == "--panel") {
                        show_panel(&handle);
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("cannot start nasdrive");
    app.run(|app, event| match event {
        // Closing windows does not quit: nasdrive stays in the tray.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        // Opening the app again (Finder, Spotlight) shows the panel.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            let app = app.clone();
            std::thread::spawn(move || show_panel(&app));
        }
        // Leaving without "Sair" (e.g. logging out): ask our service to stop too.
        RunEvent::Exit if shell(app).child.lock().unwrap().is_some() => {
            if let Some(ui) = ui_file() {
                let _ = call(&ui, "POST", "/api/quit");
            }
        }
        _ => {}
    });
}
