//! Commands that talk to the delegate running on this machine.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// A delegate as listed by `/_ts/status`.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub url: String,
    pub status: String,
    pub models: Vec<String>,
    pub last_seen_ms: u64,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    /// Talk to the delegate listening on `port` of this machine.
    pub fn local(port: u16) -> Self {
        Self {
            base: format!("http://127.0.0.1:{port}"),
            http: reqwest::Client::new(),
        }
    }

    /// Our own id, and every delegate we know about (ourselves included).
    pub async fn delegates(&self) -> Result<(String, Vec<Row>)> {
        let status: Value = self
            .http
            .get(format!("{}/_ts/status", self.base))
            .send()
            .await
            .with_context(|| format!("cannot reach a delegate at {} (is it running?)", self.base))?
            .error_for_status()?
            .json()
            .await?;
        let me = status["id"].as_str().unwrap_or_default().to_string();
        let rows = status["delegates"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|d| Row {
                        id: d["id"].as_str().unwrap_or_default().to_string(),
                        url: d["url"].as_str().unwrap_or_default().to_string(),
                        status: d["status"].as_str().unwrap_or_default().to_string(),
                        models: d["models"]
                            .as_array()
                            .map(|m| {
                                m.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        last_seen_ms: d["last_seen"].as_u64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((me, rows))
    }

    /// Ask the delegate to forget `id`. Returns whether it knew it.
    pub async fn forget(&self, id: &str) -> Result<bool> {
        let resp = self
            .http
            .post(format!("{}/_ts/forget", self.base))
            .json(&json!({ "id": id }))
            .send()
            .await
            .with_context(|| format!("cannot reach a delegate at {}", self.base))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "{}",
                body["error"].as_str().unwrap_or("the delegate refused")
            );
        }
        Ok(body["forgotten"].as_bool().unwrap_or(false))
    }
}

/// Find the one delegate `target` names: an id, the start of an id (4+ characters)
/// or a URL. Ourselves can be listed but not forgotten.
pub fn resolve<'a>(target: &str, me: &str, rows: &'a [Row]) -> Result<&'a Row> {
    let target = target.trim();
    if target.is_empty() {
        bail!("say which delegate: an id, the start of an id, or a URL");
    }
    let url_form = target.trim_end_matches('/');
    let lower = target.to_lowercase();
    let exact: Vec<&Row> = rows
        .iter()
        .filter(|r| r.id == target || r.url.trim_end_matches('/') == url_form)
        .collect();
    let matches = if exact.is_empty() && lower.len() >= 4 {
        rows.iter()
            .filter(|r| r.id.to_lowercase().starts_with(&lower))
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [] => Err(anyhow!(
            "no delegate matches \"{target}\" (run `tokenshare delegates` to list them)"
        )),
        [one] if one.id == me => bail!("that is this delegate; it cannot forget itself"),
        [one] => Ok(one),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|r| format!("{} {}", short(&r.id), r.url))
                .collect();
            Err(anyhow!("\"{target}\" is ambiguous: {}", list.join(", ")))
        }
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn ago(ms: u64, now_ms: u64) -> String {
    if ms == 0 {
        return "never".into();
    }
    let s = now_ms.saturating_sub(ms) / 1000;
    match s {
        0..=89 => format!("{s}s ago"),
        90..=5399 => format!("{}m ago", s / 60),
        5400..=129_599 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// The `tokenshare delegates` listing.
pub fn format_table(me: &str, rows: &[Row], now_ms: u64) -> String {
    let mut out = format!(
        "{:<9} {:<12} {:<10} {:<30} {}\n",
        "ID", "STATUS", "LAST SEEN", "URL", "MODELS"
    );
    for r in rows {
        let models = if r.id == me {
            "(this delegate)".to_string()
        } else if r.models.is_empty() {
            "-".to_string()
        } else {
            r.models.join(", ")
        };
        out.push_str(&format!(
            "{:<9} {:<12} {:<10} {:<30} {}\n",
            short(&r.id),
            r.status,
            ago(r.last_seen_ms, now_ms),
            r.url,
            models
        ));
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn list(port: u16) -> Result<()> {
    let (me, rows) = Client::local(port).delegates().await?;
    print!("{}", format_table(&me, &rows, now_ms()));
    Ok(())
}

pub async fn forget(port: u16, target: &str) -> Result<()> {
    let client = Client::local(port);
    let (me, rows) = client.delegates().await?;
    let row = resolve(target, &me, &rows)?;
    if client.forget(&row.id).await? {
        println!("Forgot {} ({}).", short(&row.id), row.url);
        println!("It stays out until it is heard from again.");
        Ok(())
    } else {
        bail!("the delegate no longer knows {}", short(&row.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, url: &str) -> Row {
        Row {
            id: id.into(),
            url: url.into(),
            status: "online".into(),
            models: vec![],
            last_seen_ms: 0,
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            row("83976b11-aaaa", "http://100.94.167.47:11343"),
            row("8bbb822d-bbbb", "http://100.95.20.66:11343/"),
            row("8bbb9999-cccc", "http://100.1.1.1:11343"),
            row("d44294c0-dddd", "http://127.0.0.1:11400"),
        ]
    }

    #[test]
    fn resolves_by_id_prefix_or_url() {
        let r = rows();
        let me = "83976b11-aaaa";
        assert_eq!(
            resolve("d44294c0-dddd", me, &r).unwrap().url,
            "http://127.0.0.1:11400"
        );
        assert_eq!(resolve("d442", me, &r).unwrap().id, "d44294c0-dddd");
        assert_eq!(
            resolve("D442", me, &r).unwrap().id,
            "d44294c0-dddd",
            "case-insensitive"
        );
        assert_eq!(
            resolve("http://127.0.0.1:11400", me, &r).unwrap().id,
            "d44294c0-dddd"
        );
        assert_eq!(
            resolve("http://100.95.20.66:11343", me, &r).unwrap().id,
            "8bbb822d-bbbb",
            "trailing slash"
        );
    }

    #[test]
    fn refuses_what_it_cannot_resolve_safely() {
        let r = rows();
        let me = "83976b11-aaaa";
        assert!(resolve("8bbb", me, &r)
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(resolve("zzzz", me, &r)
            .unwrap_err()
            .to_string()
            .contains("no delegate matches"));
        assert!(
            resolve("d44", me, &r).is_err(),
            "prefixes under 4 characters are not accepted"
        );
        assert!(resolve("", me, &r).is_err());
        assert!(resolve("83976b11", me, &r)
            .unwrap_err()
            .to_string()
            .contains("cannot forget itself"));
    }

    #[test]
    fn the_table_marks_this_delegate_and_shows_ages() {
        let mut r = rows();
        r[1].models = vec!["qwen3:14b".into()];
        let now = 1_792_000_000_000u64;
        r[1].last_seen_ms = now - 30_000;
        r[3].last_seen_ms = now - 3 * 86_400_000;
        let t = format_table("83976b11-aaaa", &r, now);
        assert!(t.starts_with("ID "));
        assert!(t.contains("(this delegate)"));
        assert!(t.contains("qwen3:14b"));
        assert!(t.contains("30s ago"));
        assert!(t.contains("3d ago"));
        assert!(t.contains("never"), "unknown last_seen");
    }
}
