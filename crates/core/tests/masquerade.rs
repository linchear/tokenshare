//! A masquerading delegate has two listeners over one delegate: an ollama-facing
//! one (full behavior) and the tokenshare one, which acts as a delegate with no
//! models. These tests wire that up on random ports.

use std::future::pending;
use std::sync::Arc;

use serde_json::{json, Value};
use tokenshare_core::config::Face;
use tokenshare_core::mesh::rejoin;
use tokenshare_core::{serve_face, AppState, Config};
use tokio::net::TcpListener;

mod common;
use common::{eventually, mock_ollama, node, NodeOpts};

struct Masq {
    state: Arc<AppState>,
    /// The tokenshare port (a delegate with no models).
    mesh: String,
    /// The ollama-facing port (11434 in real life).
    ollama: String,
}

async fn masquerading(fc: Option<&str>, models: &[&str]) -> Masq {
    masquerading_with(fc, models, true).await
}

async fn masquerading_with(fc: Option<&str>, models: &[&str], share_with_peers: bool) -> Masq {
    let mock = mock_ollama(models, 0).await;
    let mesh_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ollama_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mesh_addr, ollama_addr) = (mesh_l.local_addr().unwrap(), ollama_l.local_addr().unwrap());
    let config = Config {
        port: mesh_addr.port(),
        masquerade: true,
        share_with_peers,
        fc: fc.map(str::to_string),
        ollama_hosts: vec![mock.url.clone()],
        ..Config::default()
    };
    let state = AppState::new(common::uuid_like(), config, None);
    state.refresh_local().await;
    tokio::spawn(serve_face(mesh_l, state.clone(), Face::Mesh, pending()));
    tokio::spawn(serve_face(ollama_l, state.clone(), Face::Ollama, pending()));
    Masq {
        state,
        mesh: format!("http://{mesh_addr}"),
        ollama: format!("http://{ollama_addr}"),
    }
}

async fn chat(url: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .json(&json!({"model": model, "messages": []}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_ollama_port_serves_the_local_ollama() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    let r = chat(&m.ollama, "gemma2").await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap().lines().count(), 3);
}

#[tokio::test]
async fn the_tokenshare_port_acts_as_a_delegate_with_no_models() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    // Same delegate, same local ollama, but this port never uses it.
    assert_eq!(chat(&m.mesh, "gemma2").await.status(), 404);
    assert_eq!(m.state.pool.in_use(), 0);
    let version = reqwest::get(format!("{}/api/version", m.mesh))
        .await
        .unwrap();
    assert_eq!(version.status(), 503, "no local ollama behind this port");
    // ...while the ollama port answers the same call.
    let version = reqwest::get(format!("{}/api/version", m.ollama))
        .await
        .unwrap();
    assert_eq!(version.status(), 200);
}

#[tokio::test]
async fn the_tokenshare_port_still_forwards_to_other_delegates() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading(Some(&peer.url), &["gemma2:latest"]).await;
    assert_eq!(rejoin(&m.state).await, Some(true));

    for url in [&m.mesh, &m.ollama] {
        let r = chat(url, "llama3.2").await;
        assert_eq!(r.status(), 200, "via {url}");
        assert_eq!(r.text().await.unwrap().lines().count(), 3);
    }
    assert_eq!(m.state.pool.in_use(), 0, "never touched the local ollama");
}

#[tokio::test]
async fn it_advertises_its_hosts_on_the_tokenshare_port_when_sharing() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading(Some(&peer.url), &["gemma2:latest"]).await;
    assert_eq!(rejoin(&m.state).await, Some(true));

    // The peer knows M by its tokenshare port and sees M's models.
    let entry = peer
        .state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == m.state.id)
        .expect("peer learned about the masquerading delegate");
    assert_eq!(entry.url, m.mesh);
    assert_eq!(entry.models, vec!["gemma2:latest".to_string()]);

    // So a request to the peer is served by M's ollama hosts, through M's tokenshare port.
    let r = chat(&peer.url, "gemma2").await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap().lines().count(), 3);
    // M can see the peer's models too.
    eventually(3, "M to know the peer", || {
        m.state.delegates.peers().len() == 1
    })
    .await;
}

#[tokio::test]
async fn with_sharing_off_it_advertises_no_models_and_peers_cannot_use_its_hosts() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading_with(Some(&peer.url), &["gemma2:latest"], false).await;
    assert_eq!(rejoin(&m.state).await, Some(true));

    let entry = peer
        .state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == m.state.id)
        .unwrap();
    assert_eq!(entry.url, m.mesh, "still advertised as a delegate");
    assert!(entry.models.is_empty(), "{entry:?}");
    assert_eq!(chat(&peer.url, "gemma2").await.status(), 404);
    // ...while M's own clients still use its hosts on the ollama port.
    assert_eq!(chat(&m.ollama, "gemma2").await.status(), 200);
}

#[tokio::test]
async fn a_peer_that_joins_via_the_ollama_port_still_learns_the_tokenshare_port() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    // Someone points their first contact at M's ollama-facing port.
    let joiner = node(NodeOpts {
        fc: Some(&m.ollama),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&joiner.state).await, Some(true));
    let m_as_seen = joiner.state.delegates.peers().into_iter().next().unwrap();
    assert_eq!(m_as_seen.url, m.mesh, "peers must use the tokenshare port");
    assert_eq!(m.state.delegates.self_entry().url, m.mesh);
}

async fn forwarded_chat(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/api/chat"))
        .header("x-tokenshare-visited", "some-peer")
        .json(&json!({"model": "gemma2", "messages": []}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn forwarded_requests_use_the_local_hosts_while_sharing() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    // From another delegate, on the tokenshare port (and the ollama one).
    for url in [&m.mesh, &m.ollama] {
        let r = forwarded_chat(url).await;
        assert_eq!(r.status(), 200, "via {url}");
        assert_eq!(r.text().await.unwrap().lines().count(), 3);
    }
}

#[tokio::test]
async fn forwarded_requests_are_refused_when_not_sharing() {
    let m = masquerading_with(None, &["gemma2:latest"], false).await;
    for url in [&m.mesh, &m.ollama] {
        assert_eq!(forwarded_chat(url).await.status(), 503, "via {url}");
    }
}

#[tokio::test]
async fn a_forwarded_request_for_a_model_the_hosts_lack_is_not_passed_on() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading(Some(&peer.url), &["gemma2:latest"]).await;
    assert_eq!(rejoin(&m.state).await, Some(true));
    // Already forwarded once, so M must not forward again, even though a peer has it.
    let r = reqwest::Client::new()
        .post(format!("{}/api/chat", m.mesh))
        .header("x-tokenshare-visited", "some-peer")
        .json(&json!({"model": "llama3.2", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn tags_on_the_tokenshare_port_lists_only_other_delegates_models() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading(Some(&peer.url), &["gemma2:latest"]).await;
    assert_eq!(rejoin(&m.state).await, Some(true));

    let names = |body: Value| -> Vec<String> {
        let mut v: Vec<String> = body["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let mesh: Value = reqwest::get(format!("{}/api/tags", m.mesh))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(names(mesh), vec!["llama3.2:latest"]);
    let ollama: Value = reqwest::get(format!("{}/api/tags", m.ollama))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(names(ollama), vec!["gemma2:latest", "llama3.2:latest"]);
}

#[tokio::test]
async fn status_reports_sharing_as_configured_while_masquerading() {
    for share in [true, false] {
        let m = masquerading_with(None, &["gemma2:latest"], share).await;
        let s: Value = reqwest::get(format!("{}/_ts/status", m.mesh))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(s["sharing"], share);
        assert_eq!(s["masquerade"], true);
        assert_eq!(s["port"], m.state.config.port);
    }
}
