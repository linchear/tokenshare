# Token Share: Implementation Plan

Language: Rust (tokio, axum/hyper, reqwest, tracing, serde). Workspace: `core` (lib), `tokenshare` (headless daemon), `tray` (menubar/taskbar wrapper).

## Platforms

macOS, Windows, and Linux are all first-class targets. The tray UI is macOS and Windows only. Linux runs the headless daemon.
- The core and daemon use only cross-platform crates, with no platform-specific code outside the tray and packaging.
- Config, data, and state dirs come from the `directories` crate (XDG on Linux, `Application Support` on macOS, `AppData` on Windows).
- CI builds and runs the integration tests on all three OSes.
- Avoid Unix-only APIs (signals, paths). Use `tokio::signal` with Ctrl-C and SIGTERM handled where available.
- The daemon can run as a service on each OS: launchd, a Windows login item or service, and a systemd user unit.

## Decisions (from design review)

- **Local ollama table**: the list of ollama instances reachable directly from this delegate, with the models each serves. Built by polling `/api/tags`.
- **Port sharing**: coordination lives under `/_ts/*`. Everything else is ollama passthrough. `/api/tags` returns the union of models across the pool. Other non-inference endpoints (`pull`, etc.) go to local ollama only.
- **Model routing**: parse the JSON body to read `model`. Buffer the body so it can be replayed to another delegate. Stream responses through unbuffered.
- **Streaming** (per the design's Streaming section):
  - Never depend on the client's `stream` flag. Ollama streams NDJSON by default, so every response is passed through as it arrives.
  - The request body is buffered, capped at ~256 MB. The response is not buffered. Content-Length and hop-by-hop headers are dropped, and the response goes out chunked.
  - No total timeout on upstream requests, only a connect timeout.
  - The pool slot lives as long as the response stream. It frees when the stream ends or is dropped. A client disconnect drops the upstream connection, so ollama cancels generation.
  - Retry or try-next only happens before any response byte has been sent. After streaming starts, an upstream failure cuts the client connection (a truncated stream, never a clean end).
  - A forwarded request streams the peer's response through the same way and carries the visited header.
- **Loop prevention**: forwarded requests carry an `X-TokenShare-Visited` header (list of delegate IDs). A delegate receiving a forwarded request serves it locally or returns an error. It never forwards again.
- **Pool**: a local counter (semaphore), default 3, configurable. Full means the counter is exhausted.
- **ID**: random UUID, persisted in the config dir. No MAC hash.
- **Keepalive gossip**: each message has a message ID and a per-entry version (last_seen). A seen-cache drops duplicates, and a TTL bounds the spread.
- **Table sync**: every period, request the full table from one random delegate and merge. For each entry, the entry with the later `last_seen` wins. The delegate's own entry takes precedence if its `last_seen` is later.
- **FC is not special**: it is just the first contact. All delegates persist their table to disk.
- **Restart/rejoin**: on start, load the persisted table. Announce self to a random delegate and request its table. If that delegate is unreachable, mark it inactive and try the next. If none respond, retry in 5 minutes. If there is no persisted table, use `fc`.
- **Auth**: none for now.
- **Metrics**: skip GPU/CPU/VRAM/RAM collection. Table fields stay optional so they can be added later.
- **Tray state**: green = active, blue = in-use (at least one active request, local or forwarded), red = down.
- **"All full" response**: the design says 500, but 503 with `Retry-After` is more accurate. Defaulting to 503 unless you say otherwise.

## Phases

### M1: Single-node proxy (done, commit fdd14f2)
**Phase 0, Scaffold**
- Cargo workspace, CI (fmt, clippy, test on macOS, Windows, Linux).
- Config via file, env, and CLI flags: `port` (11343), `fc`, `ollama_hosts`, `pool_size` (3), `keepalive_interval`, `sync_interval`.
- Persisted UUID in the config dir.

**Phase 1, Local proxy**
- axum server on the configured port.
- Local ollama table with periodic `/api/tags` polling.
- Model extraction, a bounded pool, streaming passthrough. Streaming behavior is implemented as in the Streaming decision above, except for the items listed in the mid-stream and client-disconnect tests under Phase 3.
- Request-metrics logging with no content.
- Tests against a mock ollama.

### M2: Mesh and routing (done)
**Phase 2, Delegate table and join**
- Table entries: id, url, status, type, models, last_seen. Load, vram, and ram are optional.
- Stable table hash.
- Disk persistence.
- `POST /_ts/join`: identify, add, return table.
- `POST /_ts/announce` (`new-member`, id, info, table hash, count): the receiver adds the member. On a hash or count mismatch, it fetches the full table.
- `GET /_ts/table`.
- Startup and rejoin flow as above.

**Phase 3, Routing**
- Try local first (model present and a pool slot free).
- Otherwise try peers that have the model, in random order, skipping unavailable ones. Move on when a peer is full or returns non-2xx.
- Retry only before any response bytes have been streamed (connect errors, a full peer returning 503, non-2xx). The buffered request body is replayed to the next candidate.
- Forwarded responses stream through the same path as local ones, holding no local pool slot (the peer holds its own).
- Return 503 with `Retry-After` if all are exhausted.
- Loop prevention via the visited header.
- Streaming tests:
  - Forwarded response arrives chunk by chunk, not buffered (check first chunk before the last is produced).
  - A peer that fails before sending bytes causes a retry on the next peer.
  - A peer that dies mid-stream truncates the client stream with an error, with no retry and no clean end.
  - A client disconnect during a forwarded stream drops the connection to the peer.
  - Add the mid-stream upstream failure test for the local path too. M1 covers client disconnect but not upstream failure mid-stream.

### M3: Self-healing (done)
**Phase 4, Keepalive and sync**

As built:
- Config: `keepalive_interval_secs` (30), `keepalive_ttl` (3 relay hops), `sync_interval_secs` (60).
- A keepalive carries the origin's own entry and a message ID. Receivers drop duplicate IDs, merge the entry, and relay it to one random peer other than the one that sent it and the origin.
- Besides keepalive and sync failures, a failed announce or a refused connection while forwarding a request also marks the peer unavailable.
- Peers are tried online-first. If none are online, unavailable peers are tried, so an isolated delegate can still reach out.
- Status is a local judgement and never travels with a sync: an unavailable peer returns only via a keepalive with a newer `last_seen`.

Original scope:
- Periodic keepalive (models, last_seen) to one random peer.
- On timeout, mark the peer unavailable and try another.
- On receipt, update the sender's entry, then forward to a random peer other than the sender. Dedupe by message ID, with a TTL.
- A peer is marked available again only by a keepalive received from it.
- Periodic full-table pull and merge, with the later `last_seen` winning.
- Fault-injection tests: node death, partition, rejoin.

### M4: UI
**Phase 5, Status and tray**
- `GET /_ts/status` (state and table).
- Tray icon with the three colors, a peer-list menu, and quit.
- macOS menubar `.app` and Windows tray exe packaging. The tray exists on these two platforms only.
- Linux is headless: no tray or UI. Package the daemon as a binary (plus a .deb and a systemd user unit). Status is available through `/_ts/status` and logs.

**Phase 6, Hardening and docs**
- Graceful shutdown with in-flight drain.
- README, including the full-mesh scaling limit.
- Later options: partial gossip views, metrics, auth.
