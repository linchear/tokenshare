use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 11343;
/// The port ollama itself uses by default.
/// The port ollama itself uses by default, and the default for `ollama_port`.
pub const OLLAMA_PORT: u16 = 11434;
pub const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port for both ollama and coordination traffic.
    pub port: u16,
    /// Also listen on `ollama_port`, so existing ollama clients work unchanged. Conflicts with a local ollama on that port.
    pub masquerade: bool,
    /// The port ollama clients are served on when `masquerade` is on. Ollama's own
    /// default, 11434, lets apps that already talk to ollama use the pool unchanged.
    pub ollama_port: u16,
    /// First-contact delegate, e.g. `http://192.168.2.3:11343`.
    pub fc: Option<String>,
    /// How peers reach this delegate. If unset, it is learned from the first
    /// delegate we join.
    pub advertise_url: Option<String>,
    /// Ollama instances reachable directly from this machine.
    pub ollama_hosts: Vec<String>,
    /// Max concurrent requests sent to ollama.
    pub pool_size: usize,
    /// Let other delegates use this machine's ollama. When false it looks down
    /// to them (no models advertised, forwarded requests refused), while this
    /// machine's own users still get local ollama and the rest of the pool.
    pub share_with_peers: bool,
    /// How often to refresh the local model list.
    pub poll_interval_secs: u64,
    /// Wait before trying to rejoin when no known delegate answered.
    pub rejoin_retry_secs: u64,
    /// How often to send a keepalive to one random peer.
    pub keepalive_interval_secs: u64,
    /// How many more hops a keepalive is relayed after the first delegate.
    pub keepalive_ttl: u8,
    /// How often to pull and merge the full table from one random peer.
    pub sync_interval_secs: u64,
    /// On shutdown, how long to let in-flight requests finish before cutting them off.
    pub shutdown_grace_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            masquerade: false,
            ollama_port: OLLAMA_PORT,
            fc: None,
            advertise_url: None,
            ollama_hosts: vec![DEFAULT_OLLAMA_HOST.to_string()],
            pool_size: 3,
            share_with_peers: true,
            poll_interval_secs: 10,
            rejoin_retry_secs: 300,
            keepalive_interval_secs: 30,
            keepalive_ttl: 3,
            sync_interval_secs: 60,
            shutdown_grace_secs: 30,
        }
    }
}

/// What a listening port does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// The ollama API: serves from the local ollama and the pool.
    Ollama,
    /// The tokenshare port when masquerading: the delegates' port. Other
    /// delegates' (forwarded) requests are served from the local ollama hosts
    /// when sharing is on, but direct clients on it get no local ollama: it
    /// acts as a delegate with no models and forwards to other delegates.
    Mesh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub face: Face,
}

impl Config {
    /// The ports to bind. `port` (the tokenshare port) is always one of them. With
    /// `masquerade`, `ollama_port` (11434 by default) is bound too for ollama
    /// clients, and `port` becomes a mesh-only listener so this machine can still
    /// advertise itself to other delegates.
    pub fn listeners(&self) -> Vec<Listener> {
        if self.masquerade && self.port != self.ollama_port {
            vec![
                Listener {
                    port: self.port,
                    face: Face::Mesh,
                },
                Listener {
                    port: self.ollama_port,
                    face: Face::Ollama,
                },
            ]
        } else {
            vec![Listener {
                port: self.port,
                face: Face::Ollama,
            }]
        }
    }

    pub fn listen_ports(&self) -> Vec<u16> {
        self.listeners().iter().map(|l| l.port).collect()
    }

    /// Whether other delegates may use this machine's ollama hosts. Independent
    /// of `masquerade`: a masquerading machine can still lend its hosts, which
    /// peers reach on the tokenshare port.
    pub fn shares_models(&self) -> bool {
        self.share_with_peers
    }

    /// Configured ollama hosts that are really this delegate (a loopback address
    /// on our own listen port), which would make requests loop back into us.
    pub fn self_loop_hosts(&self) -> Vec<String> {
        let ports = self.listen_ports();
        self.ollama_hosts
            .iter()
            .map(|h| crate::ollama::normalize_host(h))
            .filter(|h| {
                let rest = h.split("://").nth(1).unwrap_or("");
                let authority = rest.split('/').next().unwrap_or("");
                let (host, p) = match authority.rsplit_once(':') {
                    Some((host, p)) if !p.contains(']') => (host, p.parse::<u16>().ok()),
                    _ => (authority, None),
                };
                let loopback = matches!(
                    host.trim_matches(|c| c == '[' || c == ']'),
                    "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"
                );
                loopback && p.is_some_and(|p| ports.contains(&p))
            })
            .collect()
    }

    /// Load from `path`; a missing file yields defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config {}", path.display())),
        }
    }
}

impl Config {
    /// Write to `path` atomically (temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(
            &tmp,
            toml::to_string_pretty(self).context("serializing config")?,
        )
        .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }
}

fn dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "tokenshare")
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs().map(|d| d.config_dir().join("config.toml"))
}

pub fn default_data_dir() -> Option<PathBuf> {
    dirs().map(|d| d.data_dir().to_path_buf())
}

/// Load this delegate's persisted ID from `dir`, creating it on first run.
pub fn load_or_create_id(dir: &Path) -> Result<String> {
    let path = dir.join("id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Whether something is already listening on `port`. Checks loopback by
/// connecting as well as binding: on macOS and BSD, binding the wildcard
/// address succeeds even while another program holds 127.0.0.1:port (ollama
/// does), and that program would silently win all local connections.
pub fn port_in_use(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
    use std::time::Duration;
    if port == 0 {
        return false;
    }
    let reachable = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|a| TcpStream::connect_timeout(a, Duration::from_millis(300)).is_ok());
    reachable || TcpListener::bind(("0.0.0.0", port)).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load(Path::new("/nonexistent/tokenshare.toml")).unwrap();
        assert_eq!(c.port, 11343);
        assert_eq!(c.pool_size, 3);
    }

    #[test]
    fn partial_toml_overrides_defaults() {
        let c: Config = toml::from_str("pool_size = 5\nfc = \"http://a:1\"").unwrap();
        assert_eq!(c.pool_size, 5);
        assert_eq!(c.fc.as_deref(), Some("http://a:1"));
        assert_eq!(c.port, 11343);
    }

    #[test]
    fn plain_delegate_listens_on_one_port() {
        let c = Config {
            port: 12000,
            ..Config::default()
        };
        assert_eq!(
            c.listeners(),
            vec![Listener {
                port: 12000,
                face: Face::Ollama
            }]
        );
        assert!(c.shares_models());
    }

    #[test]
    fn masquerade_adds_ollamas_port_and_makes_the_tokenshare_port_mesh_only() {
        let c = Config {
            port: 12000,
            masquerade: true,
            ..Config::default()
        };
        assert_eq!(
            c.listeners(),
            vec![
                Listener {
                    port: 12000,
                    face: Face::Mesh
                },
                Listener {
                    port: 11434,
                    face: Face::Ollama
                },
            ]
        );
        assert_eq!(c.listen_ports(), vec![12000, 11434]);
        assert!(
            c.shares_models(),
            "masquerading does not stop sharing with peers"
        );
        assert!(!Config {
            share_with_peers: false,
            ..c.clone()
        }
        .shares_models());
        assert_eq!(c.port, 12000, "the tokenshare port is unchanged");
    }

    #[test]
    fn the_ollama_facing_port_is_configurable() {
        let c = Config {
            port: 12000,
            masquerade: true,
            ollama_port: 12434,
            ..Config::default()
        };
        assert_eq!(c.listen_ports(), vec![12000, 12434]);
        assert_eq!(
            Config {
                masquerade: false,
                ..c.clone()
            }
            .listen_ports(),
            vec![12000],
            "unused unless masquerading"
        );
        assert_eq!(Config::default().ollama_port, 11434);
        // Hosts pointing back at the custom port are self-loops, 11434 is a real ollama.
        let looped = Config {
            ollama_hosts: vec!["localhost:12434".into(), "localhost:11434".into()],
            ..c
        };
        assert_eq!(looped.self_loop_hosts(), vec!["http://localhost:12434"]);
    }

    #[test]
    fn masquerading_on_the_ollama_port_itself_is_a_single_full_listener() {
        let c = Config {
            port: 11434,
            masquerade: true,
            ..Config::default()
        };
        assert_eq!(
            c.listeners(),
            vec![Listener {
                port: 11434,
                face: Face::Ollama
            }]
        );
    }

    #[test]
    fn hosts_pointing_back_at_us_are_found() {
        let c = Config {
            masquerade: true,
            ollama_hosts: vec![
                "localhost:11434".into(),
                "http://127.0.0.1:11434/".into(),
                "http://[::1]:11434".into(),
                "http://gpu-box:11434".into(),
                "http://localhost:11435".into(),
            ],
            ..Config::default()
        };
        assert_eq!(
            c.self_loop_hosts(),
            vec![
                "http://localhost:11434",
                "http://127.0.0.1:11434",
                "http://[::1]:11434"
            ]
        );
        // The tokenshare port counts too while masquerading.
        assert_eq!(
            Config {
                masquerade: true,
                ollama_hosts: vec!["127.0.0.1:11343".into()],
                ..Config::default()
            }
            .self_loop_hosts(),
            vec!["http://127.0.0.1:11343"]
        );
        // Without masquerade we listen on 11343, so 11434 is a real ollama.
        assert!(Config {
            masquerade: false,
            ..c
        }
        .self_loop_hosts()
        .is_empty());
    }

    #[test]
    fn port_in_use_sees_loopback_only_listeners() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(port_in_use(port));
        drop(l);
        assert!(!port_in_use(port));
        assert!(!port_in_use(0));
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("ts-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("config.toml");
        let c = Config {
            fc: Some("http://a:1".into()),
            advertise_url: None,
            pool_size: 7,
            ollama_hosts: vec!["http://x:1".into(), "http://y:2".into()],
            ..Config::default()
        };
        c.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(
            (back.fc, back.advertise_url, back.pool_size),
            (c.fc, None, 7)
        );
        assert_eq!(back.ollama_hosts, c.ollama_hosts);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn id_is_stable_across_loads() {
        let dir = std::env::temp_dir().join(format!("ts-id-{}", uuid::Uuid::new_v4()));
        let a = load_or_create_id(&dir).unwrap();
        let b = load_or_create_id(&dir).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(dir).ok();
    }
}
