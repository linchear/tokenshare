# Token Share

Route [ollama](https://ollama.com) calls among several machines. Each machine
runs a small program, a **delegate**, that listens on one port (default
`11343`). Point your ollama client at any delegate. It serves the request from
its own ollama if it can, and otherwise streams it from a peer that has the
model.

- macOS: menubar app (`Token Share.app`). Windows: taskbar app.
- Linux: headless daemon, no UI.
- Machines must be on a trusted network (LAN or a VPN such as Tailscale).
  There is **no authentication**.

## How it works

- **Delegate table.** Every delegate keeps a table of all delegates: id, URL,
  status, models, `last_seen`. It is persisted to disk.
- **Joining.** A delegate with `fc = "http://host:11343"` (the *first contact*,
  an ordinary delegate) introduces itself to it at startup and receives its
  table. The first contact tells everyone else about the newcomer. On
  restart a delegate tries the peers in its saved table (random order), then
  `fc`. It marks unreachable ones `inactive`, and if none answer it retries
  after `rejoin_retry_secs` (5 minutes).
- **Keepalives.** Every `keepalive_interval_secs` a delegate sends its entry
  (models, `last_seen`) to one random peer. The receiver merges it and relays
  it to one random other peer, up to `keepalive_ttl` hops, dropping duplicates
  by message ID. A peer that does not answer is marked `unavailable`. It comes
  back only when a keepalive from it arrives.
- **Table sync.** Every `sync_interval_secs` a delegate pulls the full table
  from one random peer and merges it. For each entry the later `last_seen`
  wins, and a delegate's own entry is never overwritten by others.
  This repairs missed announcements.
- **Routing.** For `/api/chat`, `/api/generate`, `/api/embed`,
  `/api/embeddings` and `/v1/{chat/completions,completions,embeddings}`, the
  delegate reads `model` from the body, then:
  1. serves it locally if its ollama has the model and a pool slot is free
     (`pool_size`, default 3);
  2. otherwise tries online peers that have the model, in random order, moving
     on if a peer is full (503), errors, or is unreachable;
  3. otherwise returns `503` with `Retry-After` (or `404` if nobody has the
     model).

  A forwarded request carries `X-TokenShare-Visited`; the receiver serves it
  itself or rejects it and never forwards again, so there are no loops.
- **Streaming.** The request body is buffered (up to 256 MB) so it can be read
  and replayed to the next peer. The response is never buffered: ollama's
  NDJSON is passed through as it arrives, whatever the client's `stream` flag.
  There is no total timeout. Retrying to another peer happens only before the
  first response byte; if an upstream dies mid-stream, the client connection is
  broken so a truncated response is never mistaken for a complete one. If the
  client disconnects, the upstream request is dropped and ollama stops
  generating.
- **Other endpoints.** `GET /api/tags` returns the union of models across the
  pool. Everything else (`pull`, `show`, ...) goes to this machine's own ollama
  only.

## Configuration

A TOML file; every key is optional. Command-line flags override it.

| Key | Default | Meaning |
| --- | --- | --- |
| `port` | `11343` | Port for ollama and coordination traffic |
| `fc` | none | First-contact delegate URL |
| `advertise_url` | learned | How peers reach this delegate (learned from the first delegate it joins) |
| `ollama_hosts` | `["http://localhost:11434"]` | Ollama instances reachable from this machine. Optional: `[]` makes a client-only delegate |
| `pool_size` | `3` | Max concurrent requests to ollama |
| `share_with_peers` | `true` | Let other delegates use this machine's ollama (see below) |
| `poll_interval_secs` | `10` | How often to refresh local models |
| `rejoin_retry_secs` | `300` | Wait before retrying when no delegate answered |
| `keepalive_interval_secs` | `30` | Keepalive period |
| `keepalive_ttl` | `3` | Extra relay hops per keepalive |
| `sync_interval_secs` | `60` | Full table pull period |
| `shutdown_grace_secs` | `30` | How long to let in-flight requests finish on shutdown |

```toml
fc = "http://192.168.2.3:11343"
ollama_hosts = ["http://localhost:11434"]
pool_size = 3
```

Default locations (the `directories` crate; the delegate ID and table live in
the data dir, which is also where the tray app writes `tokenshare.log`):

| | Config file | Data dir |
| --- | --- | --- |
| macOS | `~/Library/Application Support/tokenshare/config.toml` | `~/Library/Application Support/tokenshare/` |
| Linux | `~/.config/tokenshare/config.toml` | `~/.local/share/tokenshare/` |
| Windows | `%APPDATA%\tokenshare\config\config.toml` | `%APPDATA%\tokenshare\data\` |

## Running

Headless daemon (all platforms, the only option on Linux):

```bash
cargo run --release -p tokenshare -- --fc http://192.168.2.3:11343
```

Flags: `--config`, `--port`, `--fc`, `--advertise-url`, `--ollama-host`
(repeatable), `--client-only`, `--pool-size`. Set `RUST_LOG` to change the log level. Logs list
metrics only (method, path, model, target, status, time to first byte), never
request content. On Linux, `packaging/linux/tokenshare.service` is a systemd
user unit.

macOS menubar app:

```bash
packaging/macos/build-app.sh   # builds target/release/bundle/Token Share.app
```

The tray apps run the delegate themselves; do not also run the daemon on the
same port. The icon is a round badge with a symbol, so state does not depend on color
alone: a green **check** (✅) when up and idle, a blue **lightning bolt** (⚡)
while any request is in flight (served here or forwarded), and a red **cross**
(❌) when the delegate cannot serve anything (no local ollama and no online peer)
or failed to start. The menu
shows state, pool usage and the delegate list. Quit waits up to 5 seconds for
in-flight requests.

**Client only.** Ollama hosts are optional. With none (`ollama_hosts = []`, the
`--client-only` flag, or an empty field in Settings) the delegate serves nothing
itself and forwards every model request to a delegate that has the model, so
machines without a GPU can just consume the pool. `/api/tags` still lists the
pool's models; endpoints that only make sense locally (`pull`, `show`, ...)
return 503. It reads as **down** until it has at least one online peer, and the
menu shows "(client only)".

**Not sharing.** With `share_with_peers = false` (the *Share my ollama with other
delegates* checkbox in Settings) this delegate looks like its ollama is down to
everyone else: it advertises no models, so peers do not choose it, and it refuses
any forwarded request with 503 in case a peer's view is stale. Its own users are
unaffected: requests to it are still served by its local ollama and, failing
that, by other delegates. The menu shows "(not sharing)" and `/_ts/status`
reports `"sharing": false`.

**Settings.** Choose *Settings…* in the tray menu to edit the first contact,
ollama hosts (one per line), whether to share your ollama, connections to ollama, listen port and, under
Advanced, the advertise URL. *Save & Restart* validates the values, writes the
config file and restarts the delegate with them; requests in flight get a few
seconds to finish. Other options (intervals, TTL, grace period) are kept as they
are in the file, so edit it directly for those. On macOS the app shows a Dock icon
while the window is open. The window needs WebView2 on Windows (included in
Windows 11 and current Windows 10).

`GET /_ts/status` returns the same picture as JSON: `state`, `draining`,
`pool`, local ollama hosts and the delegate table.

## App icon

A blue squircle with a mesh of three linked nodes and a gold token in the middle;
more tokens travel along the links. It is drawn in code
(`crates/tray/examples/app_icon.rs`), and the generated assets are committed:
`packaging/macos/AppIcon.icns` (bundled by `build-app.sh`) and
`crates/tray/icon.ico` (embedded in `tokenshare-tray.exe` by `crates/tray/build.rs`).
After changing the artwork, run `packaging/icon/generate.sh` on a Mac. This is
separate from the small status icons in the menubar/taskbar.

## Shutdown

On SIGINT/SIGTERM (or Quit in the tray) a delegate stops gossiping, refuses new
model requests with 503 (peers treat that as "try the next one"), and waits up
to `shutdown_grace_secs` for requests in flight, local or forwarded, to finish.
Anything still streaming after that is cut off with a broken stream.

## Limits and caveats

- **Full mesh.** Every delegate learns about every other, and a join is
  announced to all of them. This does not scale to large pools; plan for tens
  of delegates, not thousands. Partial gossip views would be the next step.
- **Clocks.** "Later `last_seen` wins" compares timestamps from different
  machines, so keep clocks roughly in sync (NTP).
- **No auth, no encryption.** Run it only on a trusted network.
- **Load balancing is random**, not load-aware. CPU/GPU/RAM metrics are not
  collected yet.
- **`last_seen` is not "seen by me".** A relayed keepalive refreshes an entry
  with the origin's timestamp, so an entry can be marked online on the word of a
  third party until a direct request fails.

## Differences from `design/design.md`

- "All full" returns **503** with `Retry-After`, not 500.
- The delegate ID is a persisted random UUID, not a hash of the MAC address.
- The tray apps embed the delegate instead of talking to a separate daemon.

## Development

```bash
cargo test            # unit tests plus multi-node integration tests
cargo clippy --all-targets -- -D warnings
```

Layout: `crates/core` (library: proxy, delegate table, mesh protocol, daemon
startup, health), `crates/tokenshare` (headless daemon), `crates/tray`
(menubar/taskbar app; a stub on Linux). Design notes are in `design/`;
`design/plan.md` records the decisions and what was built.

## Building for Windows from macOS or Linux

```bash
brew install mingw-w64            # or your distro's mingw-w64 package
rustup target add x86_64-pc-windows-gnu
packaging/windows/build-from-unix.sh
```

This writes `target/x86_64-pc-windows-gnu/tokenshare-windows-x64.zip` containing
`tokenshare-tray.exe`, `tokenshare.exe` and `WebView2Loader.dll` (keep the DLL next
to the tray exe). The binaries have not been run on Windows; CI builds natively on
`windows-latest`.
