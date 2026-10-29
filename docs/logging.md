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
`pool` usage, local ollama hosts and the delegate table. `tokenshare delegates`
prints the delegates this machine knows about.
