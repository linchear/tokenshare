use std::collections::HashMap;
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
use crate::notices::{self, Notice, Notifier};
use crate::settings::{self, Message};
#[cfg(target_os = "macos")]
use crate::update;
use tokenshare_core::settings::{apply, Form};

/// Everything the menu shows; only sent to the UI thread when it changes.
#[derive(Clone, PartialEq)]
struct Snapshot {
    health: Health,
    /// Whether the delegate is running; Open Dashboard is greyed out otherwise.
    running: bool,
    headline: String,
    pool: String,
    peers: Vec<PeerRow>,
}

/// One delegate in the menu: its id (for Forget) and what to show.
#[derive(Clone, PartialEq)]
struct PeerRow {
    id: String,
    label: String,
}

enum UserEvent {
    Snapshot(Snapshot),
    Menu(MenuEvent),
    /// A message from the settings page.
    Settings(String),
    /// The result of looking for a new version; `manual` if the user asked.
    #[cfg(target_os = "macos")]
    UpdateChecked {
        manual: bool,
        result: Result<Option<Box<cargo_packager_updater::Update>>, String>,
    },
    /// A new version was (or could not be) put in place of this one.
    #[cfg(target_os = "macos")]
    UpdateInstalled(Result<(), String>),
}

/// The running delegate as the UI sees it. It changes when settings are saved.
#[derive(Default)]
struct Current {
    state: Option<Arc<AppState>>,
    error: Option<String>,
    /// The `notifications` setting, known even when the delegate failed to start.
    notify: bool,
    /// The `check_for_updates` setting.
    check_updates: bool,
}

type Shared = Arc<Mutex<Current>>;

/// Width of the settings window in logical pixels; its height follows the content.
const SETTINGS_WIDTH: f64 = 580.0;

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
            running: false,
            headline: format!("Token Share: {} Not running", icon::emoji(Health::Down)),
            pool: c.error.as_deref().map(menu_error).unwrap_or_default(),
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
            let who = match &p.name {
                Some(name) => format!("{name}  {}", p.url),
                None => p.url.clone(),
            };
            PeerRow {
                id: p.id.clone(),
                label: format!("{who}  ({st})  {models}"),
            }
        })
        .collect();
    Snapshot {
        health: h,
        running: true,
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

/// Why the delegate is not running, short enough for a menu line. The full
/// message is in tokenshare.log.
fn menu_error(e: &str) -> String {
    const MAX: usize = 80;
    // The first clause is the useful part: "port 11343 is already in use (is ollama...".
    let first = e.split(['(', ';']).next().unwrap_or(e).trim();
    let mut text = format!("Cannot start: {first}");
    if text.chars().count() > MAX {
        text = text.chars().take(MAX - 1).collect::<String>() + "…";
    }
    text
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
    let (mut running, start_error) = match start(&rt) {
        Ok(r) => (Some(r), None),
        Err(e) => {
            tracing::error!(error = %e, "delegate failed to start");
            (None, Some(e))
        }
    };
    let current: Shared = Arc::new(Mutex::new(Current::default()));
    {
        let mut c = current.lock().unwrap();
        c.state = running.as_ref().map(|r| r.state.clone());
        // Shown in the menu, so it says why rather than only pointing at the log.
        c.error = start_error;
        let file = file_config(&config_path);
        c.notify = file.as_ref().is_none_or(|c| c.notifications);
        c.check_updates = file.as_ref().is_none_or(|c| c.check_for_updates);
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
    let dashboard_item = MenuItem::new("Open Dashboard", true, None);
    let dashboard_id: MenuId = dashboard_item.id().clone();
    let settings_item = MenuItem::new("Settings…", config_path.is_some(), None);
    let settings_id: MenuId = settings_item.id().clone();
    let quit = MenuItem::new("Quit Token Share", true, None);
    let quit_id: MenuId = quit.id().clone();
    #[cfg(target_os = "macos")]
    let update_item = MenuItem::new(CHECK_FOR_UPDATES, true, None);
    #[cfg(target_os = "macos")]
    let update_id: MenuId = update_item.id().clone();
    #[cfg(target_os = "macos")]
    let updates = update::availability();
    #[cfg(target_os = "macos")]
    let mut updater = Updater::default();
    #[cfg(target_os = "macos")]
    if let update::Availability::Ready { exe, key, .. } = &updates {
        spawn_update_checks(current.clone(), proxy.clone(), exe.clone(), key.clone());
    }
    let mut tray: Option<TrayIcon> = None;
    let mut settings_window: Option<SettingsWindow> = None;
    // The "Forget this delegate" item of each listed peer -> that peer's id.
    let mut forget_ids: HashMap<MenuId, String> = HashMap::new();
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
                    &dashboard_item,
                    &settings_item,
                ]);
                #[cfg(target_os = "macos")]
                match &updates {
                    update::Availability::Ready { .. } => {
                        let _ = menu.append(&update_item);
                    }
                    update::Availability::MoveToApplications => {
                        update_item.set_text("Move Token Share to Applications to get updates");
                        update_item.set_enabled(false);
                        let _ = menu.append(&update_item);
                    }
                    update::Availability::Off => {}
                }
                let _ = menu.append_items(&[&PredefinedMenuItem::separator(), &quit]);
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
                dashboard_item.set_enabled(s.running);
                for item in peers.items() {
                    // Delegates are submenus; "No other delegates" is a plain item.
                    if let Some(sub) = item.as_submenu() {
                        let _ = peers.remove(sub);
                    } else if let Some(plain) = item.as_menuitem() {
                        let _ = peers.remove(plain);
                    }
                }
                forget_ids.clear();
                if s.peers.is_empty() {
                    let _ = peers.append(&MenuItem::new("No other delegates", false, None));
                }
                for p in &s.peers {
                    // Each delegate is a submenu so Forget is not one stray click away.
                    let sub = Submenu::new(&p.label, true);
                    let forget = MenuItem::new("Forget this delegate", true, None);
                    forget_ids.insert(forget.id().clone(), p.id.clone());
                    let _ = sub.append(&forget);
                    let _ = peers.append(&sub);
                }
                if let Some(t) = &tray {
                    let _ = t.set_icon(Some(icon_for(s.health)));
                    let _ = t.set_tooltip(Some(&s.headline));
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) if forget_ids.contains_key(&e.id) => {
                let id = forget_ids[&e.id].clone();
                let state = current.lock().unwrap().state.clone();
                if let Some(state) = state {
                    if state.delegates.forget(&id) {
                        tracing::info!(%id, "delegate forgotten from the menu");
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == dashboard_id => {
                let state = current.lock().unwrap().state.clone();
                match state {
                    Some(state) => open_in_browser(&format!(
                        "http://127.0.0.1:{}/_ts/dashboard",
                        state.config.port
                    )),
                    // The item is greyed out then; this only covers a click in between.
                    None => tracing::warn!("delegate is not running; no dashboard to open"),
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
                        // Fit the content, but stay on screen: the title bar and
                        // menu bar or taskbar are outside the inner size.
                        let max = w
                            .window
                            .current_monitor()
                            .map(|m| m.size().to_logical::<f64>(m.scale_factor()).height - 90.0)
                            .unwrap_or(900.0)
                            .max(300.0);
                        let h = height.clamp(300.0, max);
                        w.window
                            .set_inner_size(tao::dpi::LogicalSize::new(SETTINGS_WIDTH, h));
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
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::Menu(e)) if e.id == update_id => {
                let update::Availability::Ready { exe, key, .. } = &updates else {
                    return;
                };
                let proxy = proxy.clone();
                match updater.pending.clone() {
                    Some(u) => {
                        update_item.set_text("Installing update…");
                        update_item.set_enabled(false);
                        std::thread::spawn(move || {
                            let result = update::install(&u);
                            let _ = proxy.send_event(UserEvent::UpdateInstalled(result));
                        });
                    }
                    None => {
                        update_item.set_text("Checking for updates…");
                        update_item.set_enabled(false);
                        let (exe, key) = (exe.clone(), key.clone());
                        std::thread::spawn(move || {
                            let result = check_now(&exe, &key);
                            let _ = proxy.send_event(UserEvent::UpdateChecked {
                                manual: true,
                                result,
                            });
                        });
                    }
                }
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::UpdateChecked { manual, result }) => {
                let notify = current.lock().unwrap().notify;
                for n in updater.checked(manual, notify, result) {
                    show(n);
                }
                update_item.set_text(updater.menu_text());
                update_item.set_enabled(true);
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::UpdateInstalled(result)) => match result {
                Ok(()) => {
                    tracing::info!("update installed, restarting");
                    settings_window = None;
                    if let Some(r) = running.take() {
                        if let Err(e) = rt.block_on(r.stop(Duration::from_secs(5))) {
                            tracing::warn!(error = %e, "shutdown before restarting the update");
                        }
                    }
                    if let update::Availability::Ready { bundle, .. } = &updates {
                        if let Err(e) = update::relaunch(bundle) {
                            tracing::error!(error = %e, "cannot start the new version");
                            show(Notice {
                                title: "Token Share was updated".into(),
                                body: "Open it again to start the new version.".into(),
                            });
                        }
                    }
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
                Err(e) => {
                    tracing::error!(error = %e, "update failed");
                    show(Notice {
                        title: "Token Share could not update".into(),
                        body: e,
                    });
                    update_item.set_text(updater.menu_text());
                    update_item.set_enabled(true);
                }
            },
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
    {
        let mut c = current.lock().unwrap();
        c.notify = config.notifications;
        c.check_updates = config.check_for_updates;
    }

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
/// Open `url` in the default browser.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    // `explorer <url>` hands it to the default browser without a console window.
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(url).spawn();
    if let Err(e) = result {
        tracing::error!(error = %e, url, "cannot open the dashboard");
    }
}

/// The config file, if it can be read; settings it holds default to on.
fn file_config(config_path: &Option<PathBuf>) -> Option<Config> {
    config_path.as_ref().and_then(|p| Config::load(p).ok())
}

/// What [`Notifier`] looks at, and whether notifications are on.
fn notice_view(current: &Shared) -> (notices::View, bool) {
    let c = current.lock().unwrap();
    let Some(s) = &c.state else {
        let view = notices::View {
            start_error: c.error.as_deref().map(menu_error),
            ..Default::default()
        };
        return (view, c.notify);
    };
    let view = notices::View {
        // A new AppState on every (re)start; 0 is "not running".
        generation: Arc::as_ptr(s) as usize,
        start_error: None,
        peers: s
            .delegates
            .peers()
            .into_iter()
            .map(|p| notices::Peer {
                label: p.name.clone().unwrap_or_else(|| p.url.clone()),
                online: p.status == Status::Online,
                id: p.id,
                url: p.url,
            })
            .collect(),
        secret_refused: s.activity.secret_refusals(),
    };
    (view, c.notify)
}

/// Show `n` as a desktop notification. On a thread of its own: delivering one
/// can block for a moment, and the poller must not.
fn show(n: Notice) {
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            // Show them as Token Share's (with its icon). Fails harmlessly when
            // run outside the app bundle, e.g. with `cargo run`.
            static APP: std::sync::Once = std::sync::Once::new();
            APP.call_once(|| {
                let _ = notify_rust::set_application("com.tokenshare.tray");
            });
        }
        tracing::info!(title = %n.title, "notification");
        if let Err(e) = notify_rust::Notification::new()
            .appname("Token Share")
            .summary(&n.title)
            .body(&n.body)
            .show()
        {
            tracing::warn!(error = %e, "cannot show notification");
        }
    });
}

#[cfg(target_os = "macos")]
const CHECK_FOR_UPDATES: &str = "Check for Updates…";

/// What the app knows about updates, and what to tell the user.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct Updater {
    /// A newer version, found and not yet installed.
    pending: Option<Box<cargo_packager_updater::Update>>,
    /// The last version a notification was shown for, so a daily check
    /// does not announce the same one every day.
    announced: Option<String>,
}

#[cfg(target_os = "macos")]
impl Updater {
    /// Take in the result of a check, and say what is worth saying: always
    /// when the user asked, and otherwise only news (with notifications on).
    fn checked(
        &mut self,
        manual: bool,
        notify: bool,
        result: Result<Option<Box<cargo_packager_updater::Update>>, String>,
    ) -> Vec<Notice> {
        match result {
            Ok(Some(u)) => {
                let new = self.announced.as_deref() != Some(&u.version);
                let out = if manual || (notify && new) {
                    self.announced = Some(u.version.clone());
                    vec![Notice {
                        title: format!("Token Share {} is available", u.version),
                        body: "Choose Install in the Token Share menu. The app restarts with it."
                            .into(),
                    }]
                } else {
                    vec![]
                };
                self.pending = Some(u);
                out
            }
            Ok(None) if manual => vec![Notice {
                title: "Token Share is up to date".into(),
                body: format!("Version {} is the newest.", env!("CARGO_PKG_VERSION")),
            }],
            Ok(None) => vec![],
            Err(e) => {
                tracing::warn!(error = %e, manual, "update check failed");
                if manual {
                    vec![Notice {
                        title: "Could not check for updates".into(),
                        body: e,
                    }]
                } else {
                    vec![]
                }
            }
        }
    }

    fn menu_text(&self) -> String {
        match &self.pending {
            Some(u) => format!("Install Token Share {} and Restart", u.version),
            None => CHECK_FOR_UPDATES.into(),
        }
    }
}

#[cfg(target_os = "macos")]
fn check_now(
    exe: &std::path::Path,
    key: &str,
) -> Result<Option<Box<cargo_packager_updater::Update>>, String> {
    update::check(env!("CARGO_PKG_VERSION"), update::ENDPOINT, key, exe).map(|u| u.map(Box::new))
}

/// Look for a new version a little after starting and then once a day, while
/// `check_for_updates` is on.
#[cfg(target_os = "macos")]
fn spawn_update_checks(
    current: Shared,
    proxy: EventLoopProxy<UserEvent>,
    exe: PathBuf,
    key: String,
) {
    std::thread::spawn(move || {
        std::thread::sleep(update::FIRST_CHECK_AFTER);
        loop {
            if current.lock().unwrap().check_updates {
                let result = check_now(&exe, &key);
                if proxy
                    .send_event(UserEvent::UpdateChecked {
                        manual: false,
                        result,
                    })
                    .is_err()
                {
                    return; // UI is gone
                }
            }
            std::thread::sleep(update::CHECK_EVERY);
        }
    });
}

fn spawn_poller(current: Shared, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let mut last: Option<Snapshot> = None;
        let mut notifier = Notifier::default();
        loop {
            let (view, notify) = notice_view(&current);
            // Always updated, so turning notifications on does not replay old news.
            for n in notifier.update(&view, std::time::Instant::now()) {
                if notify {
                    show(n);
                }
            }
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
        .with_inner_size(tao::dpi::LogicalSize::new(SETTINGS_WIDTH, 500.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegate_that_failed_to_start_says_why_and_has_no_dashboard() {
        let current: Shared = Arc::new(Mutex::new(Current {
            state: None,
            notify: true,
            check_updates: true,
            error: Some(
                "port 11343 is already in use (is ollama running? it uses this port by default)"
                    .into(),
            ),
        }));
        let s = snapshot(&current);
        assert!(!s.running);
        assert_eq!(s.health, Health::Down);
        assert!(s.headline.contains("Not running"));
        assert_eq!(s.pool, "Cannot start: port 11343 is already in use");
    }

    #[test]
    fn long_start_errors_are_cut_to_fit_a_menu() {
        let long = "ollama host http://localhost:11434 is this delegate itself and this message goes on and on and on";
        let text = menu_error(long);
        assert_eq!(text.chars().count(), 80);
        assert!(text.ends_with('…'));
        assert!(text.starts_with("Cannot start: ollama host"));
    }

    #[cfg(target_os = "macos")]
    fn found(version: &str) -> Box<cargo_packager_updater::Update> {
        Box::new(cargo_packager_updater::Update {
            config: Default::default(),
            body: None,
            current_version: "0.1.0".into(),
            version: version.into(),
            date: None,
            target: "macos".into(),
            extract_path: "/Applications/Token Share.app".into(),
            download_url: "https://example.com/a.tar.gz".parse().unwrap(),
            signature: String::new(),
            timeout: None,
            headers: Default::default(),
            format: cargo_packager_updater::UpdateFormat::App,
        })
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_new_version_is_announced_once_and_offered_in_the_menu() {
        let mut u = Updater::default();
        assert_eq!(u.menu_text(), CHECK_FOR_UPDATES);
        let n = u.checked(false, true, Ok(Some(found("0.2.0"))));
        assert_eq!(n[0].title, "Token Share 0.2.0 is available");
        assert_eq!(u.menu_text(), "Install Token Share 0.2.0 and Restart");
        // The next day's check finds the same version: nothing new to say.
        assert!(u.checked(false, true, Ok(Some(found("0.2.0")))).is_empty());
        // Asking again always answers.
        assert_eq!(u.checked(true, true, Ok(Some(found("0.2.0")))).len(), 1);
        assert_eq!(u.checked(false, true, Ok(Some(found("0.3.0")))).len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn automatic_checks_are_quiet_unless_there_is_news() {
        let mut u = Updater::default();
        assert!(u.checked(false, true, Ok(None)).is_empty());
        assert!(u.checked(false, true, Err("offline".into())).is_empty());
        assert!(
            u.checked(false, false, Ok(Some(found("0.2.0")))).is_empty(),
            "notifications off"
        );
        assert_eq!(
            u.menu_text(),
            "Install Token Share 0.2.0 and Restart",
            "still offered"
        );
        // A check the user asked for always says how it went.
        let mut u = Updater::default();
        assert_eq!(
            u.checked(true, false, Ok(None))[0].title,
            "Token Share is up to date"
        );
        assert_eq!(
            u.checked(true, false, Err("offline".into()))[0].title,
            "Could not check for updates"
        );
    }

    #[test]
    fn a_running_delegate_has_a_dashboard() {
        let state = AppState::new("test".into(), Config::default(), None);
        let current: Shared = Arc::new(Mutex::new(Current {
            state: Some(state),
            error: None,
            notify: true,
            check_updates: true,
        }));
        assert!(snapshot(&current).running);
    }
}
