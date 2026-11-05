//! What delegates tell each other about their models beyond the names: sizes,
//! digests and details, so pooled models list like local ones and two builds
//! under one name can be told apart.

use serde_json::Value;
use tokenshare_core::mesh::{pull_table, rejoin};

mod common;
use common::{eventually, mock_tags_entry, node, Node, NodeOpts};

async fn get(url: &str) -> Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

async fn tags(n: &Node) -> Vec<Value> {
    get(&format!("{}/api/tags", n.url)).await["models"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn peer_models_list_with_their_real_size_digest_and_details() {
    let a = node(NodeOpts {
        models: &["gemma2:latest", "private:latest"],
        shared_models: &["gemma2"],
        ..Default::default()
    })
    .await;
    let me = a.state.delegates.self_entry();
    assert_eq!(me.model_info.len(), 1, "only shared models are described");
    assert_eq!(me.model_info[0].name, "gemma2:latest");

    let c = node(NodeOpts {
        fc: Some(&a.url),
        client_only: true,
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    a.state.refresh_local().await;
    pull_table(&c.state, &a.url).await.unwrap();

    let listed = tags(&c).await;
    assert_eq!(listed.len(), 1);
    let real = mock_tags_entry("gemma2:latest");
    for field in ["name", "size", "digest", "details"] {
        assert_eq!(listed[0][field], real[field], "{field}");
    }
}

#[tokio::test]
async fn two_builds_under_one_name_are_flagged() {
    let a = node(NodeOpts {
        models: &["llama3:latest", "gemma2:latest"],
        name: Some("a"),
        ..Default::default()
    })
    .await;
    let b = node(NodeOpts {
        fc: Some(&a.url),
        models: &["llama3:latest#pulled-later", "gemma2:latest"],
        name: Some("b"),
        ..Default::default()
    })
    .await;
    // The join itself carries B's model info.
    assert_eq!(rejoin(&b.state).await, Some(true));
    eventually(3, "A to list B", || a.state.delegates.peers().len() == 1).await;

    let status = get(&format!("{}/_ts/status", a.url)).await;
    let differing = status["differing_models"].as_array().unwrap();
    assert_eq!(differing.len(), 1, "{differing:?}");
    assert_eq!(differing[0]["model"], "llama3:latest");
    let mut ids: Vec<String> = differing[0]["variants"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|v| v["delegates"].as_array().unwrap().clone())
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut want = vec![a.state.id.clone(), b.state.id.clone()];
    want.sort();
    assert_eq!(ids, want);

    let page = reqwest::get(format!("{}/_ts/dashboard", a.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("differing_models"));
}
