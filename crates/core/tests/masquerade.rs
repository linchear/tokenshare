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
    let mock = mock_ollama(models, 0).await;
    let mesh_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ollama_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mesh_addr, ollama_addr) = (mesh_l.local_addr().unwrap(), ollama_l.local_addr().unwrap());
    let config = Config {
        port: mesh_addr.port(),
        masquerade: true,
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
async fn it_advertises_itself_on_the_tokenshare_port_with_no_models() {
    let peer = node(NodeOpts {
        models: &["llama3.2:latest"],
        ..Default::default()
    })
    .await;
    let m = masquerading(Some(&peer.url), &["gemma2:latest"]).await;
    assert_eq!(rejoin(&m.state).await, Some(true));

    // The peer knows M by its tokenshare port, and M lists no models.
    let entry = peer
        .state
        .delegates
        .peers()
        .into_iter()
        .find(|p| p.id == m.state.id)
        .expect("peer learned about the masquerading delegate");
    assert_eq!(entry.url, m.mesh);
    assert!(entry.models.is_empty(), "{entry:?}");

    // So the peer never sends gemma2 to M, even though M's ollama has it.
    assert_eq!(chat(&peer.url, "gemma2").await.status(), 404);
    // And M can see the peer's models.
    eventually(3, "M to know the peer", || {
        m.state.delegates.peers().len() == 1
    })
    .await;
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

#[tokio::test]
async fn forwarded_requests_are_refused_by_a_masquerading_delegate() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    for url in [&m.mesh, &m.ollama] {
        let r = reqwest::Client::new()
            .post(format!("{url}/api/chat"))
            .header("x-tokenshare-visited", "some-peer")
            .json(&json!({"model": "gemma2", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503, "via {url}");
    }
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
async fn status_reports_both_ports_and_that_nothing_is_shared() {
    let m = masquerading(None, &["gemma2:latest"]).await;
    let s: Value = reqwest::get(format!("{}/_ts/status", m.mesh))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["sharing"], false);
    assert_eq!(s["masquerade"], true);
    assert_eq!(s["port"], m.state.config.port);
}
