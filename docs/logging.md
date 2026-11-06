# Logging

Set `RUST_LOG` to change the level (the default is `info`). The daemon logs to
stderr; the tray apps write `tokenshare.log` in the data dir
([where](configuration.md#where-files-live)). **Only metrics are logged, never
request or response content.**

## Requests and responses

Each call logs a `request` line when the response starts:

```
INFO request method=POST path="/v1/chat/completions" model="qwen3:14b" target="http://100.95.20.66:11343" status=200 forwarded=false ttfb_ms=7073
```

and every proxied response logs one `response` line when it **ends** (at `info`, or
`warn` when something looks wrong), so a client's "stream ended without
finish_reason" can be traced to its cause:

```
INFO response model="gemma2" route="local" outcome="complete" problem="" bytes=347 request_bytes=80 ttfb_ms=1 total_ms=206 finish="stop" prompt_tokens=61820 completion_tokens=412
WARN response model="gemma2" route="local" outcome="complete" problem="no_final_chunk" bytes=176 ...
WARN response model="gemma2" route="local" outcome="client_gone" problem="client_left_early" ...
```

| Field | Meaning |
| --- | --- |
| `route` | `local` (this machine's ollama), `peer` (another delegate) or `passthrough`; `target` is the host or peer URL that answered |
| `outcome` | `complete` (the upstream closed normally), `upstream_error` (reading it failed, with `error`), `cut_off` (this delegate was shut down mid-response), or `client_gone` (the client hung up first) |
| `problem` | empty when healthy, else `no_final_chunk`, `client_left_early`, `upstream_error` or `cut_off` |
| `upstream_message` | an error the upstream wrote into the stream itself (`{"error":...}`), which is how ollama reports a failure after the response has begun, for example a crashed runner |
| `finish`, `prompt_tokens`, `completion_tokens` | read from the final chunk (`-1` when absent) |
| `request_bytes`, `bytes`, `ttfb_ms`, `total_ms` | request size, response size, time to first byte and total time |

How to read the problems:

- `no_final_chunk`: the upstream ended cleanly but never sent a final
  `finish_reason`, `[DONE]` or `"done":true`. This is what an OpenAI client reports
  as "ended without finish_reason", so look at the upstream, and at
  `upstream_message`.
- `client_left_early`: the client gave up before the final chunk, for example a
  client timeout.
- `prompt_tokens` far below what you sent means the upstream truncated your prompt.

The delegate keeps the last 8 KiB of each response in memory just to find the final
chunk; the body is never stored.

## Status

`GET /_ts/status` returns JSON: `state`, `draining`, `sharing`, `masquerade`,
`pool` usage, local ollama hosts (with the models each has loaded), the delegate
table (with each delegate's published load), and `activity`: per upstream (local
ollama host or peer URL) the requests in flight, completed and problem responses,
refusals and the average time to first byte, the count of requests turned away
(busy or unknown model), and the last 100 responses. Like the log, these are
metrics only, never content. `tokenshare delegates` prints the delegates this
machine knows about.

## Metrics

`GET /_ts/metrics` serves the same information in the Prometheus text format, for
Prometheus, Grafana Agent, the Datadog agent's OpenMetrics check and similar. It
lives under `/_ts/` because every other path is ollama's. A scrape config:

```yaml
scrape_configs:
  - job_name: tokenshare
    metrics_path: /_ts/metrics
    static_configs:
      - targets: ["gpu-box:11343", "laptop:11343"]
    # Only with a pool secret:
    authorization:
      credentials: correct-horse-battery-staple
```

| Metric | Type | Labels | |
| --- | --- | --- | --- |
| `tokenshare_info` | gauge | `id`, `name`, `version` | always 1 |
| `tokenshare_state` | gauge | `state` | 1 for the current state (`active`, `in-use`, `no-models`, `down`) |
| `tokenshare_draining`, `tokenshare_sharing` | gauge | | 0 or 1 |
| `tokenshare_auth_required` | gauge | | 1 if this delegate requires the pool secret |
| `tokenshare_pool_slots`, `tokenshare_pool_slots_in_use` | gauge | | local ollama slots |
| `tokenshare_pool_peer_slots`, `tokenshare_pool_peer_slots_in_use` | gauge | | the slots other delegates may use (`max_peer_requests`), and how many they use |
| `tokenshare_forwarding` | gauge | | requests being forwarded to peers |
| `tokenshare_requests_waiting` | gauge | | requests waiting for a free slot ([`queue_timeout_secs`](configuration.md)) |
| `tokenshare_ollama_up`, `_models`, `_loaded_models` | gauge | `host` | per local ollama host |
| `tokenshare_peers` | gauge | `status` | known peers by status |
| `tokenshare_peer_up` | gauge | `peer`, `id` | 1 if online |
| `tokenshare_peer_info` | gauge | `peer`, `id`, `name` | always 1; the peer's name, to join on `peer` |
| `tokenshare_peer_models`, `_loaded_models`, `_slots`, `_slots_busy` | gauge | `peer` | as the peer last published them |
| `tokenshare_peer_last_seen_timestamp_seconds` | gauge | `peer` | the peer's own clock |
| `tokenshare_upstream_inflight` | gauge | `target` | this delegate's requests in flight per upstream |
| `tokenshare_upstream_responses_total` | counter | `route`, `target`, `result` | finished responses; `result` is `ok`, `error_status` or the stream problem (`no_final_chunk`, `upstream_error`, `cut_off`, `client_left_early`) |
| `tokenshare_upstream_refusals_total` | counter | `target`, `reason` | `declined` or `unreachable` before streaming |
| `tokenshare_requests_rejected_total` | counter | `reason` | `busy` (503, after waiting), `not_found` (404) or `unauthorized` (401, any request without the pool secret) |
| `tokenshare_response_ttfb_seconds` | histogram | `route`, `target` | request to response headers |
| `tokenshare_response_duration_seconds` | histogram | `route`, `target` | request to end of stream |
| `tokenshare_tokens_total` | counter | `model`, `kind` | `prompt` and `completion` tokens from final chunks |

`route` is `local`, `peer` or `passthrough`; `target` is an ollama host or a peer's
URL. Counters start at zero when the delegate starts. Each delegate reports only
its own view, so scrape every delegate: a request forwarded from C to A shows up as
`route="peer"` on C and `route="local"` on A. Without a
[pool secret](configuration.md#pool-secret) anyone who can reach the port can
scrape it; with one, a scraper on another machine needs the secret (above).

## Dashboard

`http://<delegate>:11343/_ts/dashboard` is a page that polls `/_ts/status` every
two seconds and shows the delegates with their slots and warm models, the local
ollama hosts, and recent responses with how they ended. Hovering over a model
shows its parameter size, quantization, size and digest. A model marked **≠** is
one that delegates have in different builds (`differing_models` in
`/_ts/status`), so the answer can depend on which machine serves it. A peer
tagged **refuses our secret** answered one of this delegate's requests with 401
(`activity.secret_refused` in `/_ts/status`). The tray app opens it from
**Open Dashboard**. It needs no internet access, and it shows only what
`/_ts/status` does. With a [pool secret](configuration.md#pool-secret), opening it
from another machine asks for the secret, which is kept for that browser tab
only.
