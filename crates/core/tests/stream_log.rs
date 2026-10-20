//! How each streamed response ended is logged once, with enough detail to tell an
//! upstream that finished properly from one that died, was cut off by a missing
//! final chunk, or lost its client first.

use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokenshare_core::mesh::rejoin;
use tracing_subscriber::fmt::MakeWriter;

mod common;
use common::{eventually, node, NodeOpts};

/// Captures log output so a test can read it back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    /// The "response" log lines so far.
    fn responses(&self) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(" response") && l.contains("outcome="))
            .map(str::to_string)
            .collect()
    }
    async fn next_response(&self, after: usize) -> String {
        let me = self.clone();
        eventually(5, "a response log line", move || {
            me.responses().len() > after
        })
        .await;
        self.responses()[after].clone()
    }
}

/// Route this test's logs into a buffer. Tests run on a current-thread runtime,
/// so the thread-local default also covers the spawned servers.
fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let buf = Captured::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    (buf, tracing::subscriber::set_default(sub))
}

fn openai(model: &str) -> Value {
    json!({"model": model, "stream": true, "messages": []})
}

async fn post(url: &str, path: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}{path}"))
        .json(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_proper_openai_stream_is_logged_as_complete_with_its_finish_reason_and_tokens() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let resp = post(&a.url, "/v1/chat/completions", &openai("gemma2")).await;
    let body = resp.text().await.unwrap();
    assert!(body.contains("[DONE]"));

    let line = log.next_response(0).await;
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains("outcome=\"complete\""), "{line}");
    assert!(line.contains("route=\"local\""), "{line}");
    assert!(line.contains("model=\"gemma2\""), "{line}");
    assert!(line.contains("finish=\"stop\""), "{line}");
    assert!(line.contains("prompt_tokens=1234"), "{line}");
    assert!(line.contains("completion_tokens=56"), "{line}");
    assert!(line.contains(&format!("bytes={}", body.len())), "{line}");
    assert!(line.contains("problem=\"\""), "{line}");
}

#[tokio::test]
async fn a_stream_that_stops_without_a_final_chunk_is_flagged() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["broken:latest"],
        ..Default::default()
    })
    .await;
    // The upstream closes cleanly after two chunks: no finish_reason, no [DONE].
    // This is what an OpenAI client reports as "stream ended without finish_reason".
    let body = post(&a.url, "/v1/chat/completions", &openai("broken"))
        .await
        .text()
        .await
        .unwrap();
    assert!(!body.contains("[DONE]"));

    let line = log.next_response(0).await;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("outcome=\"complete\""), "{line}");
    assert!(line.contains("problem=\"no_final_chunk\""), "{line}");
    assert!(line.contains("finish=\"-\""), "{line}");
}

#[tokio::test]
async fn a_proper_ollama_stream_is_complete() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let body = post(
        &a.url,
        "/api/chat",
        &json!({"model": "gemma2", "messages": []}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert_eq!(body.lines().count(), 3);
    let line = log.next_response(0).await;
    assert!(
        line.contains("INFO") && line.contains("outcome=\"complete\""),
        "{line}"
    );
    assert!(line.contains("problem=\"\""), "{line}");
}

#[tokio::test]
async fn an_upstream_that_dies_mid_stream_is_flagged_with_the_error() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let resp = post(&a.url, "/api/generate", &json!({"model": "gemma2"})).await;
    assert!(resp.bytes().await.is_err());
    let line = log.next_response(0).await;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("outcome=\"upstream_error\""), "{line}");
    assert!(line.contains("problem=\"upstream_error\""), "{line}");
}

#[tokio::test]
async fn a_client_that_leaves_before_the_end_is_flagged() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        chunk_delay_ms: 150,
        ..Default::default()
    })
    .await;
    let mut resp = post(
        &a.url,
        "/api/chat",
        &json!({"model": "gemma2", "messages": []}),
    )
    .await;
    resp.chunk().await.unwrap().unwrap();
    drop(resp); // hang up mid-generation

    let line = log.next_response(0).await;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("outcome=\"client_gone\""), "{line}");
    assert!(line.contains("problem=\"client_left_early\""), "{line}");
}

#[tokio::test]
async fn a_response_through_a_peer_is_logged_with_its_route_and_the_request_size() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["gemma2:latest"],
        ..Default::default()
    })
    .await;
    let c = node(NodeOpts {
        client_only: true,
        fc: Some(&a.url),
        ..Default::default()
    })
    .await;
    assert_eq!(rejoin(&c.state).await, Some(true));
    let big = json!({"model": "gemma2", "stream": true, "messages": [{"role": "user", "content": "x".repeat(5000)}]});
    let size = serde_json::to_vec(&big).unwrap().len();
    post(&c.url, "/v1/chat/completions", &big)
        .await
        .text()
        .await
        .unwrap();

    // Both delegates log a response (C's from the peer, A's from its local ollama).
    let me = log.clone();
    eventually(5, "two response lines", move || me.responses().len() >= 2).await;
    let lines = log.responses();
    let via_peer = lines
        .iter()
        .find(|l| l.contains("route=\"peer\""))
        .expect("a peer-route line");
    assert!(
        via_peer.contains(&format!("request_bytes={size}")),
        "{via_peer}"
    );
    assert!(via_peer.contains(&a.url), "target is the peer: {via_peer}");
    assert!(lines.iter().any(|l| l.contains("route=\"local\"")));
}

#[tokio::test]
async fn an_error_event_from_the_upstream_is_logged_with_its_message() {
    let (log, _g) = capture();
    let a = node(NodeOpts {
        models: &["errored:latest"],
        ..Default::default()
    })
    .await;
    let body = post(&a.url, "/v1/chat/completions", &openai("errored"))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("llama runner process has terminated"));

    let line = log.next_response(0).await;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("problem=\"no_final_chunk\""), "{line}");
    // The reason ollama gave is in the log, so "ended without finish_reason" has a cause.
    assert!(
        line.contains("upstream_message=\"llama runner process has terminated: exit status 2\""),
        "{line}"
    );
    // The model's own words are never logged.
    assert!(!line.contains("hel"), "{line}");
}
