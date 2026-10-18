use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tao::event::WindowEvent;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use tokenshare_core::config::{default_config_path, default_data_dir};
use tokenshare_core::daemon::{self, Running};
use tokenshare_core::status::{health, Health};
use tokenshare_core::table::Status;
use tokenshare_core::{AppState, Config};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use wry::{WebView, WebViewBuilder};

use crate::icon;
use crate::settings::{self, Message};
use tokenshare_core::settings::{apply, Form};

/// Everything the menu shows; only sent to the UI thread when it changes.
#[derive(Clone, PartialEq)]
struct Snapshot {
    health: Health,
    headline: String,
    pool: String,
    peers: Vec<String>,
}

enum UserEvent {
    Snapshot(Snapshot),
    Menu(MenuEvent),
    /// A message from the settings page.
    Settings(String),
}

/// The running delegate as the UI sees it. It changes when settings are saved.
#[derive(Default)]
struct Current {
    state: Option<Arc<AppState>>,
    error: Option<String>,
}

type Shared = Arc<Mutex<Current>>;

struct SettingsWindow {
    // The webview must be dropped before its window.
    webview: WebView,
    window: Window,
}

fn label(h: Health) -> String {
    let text = match h {
        Health::Active => "Active",
        Health::InUse => "In use",
        Health::NoModels => "No models available",
        Health::Down => "Unavailable",
    };
    format!("{} {text}", icon::emoji(h))
}

fn snapshot(current: &Shared) -> Snapshot {
    let c = current.lock().unwrap();
    let Some(s) = &c.state else {
        return Snapshot {
            health: Health::Down,
            headline: format!("Token Share: {} Not running", icon::emoji(Health::Down)),
            pool: c.error.clone().unwrap_or_default(),
            peers: vec![],
        };
    };
    let h = health(s);
    let peers = s
        .delegates
        .peers()
        .into_iter()
        .map(|p| {
            let st = match p.status {
                Status::Online => "online",
                Status::Unavailable => "unavailable",
                Status::Inactive => "inactive",
            };
            let models = if p.models.is_empty() {
                "no models".to_string()
            } else {
                p.models.join(", ")
            };
            format!("{}  ({st})  {models}", p.url)
        })
        .collect();
    Snapshot {
        health: h,
        headline: {
            let mut notes = vec![];
            if s.config.masquerade {
                notes.push("masquerading");
            }
            if s.config.ollama_hosts.is_empty() {
                notes.push("client only");
            } else if !s.config.share_with_peers {
                notes.push("not sharing");
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };
            format!("Token Share: {}{notes}", label(h))
        },
        pool: format!(
            "Requests: {} local of {}, {} forwarded",
            s.pool.in_use(),
            s.pool.size(),
            s.pool.forwarding()
        ),
        peers,
    }
}

fn icon_for(h: Health) -> Icon {
    Icon::from_rgba(icon::render(h, icon::SIZE), icon::SIZE, icon::SIZE).expect("valid icon")
}

fn init_logging(data_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("tokenshare.log"))
    {
        let _ = tracing_subscriber::fmt()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .try_init();
    }
}

fn config_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(PathBuf::from);
        }
    }
    default_config_path()
}

pub fn run() {
    let data_dir = default_data_dir().unwrap_or_else(|| PathBuf::from("."));
    init_logging(&data_dir);
    let config_path = config_path();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let start_cfg_path = config_path.clone();
    let start = move |rt: &tokio::runtime::Runtime| -> Result<Running, String> {
        let config = match &start_cfg_path {
            Some(p) => Config::load(p).map_err(|e| format!("{e:#}"))?,
            None => Config::default(),
        };
        rt.block_on(daemon::start(config, data_dir.clone()))
            .map_err(|e| format!("{e:#}"))
    };
    let mut running = match start(&rt) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!(error = %e, "delegate failed to start");
            None
        }
    };
    let current: Shared = Arc::new(Mutex::new(Current::default()));
    {
        let mut c = current.lock().unwrap();
        c.state = running.as_ref().map(|r| r.state.clone());
        c.error = if running.is_none() {
            Some("failed to start; see tokenshare.log".into())
        } else {
            None
        };
    }

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // Menubar only: no Dock icon.
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));
    spawn_poller(current.clone(), proxy.clone());

    let headline = MenuItem::new("Token Share: starting", false, None);
    let pool = MenuItem::new("", false, None);
    let peers = Submenu::new("Delegates", true);
    let settings_item = MenuItem::new("Settings…", config_path.is_some(), None);
    let settings_id: MenuId = settings_item.id().clone();
    let quit = MenuItem::new("Quit Token Share", true, None);
    let quit_id: MenuId = quit.id().clone();
    let mut tray: Option<TrayIcon> = None;
    let mut settings_window: Option<SettingsWindow> = None;
    let open_at_start = std::env::args().any(|a| a == "--open-settings");
    let initial = if running.is_some() {
        Health::Active
    } else {
        Health::Down
    };

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray icon must be created once the loop is running (macOS).
            Event::NewEvents(StartCause::Init) => {
                let menu = Menu::new();
                let _ = menu.append_items(&[
                    &headline,
                    &pool,
                    &peers,
                    &PredefinedMenuItem::separator(),
                    &settings_item,
                    &PredefinedMenuItem::separator(),
                    &quit,
                ]);
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("Token Share")
                    .with_icon(icon_for(initial))
                    .build()
                    .map_err(|e| tracing::error!(error = %e, "cannot create tray icon"))
                    .ok();
                if open_at_start {
                    settings_window = open_settings(target, &proxy, &config_path)
                        .map_err(|e| tracing::error!(error = %e, "cannot open settings"))
                        .ok();
                }
            }
            Event::UserEvent(UserEvent::Snapshot(s)) => {
                headline.set_text(&s.headline);
                pool.set_text(&s.pool);
                for item in peers.items() {
                    let _ = peers.remove(item.as_menuitem().expect("only text items"));
                }
                if s.peers.is_empty() {
                    let _ = peers.append(&MenuItem::new("No other delegates", false, None));
                }
                for p in &s.peers {
                    let _ = peers.append(&MenuItem::new(p, false, None));
                }
                if let Some(t) = &tray {
                    let _ = t.set_icon(Some(icon_for(s.health)));
                    let _ = t.set_tooltip(Some(&s.headline));
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == settings_id => match &settings_window {
                Some(w) => w.window.set_focus(),
                None => {
                    settings_window = open_settings(target, &proxy, &config_path)
                        .map_err(|e| tracing::error!(error = %e, "cannot open settings"))
                        .ok();
                }
            },
            Event::UserEvent(UserEvent::Settings(msg)) => {
                let Some(w) = &settings_window else { return };
                match serde_json::from_str::<Message>(&msg) {
                    Ok(Message::Copy { text }) => {
                        if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text))
                        {
                            tracing::warn!(error = %e, "cannot write clipboard");
                        }
                    }
                    Ok(Message::Resize { height }) => {
                        // Title bar is outside the inner size; clamp to something sane.
                        let h = height.clamp(300.0, 900.0);
                        w.window
                            .set_inner_size(tao::dpi::LogicalSize::new(460.0, h));
                    }
                    Ok(Message::Paste) => {
                        let text = arboard::Clipboard::new()
                            .and_then(|mut c| c.get_text())
                            .unwrap_or_default();
                        let json = serde_json::to_string(&text).expect("string serializes");
                        let _ = w.webview.evaluate_script(&format!("insertText({json})"));
                    }
                    Ok(Message::Cancel) => {
                        settings_window = None;
                        set_regular_app(target, false);
                    }
                    Ok(Message::Save { form, force }) => {
                        let result = save_and_restart(
                            &form,
                            &config_path,
                            &mut running,
                            &current,
                            &rt,
                            &start,
                            force,
                        );
                        match result {
                            Ok(()) => {
                                settings_window = None;
                                set_regular_app(target, false);
                            }
                            Err(SaveError::Invalid(errors)) => show_errors(w, &errors),
                            Err(SaveError::Warning(text)) => {
                                let json = serde_json::to_string(&text).expect("string serializes");
                                let _ = w.webview.evaluate_script(&format!("showWarning({json})"));
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "bad message from settings page"),
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } if settings_window
                .as_ref()
                .is_some_and(|w| w.window.id() == window_id) =>
            {
                settings_window = None;
                set_regular_app(target, false);
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == quit_id => {
                settings_window = None;
                if let Some(r) = running.take() {
                    if let Err(e) = rt.block_on(r.stop(Duration::from_secs(5))) {
                        tracing::warn!(error = %e, "shutdown");
                    }
                }
                tray.take();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

enum SaveError {
    /// Problems with the values; fix them and save again.
    Invalid(Vec<String>),
    /// A heads-up that does not block: saving again (`force`) goes ahead.
    Warning(String),
}

/// Validate, write the config, and restart the delegate with it.
fn save_and_restart(
    form: &Form,
    config_path: &Option<PathBuf>,
    running: &mut Option<Running>,
    current: &Shared,
    rt: &tokio::runtime::Runtime,
    start: &dyn Fn(&tokio::runtime::Runtime) -> Result<Running, String>,
    force: bool,
) -> Result<(), SaveError> {
    let invalid = |msg: String| SaveError::Invalid(vec![msg]);
    let path = config_path
        .as_ref()
        .ok_or_else(|| invalid("no config location".into()))?;
    let base = Config::load(path).map_err(|e| invalid(format!("{e:#}")))?;
    let config = apply(form, &base).map_err(SaveError::Invalid)?;

    // Warn if a port we are about to take is busy. Our own current ports do
    // not count: they are freed by the restart.
    let ours = running.as_ref().map(|r| r.ports()).unwrap_or_default();
    let busy = config
        .listen_ports()
        .into_iter()
        .find(|p| !ours.contains(p) && tokenshare_core::config::port_in_use(*p));
    if let (false, Some(port)) = (force, busy) {
        let who = if port == tokenshare_core::config::OLLAMA_PORT {
            "probably ollama itself"
        } else {
            "by another program"
        };
        return Err(SaveError::Warning(format!(
            "Port {port} is already in use ({who}). The delegate cannot start until it is \
             free: stop that program first, or press Save Anyway to save the setting."
        )));
    }

    config.save(path).map_err(|e| invalid(format!("{e:#}")))?;
    tracing::info!("settings saved, restarting delegate");

    if let Some(r) = running.take() {
        if let Err(e) = rt.block_on(r.stop(Duration::from_secs(5))) {
            tracing::warn!(error = %e, "shutdown before restart");
        }
    }
    current.lock().unwrap().state = None;
    match start(rt) {
        Ok(r) => {
            let mut c = current.lock().unwrap();
            c.state = Some(r.state.clone());
            c.error = None;
            *running = Some(r);
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "delegate failed to restart");
            current.lock().unwrap().error = Some(e.clone());
            Err(invalid(format!(
                "Saved, but the delegate could not start: {e}"
            )))
        }
    }
}

/// Watch the delegate and tell the UI thread whenever the picture changes.
fn spawn_poller(current: Shared, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let mut last: Option<Snapshot> = None;
        loop {
            let now = snapshot(&current);
            if last.as_ref() != Some(&now) {
                if proxy.send_event(UserEvent::Snapshot(now.clone())).is_err() {
                    return; // UI is gone
                }
                last = Some(now);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Open the settings window with the values currently on disk.
fn open_settings(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: &EventLoopProxy<UserEvent>,
    config_path: &Option<PathBuf>,
) -> Result<SettingsWindow, String> {
    let config = match config_path {
        Some(p) => Config::load(p).map_err(|e| format!("{e:#}"))?,
        None => return Err("no config location on this system".into()),
    };
    set_regular_app(target, true);
    let window = WindowBuilder::new()
        .with_title("Token Share Settings")
        .with_inner_size(tao::dpi::LogicalSize::new(460.0, 500.0))
        .with_resizable(false)
        .build(target)
        .map_err(|e| {
            set_regular_app(target, false);
            e.to_string()
        })?;
    let ipc = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(settings::page(&Form::from_config(&config)))
        .with_ipc_handler(move |req| {
            let _ = ipc.send_event(UserEvent::Settings(req.into_body()));
        })
        .build(&window)
        .map_err(|e| {
            set_regular_app(target, false);
            e.to_string()
        })?;
    window.set_focus();
    Ok(SettingsWindow { webview, window })
}

/// While the settings window is open, behave like a regular app so it gets the
/// menu bar (and with it Cmd+C/V/A) and proper focus; revert to menubar-only after.
#[cfg(target_os = "macos")]
fn set_regular_app(target: &EventLoopWindowTarget<UserEvent>, regular: bool) {
    use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
    target.set_activation_policy_at_runtime(if regular {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    });
}

#[cfg(not(target_os = "macos"))]
fn set_regular_app(_: &EventLoopWindowTarget<UserEvent>, _: bool) {}

fn show_errors(w: &SettingsWindow, errors: &[String]) {
    let json = serde_json::to_string(errors).expect("strings serialize");
    let _ = w.webview.evaluate_script(&format!("showErrors({json})"));
}
