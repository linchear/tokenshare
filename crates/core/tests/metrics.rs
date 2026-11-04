//! `GET /_ts/metrics`: well-formed Prometheus text, with the values we expect.

use std::collections::{HashMap, HashSet};

use serde_json::json;
use tokenshare_core::mesh::rejoin;

mod common;
use common::{eventually, node, NodeOpts};

async fn scrape(url: &str) -> String {
    let r = reqwest::get(format!("{url}/_ts/metrics")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    r.text().await.unwrap()
}

/// Check the exposition format and return `name{labels}` -> value.
/// Every sample must belong to the family most recently declared by `# TYPE`
/// (histograms add `_bucket`, `_sum` and `_count`), and no family is declared twice.
fn parse(text: &str) -> HashMap<String, f64> {
    let mut samples = HashMap::new();
    let mut declared = HashSet::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').unwrap();
            assert!(["gauge", "counter", "histogram"].contains(&kind), "{line}");
            assert!(declared.insert(name.to_string()), "declared twice: {name}");
            current = Some((name.to_string(), kind.to_string()));
            continue;
        }
        if line.starts_with("# HELP ") {
            continue;
        }
        assert!(!line.trim().is_empty(), "blank line");
        let (series, value) = line.rsplit_once(' ').unwrap();
        let value: f64 = value
            .parse()
            .unwrap_or_else(|_| panic!("bad value: {line}"));
        let name = series.split('{').next().unwrap();
        let (family, kind) = current.as_ref().expect("sample before any # TYPE");
        let ok = match kind.as_str() {
            "histogram" => ["_bucket", "_sum", "_count"]
                .iter()
                .any(|s| name == format!("{family}{s}")),
            _ => name == family,
        };
        assert!(ok, "{name} outside its family {family}");
        if let Some(labels) = series.strip_prefix(name).and_then(|l| l.strip_prefix('{')) {
            assert!(labels.ends_with('}'), "{line}");
        }
        assert!(
            samples.insert(series.to_string(), value).is_none(),
            "duplicate series: {series}"
        );
    }
    samples
}

#[tokio::test]
async fn an_idle_delegate_exposes_its_state() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        pool: 4,
        ..Default::default()
    })
    .await;
    let m = parse(&scrape(&a.url).await);
    assert_eq!(m["tokenshare_state{state=\"active\"}"], 1.0);
    assert_eq!(m["tokenshare_state{state=\"down\"}"], 0.0);
    assert_eq!(m["tokenshare_pool_slots"], 4.0);
    assert_eq!(m["tokenshare_pool_slots_in_use"], 0.0);
    assert_eq!(m["tokenshare_sharing"], 1.0);
    let host = &a.mock.url;
    assert_eq!(m[&format!("tokenshare_ollama_up{{host=\"{host}\"}}")], 1.0);
    assert_eq!(
        m[&format!("tokenshare_ollama_models{{host=\"{host}\"}}")],
        1.0
    );
    assert_eq!(
        m["tokenshare_requests_rejected_total{reason=\"busy\"}"],
        0.0
    );
    assert_eq!(m["tokenshare_peers{status=\"online\"}"], 0.0);
}

#[tokio::test]
async fn finished_responses_count_and_time_and_tokens_add_up() {
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        fc: Some(&a.url),
        client_only: true,
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    a.state.refresh_local().await;
    tokenshare_core::mesh::pull_table(&c.state, &a.url)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    for _ in 0..2 {
        // The mock's OpenAI-style stream reports 1234 prompt and 56 completion tokens.
        let r = client
            .post(format!("{}/v1/chat/completions", c.url))
            .json(&json!({"model": "gemma2:latest", "messages": [], "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
    }
    let r = client
        .post(format!("{}/api/chat", c.url))
        .json(&json!({"model": "missing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    eventually(3, "both responses recorded", || {
        c.state.activity.snapshot()["recent"]
            .as_array()
            .unwrap()
            .len()
            == 2
    })
    .await;

    let m = parse(&scrape(&c.url).await);
    let peer = &a.url;
    let ok = format!(
        "tokenshare_upstream_responses_total{{route=\"peer\",target=\"{peer}\",result=\"ok\"}}"
    );
    assert_eq!(m[&ok], 2.0);
    assert_eq!(
        m[&format!("tokenshare_upstream_inflight{{target=\"{peer}\"}}")],
        0.0
    );
    let count =
        format!("tokenshare_response_duration_seconds_count{{route=\"peer\",target=\"{peer}\"}}");
    assert_eq!(m[&count], 2.0);
    let inf = format!(
        "tokenshare_response_ttfb_seconds_bucket{{route=\"peer\",target=\"{peer}\",le=\"+Inf\"}}"
    );
    assert_eq!(m[&inf], 2.0);
    assert_eq!(
        m["tokenshare_tokens_total{model=\"gemma2:latest\",kind=\"prompt\"}"],
        2468.0
    );
    assert_eq!(
        m["tokenshare_tokens_total{model=\"gemma2:latest\",kind=\"completion\"}"],
        112.0
    );
    assert_eq!(
        m["tokenshare_requests_rejected_total{reason=\"not_found\"}"],
        1.0
    );
    assert_eq!(m["tokenshare_peers{status=\"online\"}"], 1.0);
    assert_eq!(m[&format!("tokenshare_peer_slots{{peer=\"{peer}\"}}")], 3.0);
    assert_eq!(m["tokenshare_pool_slots_in_use"], 0.0);

    // The serving delegate counts the same responses as local ones.
    let am = parse(&scrape(&a.url).await);
    let local = format!(
        "tokenshare_upstream_responses_total{{route=\"local\",target=\"{}\",result=\"ok\"}}",
        a.mock.url
    );
    assert_eq!(am[&local], 2.0);
}

#[tokio::test]
async fn a_broken_stream_is_counted_by_its_problem() {
    let a = node(NodeOpts {
        models: &["broken"],
        ..Default::default()
    })
    .await;
    // The mock's "broken" model stops without a final chunk.
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", a.url))
        .json(&json!({"model": "broken", "messages": []}))
        .send()
        .await
        .unwrap();
    r.text().await.unwrap();
    eventually(3, "the response recorded", || !recent_is_empty(&a.state)).await;
    let m = parse(&scrape(&a.url).await);
    let key = format!(
        "tokenshare_upstream_responses_total{{route=\"local\",target=\"{}\",result=\"no_final_chunk\"}}",
        a.mock.url
    );
    assert_eq!(m[&key], 1.0);
}

fn recent_is_empty(s: &tokenshare_core::AppState) -> bool {
    s.activity.snapshot()["recent"]
        .as_array()
        .unwrap()
        .is_empty()
}
