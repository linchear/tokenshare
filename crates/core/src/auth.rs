//! Optional shared-secret authentication (`pool_secret`).
//!
//! With a secret configured, every request to this delegate must carry it as
//! `Authorization: Bearer <secret>`: other delegates' gossip and forwarded
//! requests, and clients on other machines (OpenAI-style clients send their API
//! key this way). Delegates add it to everything they send to peers.
//!
//! Exceptions:
//! - Connections from this machine are trusted unless `trust_localhost` is off,
//!   so local apps, the `tokenshare` CLI and the tray's dashboard keep working.
//! - `GET /`, `GET /api/version` and the dashboard page say nothing about the
//!   pool; they stay open so health checks work. The dashboard's data comes from
//!   `/_ts/status`, which does need the secret.
//!
//! The secret is never passed on to ollama. It is a password, not encryption:
//! over plain HTTP anyone who can watch the traffic can read it (and the
//! prompts). Use a network you trust, or a VPN such as WireGuard or Tailscale.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::activity::Rejection;
use crate::mesh::is_local;
use crate::proxy::AppState;

pub const MIN_LEN: usize = 8;

/// Whether `secret` can be used: long enough, and sendable in a header as is.
pub fn validate(secret: &str) -> Result<(), String> {
    if !secret.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(
            "The pool secret may only contain letters, digits and punctuation, without spaces"
                .into(),
        );
    }
    if secret.len() < MIN_LEN {
        return Err(format!(
            "The pool secret must be at least {MIN_LEN} characters long"
        ));
    }
    Ok(())
}

pub struct Secret {
    digest: [u8; 32],
    header: HeaderValue,
}

impl Secret {
    pub fn new(secret: &str) -> Result<Self, String> {
        validate(secret)?;
        let mut header =
            HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|e| e.to_string())?;
        header.set_sensitive(true);
        Ok(Self {
            digest: digest(secret.as_bytes()),
            header,
        })
    }

    /// The `Authorization` value to send to peers.
    pub fn header(&self) -> &HeaderValue {
        &self.header
    }

    /// Whether `headers` carry this secret as a bearer token. Digests are
    /// compared in constant time, so timing reveals nothing about the secret.
    pub fn matches(&self, headers: &HeaderMap) -> bool {
        let Some(token) = bearer(headers) else {
            return false;
        };
        let d = digest(token);
        d.iter()
            .zip(self.digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn bearer(headers: &HeaderMap) -> Option<&[u8]> {
    let v = headers.get(header::AUTHORIZATION)?.as_bytes();
    let (scheme, token) = v.split_at_checked(7)?;
    scheme
        .eq_ignore_ascii_case(b"bearer ")
        .then(|| token.trim_ascii())
}

/// Requests anyone may make: they reveal nothing about the pool.
fn is_open(method: &Method, path: &str) -> bool {
    (method == Method::GET || method == Method::HEAD)
        && matches!(path, "/" | "/api/version" | "/_ts/dashboard")
}

/// Middleware on every listener: let the request through or answer 401.
pub async fn require(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let Some(secret) = &s.secret else {
        return next.run(req).await;
    };
    if secret.matches(req.headers()) {
        // Ollama never sees it; peers get it from us again when forwarding.
        req.headers_mut().remove(header::AUTHORIZATION);
        return next.run(req).await;
    }
    let from = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let local = from.is_some_and(is_local);
    if (local && s.config.trust_localhost) || is_open(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    s.activity.rejected(Rejection::Unauthorized);
    warn!(
        from = from.map(|ip| ip.to_string()).unwrap_or_default(),
        method = %req.method(),
        path = req.uri().path(),
        "refused a request without the pool secret"
    );
    let mut r = (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "this delegate needs the pool secret (Authorization: Bearer <secret>)"
        })),
    )
        .into_response();
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"tokenshare\""),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn only_the_exact_secret_as_a_bearer_token_matches() {
        let s = Secret::new("correct-horse").unwrap();
        assert!(s.matches(&auth("Bearer correct-horse")));
        assert!(
            s.matches(&auth("bearer correct-horse")),
            "scheme is case-insensitive"
        );
        assert!(
            s.matches(&auth("Bearer  correct-horse ")),
            "surrounding space"
        );
        for bad in [
            "Bearer correct-hors",
            "Bearer correct-horse!",
            "Bearer CORRECT-HORSE",
            "Basic correct-horse",
            "correct-horse",
            "Bearer ",
            "Bearer",
        ] {
            assert!(!s.matches(&auth(bad)), "{bad}");
        }
        assert!(!s.matches(&HeaderMap::new()));
        assert_eq!(s.header(), "Bearer correct-horse");
        assert!(s.header().is_sensitive());
    }

    #[test]
    fn secrets_must_be_long_enough_and_header_safe() {
        assert!(validate("12345678").is_ok());
        assert!(validate("1234567").unwrap_err().contains("at least 8"));
        assert!(validate("has a space")
            .unwrap_err()
            .contains("without spaces"));
        assert!(validate("naïve-secret").is_err());
        assert!(validate("tab\tsecret").is_err());
    }

    #[test]
    fn only_harmless_reads_are_open() {
        assert!(is_open(&Method::HEAD, "/"));
        assert!(is_open(&Method::GET, "/api/version"));
        assert!(is_open(&Method::GET, "/_ts/dashboard"));
        for (m, p) in [
            (Method::GET, "/_ts/status"),
            (Method::GET, "/_ts/metrics"),
            (Method::GET, "/_ts/table"),
            (Method::GET, "/api/tags"),
            (Method::POST, "/api/chat"),
            (Method::POST, "/"),
            (Method::POST, "/_ts/join"),
        ] {
            assert!(!is_open(&m, p), "{m} {p}");
        }
    }
}
