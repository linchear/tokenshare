use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokenshare_core::config::{default_config_path, default_data_dir};
use tokenshare_core::daemon::{self, Running};
use tokenshare_core::status::{health, Health};
use tokenshare_core::table::Status;
use tokenshare_core::{AppState, Config};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::icon;

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
}

fn label(h: Health) -> &'static str {
    match h {
        Health::Active => "Active",
        Health::InUse => "In use",
        Health::Down => "Down",
    }
}

fn snapshot(s: &AppState) -> Snapshot {
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
        headline: format!("Token Share: {}", label(h)),
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
    Icon::from_rgba(
        icon::disc_rgba(icon::SIZE, icon::color(h)),
        icon::SIZE,
        icon::SIZE,
    )
    .expect("valid icon")
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

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let started: Result<Running, String> = (|| {
        let config = match config_path() {
            Some(p) => Config::load(&p).map_err(|e| format!("{e:#}"))?,
            None => Config::default(),
        };
        rt.block_on(daemon::start(config, data_dir))
            .map_err(|e| format!("{e:#}"))
    })();
    if let Err(e) = &started {
        tracing::error!(error = %e, "delegate failed to start");
    }
    let (mut running, error) = match started {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e)),
    };

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

    if let Some(r) = &running {
        spawn_poller(r.state.clone(), proxy.clone());
    }

    let headline = MenuItem::new(
        error
            .as_ref()
            .map(|e| format!("Not running: {e}"))
            .unwrap_or_else(|| "Token Share: starting".into()),
        false,
        None,
    );
    let pool = MenuItem::new("", false, None);
    let peers = Submenu::new("Delegates", true);
    let quit = MenuItem::new("Quit Token Share", true, None);
    let quit_id: MenuId = quit.id().clone();
    let mut tray: Option<TrayIcon> = None;
    let initial = if error.is_some() {
        Health::Down
    } else {
        Health::Active
    };

    event_loop.run(move |event, _, control_flow| {
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
                    &quit,
                ]);
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("Token Share")
                    .with_icon(icon_for(initial))
                    .build()
                    .map_err(|e| tracing::error!(error = %e, "cannot create tray icon"))
                    .ok();
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
            Event::UserEvent(UserEvent::Menu(e)) if e.id == quit_id => {
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

/// Watch the delegate and tell the UI thread whenever the picture changes.
fn spawn_poller(state: std::sync::Arc<AppState>, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let mut last: Option<Snapshot> = None;
        loop {
            let now = snapshot(&state);
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
