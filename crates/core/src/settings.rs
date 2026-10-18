//! The settings a user edits in the UI, as text fields, and how they map onto
//! [`Config`]. Advanced options (intervals, TTL, grace period) are left alone.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::ollama::normalize_host;

/// What the settings form holds. Everything is a string, as typed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Form {
    /// First-contact delegate URL; blank for none.
    pub fc: String,
    /// How peers reach this machine; blank to learn it automatically.
    pub advertise_url: String,
    /// Ollama hosts, one per line. May be empty for a client-only delegate.
    pub ollama_hosts: String,
    pub port: String,
    pub pool_size: String,
    /// Let other delegates use this machine's ollama.
    pub share_with_peers: bool,
}

impl Form {
    pub fn from_config(c: &Config) -> Self {
        Self {
            fc: c.fc.clone().unwrap_or_default(),
            advertise_url: c.advertise_url.clone().unwrap_or_default(),
            ollama_hosts: c.ollama_hosts.join("\n"),
            port: c.port.to_string(),
            pool_size: c.pool_size.to_string(),
            share_with_peers: c.share_with_peers,
        }
    }
}

fn check_url(label: &str, v: &str, errors: &mut Vec<String>) {
    let ok = ["http://", "https://"].iter().any(|p| {
        v.strip_prefix(p).is_some_and(|rest| {
            !rest.is_empty() && !rest.contains(char::is_whitespace) && !rest.contains("://")
        })
    });
    if !ok {
        errors.push(format!("{label} must look like http://host:port"));
    }
}

/// Apply `form` on top of `base`. Returns every problem found, not just the first.
pub fn apply(form: &Form, base: &Config) -> Result<Config, Vec<String>> {
    let mut errors = vec![];
    let mut c = base.clone();

    let fc = form.fc.trim().trim_end_matches('/');
    c.fc = if fc.is_empty() {
        None
    } else {
        check_url("First contact", fc, &mut errors);
        Some(fc.to_string())
    };

    let adv = form.advertise_url.trim().trim_end_matches('/');
    c.advertise_url = if adv.is_empty() {
        None
    } else {
        check_url("Advertise URL", adv, &mut errors);
        Some(adv.to_string())
    };

    let hosts: Vec<String> = form
        .ollama_hosts
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(normalize_host)
        .collect();
    for h in &hosts {
        check_url(&format!("Ollama host {h}"), h, &mut errors);
    }
    c.ollama_hosts = hosts;

    match form.port.trim().parse::<u16>() {
        Ok(p) if p > 0 => c.port = p,
        _ => errors.push("Port must be a number from 1 to 65535".into()),
    }
    match form.pool_size.trim().parse::<usize>() {
        Ok(n) if (1..=64).contains(&n) => c.pool_size = n,
        _ => errors.push("Connections must be a number from 1 to 64".into()),
    }

    c.share_with_peers = form.share_with_peers;

    if errors.is_empty() {
        Ok(c)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        Form::from_config(&Config::default())
    }

    #[test]
    fn untouched_form_reproduces_the_config() {
        let base = Config {
            fc: Some("http://a:1".into()),
            pool_size: 5,
            ..Config::default()
        };
        assert_eq!(apply(&Form::from_config(&base), &base).unwrap().fc, base.fc);
        let c = apply(&Form::from_config(&base), &base).unwrap();
        assert_eq!(
            (c.port, c.pool_size, c.ollama_hosts),
            (base.port, 5, base.ollama_hosts)
        );
    }

    #[test]
    fn values_are_trimmed_and_blank_means_unset() {
        let f = Form {
            fc: "  http://10.0.0.2:11343/ ".into(),
            advertise_url: "  ".into(),
            ollama_hosts: " localhost:11434 \n\n http://gpu:11434/ \n".into(),
            ..form()
        };
        let c = apply(&f, &Config::default()).unwrap();
        assert_eq!(c.fc.as_deref(), Some("http://10.0.0.2:11343"));
        assert_eq!(c.advertise_url, None);
        assert_eq!(
            c.ollama_hosts,
            vec!["http://localhost:11434", "http://gpu:11434"]
        );
    }

    #[test]
    fn sharing_toggle_round_trips() {
        let mut f = form();
        assert!(f.share_with_peers, "sharing is on by default");
        f.share_with_peers = false;
        let c = apply(&f, &Config::default()).unwrap();
        assert!(!c.share_with_peers);
        assert!(!Form::from_config(&c).share_with_peers);
    }

    #[test]
    fn advanced_settings_are_preserved() {
        let base = Config {
            keepalive_ttl: 9,
            shutdown_grace_secs: 77,
            ..Config::default()
        };
        let c = apply(&form(), &base).unwrap();
        assert_eq!((c.keepalive_ttl, c.shutdown_grace_secs), (9, 77));
    }

    #[test]
    fn every_problem_is_reported() {
        let f = Form {
            fc: "not a url".into(),
            ollama_hosts: "not-a url".into(),
            port: "0".into(),
            pool_size: "lots".into(),
            ..form()
        };
        let errors = apply(&f, &Config::default()).unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:?}");
    }

    #[test]
    fn no_ollama_hosts_is_allowed_for_client_only_delegates() {
        let f = Form {
            ollama_hosts: "  \n\n ".into(),
            ..form()
        };
        let c = apply(&f, &Config::default()).unwrap();
        assert!(c.ollama_hosts.is_empty());
        assert!(Form::from_config(&c).ollama_hosts.is_empty());
    }

    #[test]
    fn two_urls_pasted_together_are_rejected() {
        let f = Form {
            fc: "http://a:1http://b:2".into(),
            ..form()
        };
        assert!(apply(&f, &Config::default()).is_err());
    }

    #[test]
    fn port_and_pool_bounds() {
        for (port, pool, ok) in [
            ("1", "1", true),
            ("65535", "64", true),
            ("65536", "3", false),
            ("80", "0", false),
            ("80", "65", false),
        ] {
            let f = Form {
                port: port.into(),
                pool_size: pool.into(),
                ..form()
            };
            assert_eq!(apply(&f, &Config::default()).is_ok(), ok, "{port} {pool}");
        }
    }
}
