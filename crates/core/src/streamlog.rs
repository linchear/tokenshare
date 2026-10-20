//! A one-line log of how each streamed response ended, so a report like "the
//! stream ended without a finish_reason" can be answered from the logs: did the
//! upstream finish properly, die, or did the client hang up first?
//!
//! Only metrics are logged. The response body is never stored; we keep the last
//! few KiB of it just long enough to look for the final chunk's markers (an SSE
//! `[DONE]` or `finish_reason`, an NDJSON `"done":true`, token counts).

use std::time::Instant;

use tracing::{info, warn};

/// How many trailing bytes of the response we keep to look for the final chunk.
const TAIL_BYTES: usize = 8 * 1024;

/// What kind of stream this is, which decides what a proper ending looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// OpenAI-style server-sent events (`/v1/...`): ends with a chunk carrying a
    /// `finish_reason`, then `data: [DONE]`.
    Sse,
    /// Ollama's native newline-delimited JSON: ends with `"done":true`.
    Ndjson,
    /// Anything else (a plain JSON body): nothing to look for.
    Other,
}

impl Kind {
    /// From the response's content type, falling back to the endpoint when it has
    /// none worth trusting: the chat and generate endpoints always end with
    /// `"done":true`, and the OpenAI-style ones with a `finish_reason`, whether or
    /// not they were streamed.
    pub fn for_response(content_type: Option<&str>, path: &str) -> Self {
        match content_type.map(|c| c.to_ascii_lowercase()) {
            Some(c) if c.starts_with("text/event-stream") => return Kind::Sse,
            Some(c) if c.contains("ndjson") => return Kind::Ndjson,
            _ => {}
        }
        match path {
            "/api/chat" | "/api/generate" => Kind::Ndjson,
            "/v1/chat/completions" | "/v1/completions" => Kind::Sse,
            _ => Kind::Other,
        }
    }
}

/// How the stream itself stopped, as far as the delegate could see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The upstream closed the response normally.
    Complete,
    /// Reading from the upstream failed.
    UpstreamError(String),
    /// This delegate was shut down mid-response.
    CutOff,
}

/// What the end of a response says about how it finished.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TailInfo {
    /// The terminator was seen: `[DONE]` (SSE) or `"done":true` (NDJSON).
    pub done: bool,
    /// The last `finish_reason` / `done_reason` value.
    pub finish: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

fn last_after(text: &str, key: &str) -> Option<usize> {
    text.rfind(key).map(|i| i + key.len())
}

fn number_after(text: &str, keys: &[&str]) -> Option<u64> {
    let start = keys.iter().filter_map(|k| last_after(text, k)).max()?;
    let digits: String = text[start..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn string_after(text: &str, keys: &[&str]) -> Option<String> {
    let start = keys.iter().filter_map(|k| last_after(text, k)).max()?;
    let rest = text[start..].trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// Look at the last bytes of a response for the markers of a proper ending.
pub fn scan_tail(tail: &[u8], kind: Kind) -> TailInfo {
    let text = String::from_utf8_lossy(tail);
    let done = match kind {
        Kind::Sse => text.contains("[DONE]"),
        Kind::Ndjson => text.contains("\"done\":true") || text.contains("\"done\": true"),
        Kind::Other => false,
    };
    TailInfo {
        done,
        // `"finish_reason":null` appears in every OpenAI chunk; only a string counts.
        finish: string_after(&text, &["\"finish_reason\":", "\"done_reason\":"]),
        prompt_tokens: number_after(&text, &["\"prompt_tokens\":", "\"prompt_eval_count\":"]),
        completion_tokens: number_after(&text, &["\"completion_tokens\":", "\"eval_count\":"]),
    }
}

/// Did the response end the way this kind of stream should?
pub fn properly_terminated(kind: Kind, info: &TailInfo) -> bool {
    match kind {
        Kind::Sse => info.done || info.finish.is_some(),
        Kind::Ndjson => info.done,
        Kind::Other => true,
    }
}

/// Static facts about the request a response belongs to.
pub struct StreamMeta {
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    /// `local` (our ollama), `peer` (another delegate) or `passthrough`.
    pub route: &'static str,
    /// The ollama host or peer URL that produced the response.
    pub target: String,
    pub request_bytes: usize,
    /// When the request arrived.
    pub started: Instant,
    pub status: u16,
    pub kind: Kind,
    pub ttfb_ms: u64,
}

/// Follows one response as it streams and logs how it ended. The log line is
/// written when this is dropped, which is when the stream finishes or the client
/// goes away, whichever comes first.
pub struct StreamTracker {
    meta: StreamMeta,
    bytes: u64,
    tail: Vec<u8>,
    outcome: Option<Outcome>,
}

impl StreamTracker {
    pub fn new(meta: StreamMeta) -> Self {
        Self {
            meta,
            bytes: 0,
            tail: Vec::new(),
            outcome: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        if self.meta.kind == Kind::Other {
            return;
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TAIL_BYTES * 2 {
            let cut = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }

    pub fn finish(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }
}

macro_rules! emit {
    ($level:ident, $m:expr, $outcome:expr, $problem:expr, $bytes:expr, $info:expr, $error:expr) => {
        $level!(
            method = %$m.method,
            path = %$m.path,
            model = $m.model.as_deref().unwrap_or("-"),
            route = $m.route,
            target = %$m.target,
            status = $m.status,
            outcome = $outcome,
            problem = $problem,
            bytes = $bytes,
            request_bytes = $m.request_bytes as u64,
            ttfb_ms = $m.ttfb_ms,
            total_ms = $m.started.elapsed().as_millis() as u64,
            finish = $info.finish.as_deref().unwrap_or("-"),
            prompt_tokens = $info.prompt_tokens.map_or(-1, |n| n as i64),
            completion_tokens = $info.completion_tokens.map_or(-1, |n| n as i64),
            error = $error,
            "response"
        )
    };
}

impl Drop for StreamTracker {
    fn drop(&mut self) {
        let m = &self.meta;
        let info = scan_tail(&self.tail, m.kind);
        let ended_properly = properly_terminated(m.kind, &info);
        // An error status is the upstream's answer, not a broken stream.
        let ok_status = (200..300).contains(&m.status);
        let (outcome, problem, error) = match &self.outcome {
            Some(Outcome::Complete) if ok_status && !ended_properly => {
                ("complete", "no_final_chunk", String::new())
            }
            Some(Outcome::Complete) => ("complete", "", String::new()),
            Some(Outcome::UpstreamError(e)) => ("upstream_error", "upstream_error", e.clone()),
            Some(Outcome::CutOff) => ("cut_off", "cut_off", String::new()),
            None if ok_status && !ended_properly => {
                ("client_gone", "client_left_early", String::new())
            }
            None => ("client_gone", "", String::new()),
        };
        if problem.is_empty() {
            emit!(info, m, outcome, problem, self.bytes, info, error);
        } else {
            emit!(warn, m, outcome, problem, self.bytes, info, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_END: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":32770,\"completion_tokens\":6590,\"total_tokens\":39360}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn openai_stream_with_a_proper_ending() {
        let info = scan_tail(OPENAI_END.as_bytes(), Kind::Sse);
        assert!(info.done);
        assert_eq!(info.finish.as_deref(), Some("stop"));
        assert_eq!(info.prompt_tokens, Some(32770));
        assert_eq!(info.completion_tokens, Some(6590));
        assert!(properly_terminated(Kind::Sse, &info));
    }

    #[test]
    fn openai_stream_cut_before_the_final_chunk_is_not_properly_terminated() {
        let cut =
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
        let info = scan_tail(cut.as_bytes(), Kind::Sse);
        assert!(!info.done);
        assert_eq!(
            info.finish, None,
            "a null finish_reason is not a finish_reason"
        );
        assert!(!properly_terminated(Kind::Sse, &info));
    }

    #[test]
    fn a_finish_reason_without_done_still_counts() {
        let s = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n";
        assert!(properly_terminated(
            Kind::Sse,
            &scan_tail(s.as_bytes(), Kind::Sse)
        ));
    }

    #[test]
    fn ollama_ndjson_ending() {
        let s = "{\"message\":{\"content\":\"a\"},\"done\":false}\n{\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":120,\"eval_count\":45}\n";
        let info = scan_tail(s.as_bytes(), Kind::Ndjson);
        assert!(info.done);
        assert_eq!(info.finish.as_deref(), Some("stop"));
        assert_eq!(
            (info.prompt_tokens, info.completion_tokens),
            (Some(120), Some(45))
        );
        assert!(properly_terminated(Kind::Ndjson, &info));
        let cut = "{\"message\":{\"content\":\"a\"},\"done\":false}\n";
        assert!(!properly_terminated(
            Kind::Ndjson,
            &scan_tail(cut.as_bytes(), Kind::Ndjson)
        ));
    }

    #[test]
    fn plain_json_has_nothing_to_check() {
        assert!(properly_terminated(Kind::Other, &TailInfo::default()));
    }

    #[test]
    fn kind_comes_from_the_content_type_then_the_endpoint() {
        let k = Kind::for_response;
        assert_eq!(k(Some("text/event-stream"), "/x"), Kind::Sse);
        assert_eq!(k(Some("text/event-stream; charset=utf-8"), "/x"), Kind::Sse);
        assert_eq!(k(Some("application/x-ndjson"), "/x"), Kind::Ndjson);
        // No streaming content type: the endpoint says what its ending looks like.
        assert_eq!(k(Some("application/json"), "/api/chat"), Kind::Ndjson);
        assert_eq!(k(None, "/api/generate"), Kind::Ndjson);
        assert_eq!(
            k(Some("application/json"), "/v1/chat/completions"),
            Kind::Sse
        );
        // Embeddings, tags and the like have no final-chunk marker to look for.
        assert_eq!(k(Some("application/json"), "/api/embed"), Kind::Other);
        assert_eq!(k(None, "/api/tags"), Kind::Other);
    }

    #[test]
    fn only_the_tail_is_kept_and_bytes_are_counted() {
        let mut t = StreamTracker::new(StreamMeta {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            model: None,
            route: "local",
            target: "x".into(),
            request_bytes: 0,
            started: Instant::now(),
            status: 200,
            kind: Kind::Sse,
            ttfb_ms: 0,
        });
        for _ in 0..100 {
            t.feed(&[b'x'; 1000]);
        }
        t.feed(b"data: [DONE]\n\n");
        assert_eq!(t.bytes, 100_000 + 14);
        assert!(t.tail.len() <= TAIL_BYTES * 2 + 14);
        assert!(scan_tail(&t.tail, Kind::Sse).done);
        t.finish(Outcome::Complete);
    }
}
