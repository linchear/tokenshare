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
| `masquerade` | `false` | Also listen on `ollama_port` for ollama clients; `port` then serves other delegates only (see below) |
| `ollama_port` | `11434` | The port ollama clients use while masquerading. 11434 is ollama's own, so existing apps need no change. Shown under *Advanced* in Settings |
| `fc` | none | First-contact delegate URL |
| `advertise_url` | learned | How peers reach this delegate (learned from the first delegate it joins) |
| `ollama_hosts` | `["http://localhost:11434"]` | Ollama instances reachable from this machine. Optional: `[]` makes a client-only delegate |
| `pool_size` | `3` | Max concurrent requests to ollama |
| `share_with_peers` | `true` | Let other delegates use this machine's ollama (see below). |
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
(repeatable), `--client-only`, `--masquerade`, `--pool-size`. Set `RUST_LOG` to change the log level. Logs list
metrics only (method, path, model, target, status, time to first byte), never
request content. On Linux, `packaging/linux/tokenshare.service` is a systemd
user unit.

macOS menubar app:

```bash
packaging/macos/build-app.sh   # builds target/release/bundle/Token Share.app
```

The tray apps run the delegate themselves; do not also run the daemon on the
same port. The icon is a round badge with a symbol, so state does not depend on color
alone: a green **check** (✅) when a model is available and the delegate is idle, a
blue **lightning bolt** (⚡) while any request is in flight (served here or
forwarded), a yellow **exclamation mark** (⚠️) when no model is available anywhere
but other delegates are online (it turns green by itself when some ollama comes
back), and a gray **unplugged** icon (🔌) when no model is available and no other delegate
is online, or the delegate failed to start (the menu says "Unavailable"). "Available" means a model listed by
this machine's ollama or by an online peer; a reachable ollama with nothing
installed does not count. The menu
shows state, pool usage and the delegate list. Quit waits up to 5 seconds for
in-flight requests.

**Masquerade.** With `masquerade = true` (the *Also listen on ollama's port*
checkbox, or `--masquerade`) the delegate listens on **two** ports:

- **`ollama_port`** (11434 by default, ollama's own; `--ollama-port`, or *Ollama port*
  under *Advanced* in Settings), with the full behavior: apps already pointed
  at ollama use the local ollama and the pool with no changes.
- **`port`** (11343 by default), the tokenshare port, which is how other
  delegates reach it. Requests that other delegates forward to it are served
  from your ollama hosts (when *Share my ollama with other delegates* is on, the
  default), so a masquerading machine, and any other ollama boxes on your
  network that you list as hosts, can still lend their models to the pool.
  Apps that connect to this port directly are not served from your hosts: it
  acts as a delegate with **no models** for them, forwarding to other delegates
  like a client-only delegate. Your own apps use the ollama port.

Turn sharing off to keep the hosts private: the machine still joins and
gossips, but advertises no models and refuses forwarded requests. In both
cases the delegate advertises itself on the tokenshare port even if another
delegate joined it through the ollama port.

11434 is also the port a local ollama uses, so you cannot run both on it. Either
start ollama on another port (for example `OLLAMA_HOST=127.0.0.1:11435`) and
list that address as the ollama host, or leave ollama alone and set a different
`ollama_port` for the delegate (apps then need that port instead of 11434). The delegate refuses to start if an ollama host
points back at itself (on either of its ports). If a port is already taken,
Settings warns before saving (you can *Save Anyway*), and starting fails with
"port 11434 is already in use" (naming whichever port it is). The check also catches ollama listening on
127.0.0.1 only, which on macOS would otherwise quietly shadow the delegate for
local clients. The menu shows "(masquerading)".

**What ollama clients see.** On either port the delegate answers `GET`/`HEAD /`
("Ollama is running") itself, and `/api/version` too: on the mesh port always, on
the ollama port when there is no local ollama (a fixed `0.12.0`; with one, its own
answer is passed through). Clients check these first, and the `ollama` CLI launches Ollama.app if they
fail. `/api/tags` (what `ollama ls` calls) lists this machine's models as ollama
reports them, plus peers' models with the fields filled in as far as we know them:
the digest is a stand-in hash of the name, the size is 0 and the time is when the
peer was last heard from. `ollama ls` needs a digest on every entry and crashes
without one.

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

## Packaging

Both scripts cross-compile from macOS (or Linux) and need the toolchains noted
at the top of each.

```bash
packaging/windows/build-zip.sh   # target/x86_64-pc-windows-gnu/tokenshare-windows-x64.zip
packaging/linux/build-deb.sh     # target/deb/tokenshare_<version>_amd64.deb
```

- **Windows** (`brew install mingw-w64`): the zip holds `tokenshare-tray.exe`,
  `tokenshare.exe` and `WebView2Loader.dll`, which must stay next to the tray
  exe. Unzip anywhere and run the tray exe.
- **Linux** (`brew install zig dpkg`, `cargo install cargo-zigbuild`): the
  `.deb` installs `/usr/bin/tokenshare`, its man page (`man tokenshare`), a
  systemd *user* unit, and an example config under `/usr/share/doc/tokenshare/`. It is built against glibc 2.17 and
  needs only libc. After `sudo apt install ./tokenshare_*_amd64.deb`, run
  `systemctl --user enable --now tokenshare`; configuration is read from
  `~/.config/tokenshare/config.toml`. To keep it running without a login
  session, `loginctl enable-linger $USER`. Full guide: [docs/linux.md](docs/linux.md).

The cross-built binaries have not been run on Windows or Linux by the author of
this note; CI builds natively on both.
