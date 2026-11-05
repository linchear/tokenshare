//! The optional pool secret: who gets in without it, who needs it, and that
//! delegates pass it between themselves but never to ollama.
//!
//! Every test runs on loopback, which a delegate trusts by default, so the
//! delegates checking the secret here have `trust_localhost` off: to them,
//! these test clients look like other machines.

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokenshare_core::mesh::{pull_table, rejoin};

mod common;
use common::{eventually, node, NodeOpts};

const SECRET: &str = "s3cret-pool-key";

/// A delegate that requires the secret from everyone.
fn strict<'a>(models: &'a [&'a str]) -> NodeOpts<'a> {
    NodeOpts {
        models,
        secret: Some(SECRET),
        trust_localhost: false,
        ..Default::default()
    }
}

async fn chat(url: &str, auth: Option<&str>) -> reqwest::Response {
    let mut rb = reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": "gemma2", "messages": []}));
    if let Some(a) = auth {
        rb = rb.header("authorization", a);
    }
    rb.send().await.unwrap()
}

async fn get(url: &str, auth: Option<&str>) -> reqwest::Response {
    let mut rb = reqwest::Client::new().get(url);
    if let Some(a) = auth {
        rb = rb.header("authorization", a);
    }
    rb.send().await.unwrap()
}

fn bearer(s: &str) -> String {
    format!("Bearer {s}")
}

#[tokio::test]
async fn without_the_secret_only_harmless_reads_get_in() {
    let a = node(strict(&["gemma2:latest"])).await;

    let r = chat(&a.url, None).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.headers()["www-authenticate"],
        "Bearer realm=\"tokenshare\""
    );
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("pool secret"));

    for path in [
        "/api/tags",
        "/_ts/status",
        "/_ts/metrics",
        "/_ts/table",
        "/api/ps",
    ] {
        let r = get(&format!("{}{path}", a.url), None).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let r = reqwest::Client::new()
        .post(format!("{}/_ts/join", a.url))
        .json(&json!({"id": "intruder", "url": "http://127.0.0.1:9", "port": 9, "models": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(
        a.state.delegates.peers().is_empty(),
        "nobody joined without the secret"
    );

    // Health checks still work; the dashboard page has no data of its own.
    for path in ["/", "/api/version", "/_ts/dashboard"] {
        let r = get(&format!("{}{path}", a.url), None).await;
        assert_eq!(r.status(), StatusCode::OK, "{path}");
    }
    assert_eq!(a.mock.chat_count(), 0, "ollama never saw a refused request");
}

#[tokio::test]
async fn a_wrong_secret_is_refused_and_the_right_one_never_reaches_ollama() {
    let a = node(strict(&["gemma2:latest"])).await;
    for wrong in [
        bearer("s3cret-pool-ke"),
        bearer("S3CRET-POOL-KEY"),
        format!("Basic {SECRET}"),
        SECRET.to_string(),
    ] {
        let r = chat(&a.url, Some(&wrong)).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{wrong}");
    }

    let r = chat(&a.url, Some(&bearer(SECRET))).await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    assert_eq!(*a.mock.chat_auth.lock().unwrap(), vec![None]);

    let status: Value = get(&format!("{}/_ts/status", a.url), Some(&bearer(SECRET)))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["auth"], true);
    assert_eq!(status["activity"]["rejected"]["unauthorized"], 4);
    let metrics = get(&format!("{}/_ts/metrics", a.url), Some(&bearer(SECRET)))
        .await
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("\ntokenshare_auth_required 1\n"));
    assert!(metrics.contains("tokenshare_requests_rejected_total{reason=\"unauthorized\"} 4\n"));
}

#[tokio::test]
async fn this_machine_is_trusted_by_default() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        secret: Some(SECRET),
        ..Default::default()
    })
    .await;
    let r = chat(&a.url, None).await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    // A local app's own API key is its business: passed through as before.
    let r = chat(&a.url, Some("Bearer ollama")).await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    assert_eq!(
        *a.mock.chat_auth.lock().unwrap(),
        vec![None, Some("Bearer ollama".to_string())]
    );
    let r = get(&format!("{}/_ts/status", a.url), None).await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn delegates_with_the_same_secret_form_a_pool() {
    let a = node(strict(&["gemma2:latest"])).await;
    // C trusts its own machine, as a real delegate would: local apps need no secret.
    let c = node(NodeOpts {
        fc: Some(&a.url),
        client_only: true,
        secret: Some(SECRET),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    eventually(3, "A to list C", || a.state.delegates.peers().len() == 1).await;
    a.state.refresh_local().await;
    pull_table(&c.state, &a.url).await.unwrap();
    assert!(tokenshare_core::mesh::send_keepalive(&c.state).await);

    // A local app with its own API key: C sends A the secret in its place.
    let r = chat(&c.url, Some("Bearer ollama")).await;
    assert_eq!(r.status(), StatusCode::OK);
    r.text().await.unwrap();
    assert_eq!(a.mock.chat_count(), 1);
    assert_eq!(
        *a.mock.chat_auth.lock().unwrap(),
        vec![None],
        "ollama never sees the secret or the app's key"
    );
}

#[tokio::test]
async fn a_delegate_without_the_right_secret_cannot_join_or_use_the_pool() {
    let a = node(strict(&["gemma2:latest"])).await;
    for secret in [None, Some("another-secret")] {
        let x = node(NodeOpts {
            fc: Some(&a.url),
            client_only: true,
            secret,
            ..Default::default()
        })
        .await;
        assert_eq!(rejoin(&x.state).await, Some(false), "{secret:?}");
        assert!(pull_table(&x.state, &a.url)
            .await
            .unwrap_err()
            .to_string()
            .contains("pool secret"));
        assert!(a.state.delegates.peers().is_empty(), "{secret:?}");
        // X knows who turned it away, for its dashboard and tray.
        assert_eq!(x.state.activity.secret_refusals(), vec![a.url.clone()]);
        let status: Value = get(&format!("{}/_ts/status", x.url), None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(status["activity"]["secret_refused"], json!([a.url]));
    }
    assert_eq!(a.mock.chat_count(), 0);
}

#[tokio::test]
async fn a_peer_with_another_secret_is_skipped_for_one_that_matches() {
    // A has the model but a different secret; B shares C's.
    let a = node(NodeOpts {
        secret: Some("another-secret"),
        ..strict(&["gemma2:latest"])
    })
    .await;
    let b = node(strict(&["gemma2:latest"])).await;
    let c = node(NodeOpts {
        fc: Some(&b.url),
        client_only: true,
        secret: Some(SECRET),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    // Put A in C's table anyway, as a stale or mistaken entry might be.
    let mut stale = a.state.delegates.self_entry();
    stale.url = a.url.clone();
    stale.last_seen = tokenshare_core::table::now_ms();
    c.state.delegates.merge([stale]);
    b.state.refresh_local().await;
    pull_table(&c.state, &b.url).await.unwrap();
    assert_eq!(c.state.delegates.peers_with_model("gemma2", &[]).len(), 2);

    // Whichever is tried first, A answers 401 and B serves every request.
    for _ in 0..4 {
        let r = chat(&c.url, None).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.text().await.unwrap();
    }
    assert_eq!(a.mock.chat_count(), 0);
    assert_eq!(b.mock.chat_count(), 4);
}

#[tokio::test]
async fn a_peer_refusing_our_secret_is_remembered_until_it_accepts_it() {
    let a = node(strict(&["gemma2:latest"])).await;
    let c = node(NodeOpts {
        client_only: true,
        secret: Some("another-secret"),
        ..Default::default()
    })
    .await;
    let mut stale = a.state.delegates.self_entry();
    stale.url = a.url.clone();
    stale.last_seen = tokenshare_core::table::now_ms();
    c.state.delegates.merge([stale]);

    // A forwarded request answered 401 counts, like one from gossip.
    let r = chat(&c.url, None).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(c.state.activity.secret_refusals(), vec![a.url.clone()]);

    // Once a request to it succeeds, it is no longer listed.
    let fixed = node(NodeOpts {
        client_only: true,
        secret: Some(SECRET),
        ..Default::default()
    })
    .await;
    fixed.state.activity.secret_refused_by(&a.url);
    pull_table(&fixed.state, &a.url).await.unwrap();
    assert!(fixed.state.activity.secret_refusals().is_empty());
}
