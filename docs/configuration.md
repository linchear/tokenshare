# Configuration

A TOML file; every key is optional. Command-line flags override it.

```toml
fc = "http://192.168.2.3:11343"
ollama_hosts = ["http://localhost:11434"]
pool_size = 3
```

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | hostname | How this delegate is shown to the pool: dashboards, tray menus, `tokenshare delegates`. Up to 40 characters ([see below](#names)) |
| `port` | `11343` | Port for ollama and coordination traffic |
| `masquerade` | `false` | Also listen on `ollama_port` for ollama clients; `port` then serves other delegates only ([see below](#masquerade)) |
| `ollama_port` | `11434` | The port ollama clients use while masquerading. 11434 is ollama's own, so existing apps need no change. Shown under *Advanced* in Settings |
| `fc` | none | First-contact delegate URL |
| `advertise_url` | learned | How peers reach this delegate (learned from the first delegate it joins) |
| `ollama_hosts` | `["http://localhost:11434"]` | Ollama instances reachable from this machine. Optional: `[]` makes a [client-only](#client-only) delegate |
| `pool_size` | `3` | Max concurrent requests to ollama |
| `share_with_peers` | `true` | Let other delegates use this machine's ollama ([see below](#not-sharing)) |
| `max_peer_requests` | no limit | The most requests from other delegates served at once, out of `pool_size` ([see below](#lending-limits)) |
| `shared_models` | all | The models other delegates may use, e.g. `["qwen3", "llama3.2:1b"]` ([see below](#lending-limits)) |
| `pool_secret` | none | Shared secret every delegate in the pool must use ([see below](#pool-secret)). The `TOKENSHARE_POOL_SECRET` environment variable overrides it |
| `trust_localhost` | `true` | With a `pool_secret`, let connections from this machine in without it |
| `notifications` | `true` | Tray app only: desktop notifications ([see below](#notifications)) |
| `check_for_updates` | `true` | macOS app only: look for a new version once a day ([updates](packaging.md#updates)) |
| `poll_interval_secs` | `10` | How often to refresh local models |
| `rejoin_retry_secs` | `300` | Wait before retrying when no delegate answered |
| `keepalive_interval_secs` | `30` | Keepalive period |
| `keepalive_ttl` | `3` | Extra relay hops per keepalive |
| `sync_interval_secs` | `60` | Full table pull period |
| `shutdown_grace_secs` | `30` | How long to let in-flight requests finish on shutdown |
| `queue_timeout_secs` | `30` | When every slot that could serve a request is busy, how long it waits for one before getting `503`; `0` answers `503` at once ([how it works](how-it-works.md#routing)) |
| `peer_expiry_secs` | `86400` | Drop delegates not heard from for this long; `0` keeps them forever |

## Where files live

Via the `directories` crate. The delegate ID and table live in the data dir, which
is also where the tray app writes `tokenshare.log`.

| | Config file | Data dir |
| --- | --- | --- |
| macOS | `~/Library/Application Support/tokenshare/config.toml` | `~/Library/Application Support/tokenshare/` |
| Linux | `~/.config/tokenshare/config.toml` | `~/.local/share/tokenshare/` |
| Windows | `%APPDATA%\tokenshare\config\config.toml` | `%APPDATA%\tokenshare\data\` |

## Command line

```
tokenshare [OPTIONS] [COMMAND]

Commands:
  delegates   List the delegates this machine knows about
  forget      Forget a delegate (by name, id, id prefix or URL), for example one that is gone for good

Options:
  --config <CONFIG>            Config file (TOML)
  --port <PORT>                Listen port (default 11343)
  --fc <FC>                    First-contact delegate URL
  --advertise-url <URL>        URL peers use to reach this delegate
  --ollama-host <HOST>         Ollama host; repeat for several
  --masquerade                 Also listen on the ollama port so ollama clients work unchanged
  --ollama-port <PORT>         Port for ollama clients when masquerading (default 11434)
  --client-only                Use no local ollama; only borrow other delegates'
  --pool-size <N>              Max concurrent ollama connections (default 3)
  --name <NAME>                How this machine is shown to the pool (default: its hostname)
  --max-peer-requests <N>      The most requests from other delegates served at once
  --share-model <MODEL>        A model other delegates may use; repeat for several
```

The pool secret is read from the config file or the `TOKENSHARE_POOL_SECRET`
environment variable, not from a flag, so it does not show up in process listings.

Set `RUST_LOG` to change the log level. Logs list metrics only, never request
content; see [Logging](logging.md).

## Masquerade

With `masquerade = true` (the *Also listen on ollama's port* checkbox, or
`--masquerade`) the delegate listens on **two** ports:

- **`ollama_port`** (11434 by default, ollama's own; `--ollama-port`, or *Ollama
  port* under *Advanced* in Settings), with the full behavior: apps already pointed
  at ollama use the local ollama and the pool with no changes.
- **`port`** (11343 by default), the tokenshare port, which is how other delegates
  reach it. Requests that other delegates forward to it are served from your
  ollama hosts (when *Share my ollama with other delegates* is on, the default), so
  a masquerading machine, and any other ollama boxes on your network that you list
  as hosts, can still lend their models to the pool. Apps that connect to this port
  directly are not served from your hosts: it acts as a delegate with **no models**
  for them, forwarding to other delegates like a client-only delegate. Your own
  apps use the ollama port.

Turn sharing off to keep the hosts private: the machine still joins and gossips,
but advertises no models and refuses forwarded requests. In both cases the delegate
advertises itself on the tokenshare port even if another delegate joined it through
the ollama port.

11434 is also the port a local ollama uses, so you cannot run both on it. Either
start ollama on another port (for example `OLLAMA_HOST=127.0.0.1:11435`) and list
that address as the ollama host, or leave ollama alone and set a different
`ollama_port` for the delegate (apps then need that port instead of 11434). The
delegate refuses to start if an ollama host points back at itself (on either of its
ports). If a port is already taken, Settings warns before saving (you can *Save
Anyway*), and starting fails with "port 11434 is already in use" (naming whichever
port it is). The check also catches ollama listening on 127.0.0.1 only, which on
macOS would otherwise quietly shadow the delegate for local clients. The menu shows
"(masquerading)".

## Client only

Ollama hosts are optional. With none (`ollama_hosts = []`, the `--client-only` flag,
or an empty field in Settings) the delegate serves nothing itself and forwards every
model request to a delegate that has the model, so machines without a GPU can just
consume the pool. `/api/tags` still lists the pool's models; endpoints that only
make sense locally (`pull`, ...) return 503. It reads as **unavailable** until it has
at least one online peer, and the menu shows "(client only)".

## Not sharing

With `share_with_peers = false` (the *Share my ollama with other delegates*
checkbox in Settings) this delegate looks like its ollama is down to everyone else:
it advertises no models, so peers do not choose it, and it refuses any forwarded
request with 503 in case a peer's view is stale. Its own users are unaffected:
requests to it are still served by its local ollama and, failing that, by other
delegates. The menu shows "(not sharing)" and `/_ts/status` reports
`"sharing": false`.

## Lending limits

Sharing does not have to be all or nothing. Two settings limit what other
delegates get from this machine, while its own users keep everything:

```toml
pool_size = 4
max_peer_requests = 1            # peers get at most 1 of the 4 connections
shared_models = ["qwen3", "llama3.2:1b"]
```

- **`max_peer_requests`** caps how many requests from other delegates run at
  once. The rest of `pool_size` stays free for this machine's own users. Peers are
  told how many slots they may use, so they count this delegate as full once their
  share is taken (or the pool is full) and try someone else first; one that asks
  anyway gets a 503 and moves on. It must be between 1 and `pool_size`; to lend
  nothing, turn sharing off instead.
- **`shared_models`** lists the models peers may use. A name without a tag, such
  as `qwen3`, covers every tag (`qwen3:8b`, `qwen3:32b`); with a tag, only that
  one. Other models are not advertised, so they are not in the pool for anyone
  else, and a forwarded request for one is refused with 503. Empty (the default)
  shares every model.

In Settings these are *At most, requests from others* and *Models others may
use*, under the sharing checkbox. The dashboard shows the limit next to this
delegate's slots, and how many local models are not shared.

## Names

Every delegate has a name for people to read: `name` if set, otherwise the
machine's hostname without its domain (`gpu-box`, not `gpu-box.local`). It
travels with joins and keepalives, so a rename reaches the pool within a
keepalive interval. Names are shown in the dashboard, the tray menu and
`tokenshare delegates`, and `tokenshare forget` accepts one.

Names are only labels. They need not be unique (the id is what identifies a
delegate, and an ambiguous name is refused by `forget`), and they play no part in
routing. A name received from another delegate has control characters removed and
is cut to 40 characters, so it is safe to show in a terminal or a menu. Delegates
older than names show their address instead.

## Pool secret

By default anyone who can reach a delegate's port can use the pool and join it.
Set the same `pool_secret` on every delegate (or *Pool secret* in Settings) to
require it:

```toml
pool_secret = "correct-horse-battery-staple"
```

- **Delegates** send it to each other (`Authorization: Bearer <secret>`) on every
  join, keepalive, table pull and forwarded request. A delegate without it, or
  with a different one, cannot join, and a peer that refuses it is skipped like a
  busy one. The log says `refused us: it needs the pool secret, or ours does not
  match`.
- **Apps on this machine** need no change: connections from this machine are
  trusted (`trust_localhost`). This covers apps pointed at the delegate or its
  masquerade port, the `tokenshare` commands and the tray's dashboard.
- **Apps on other machines** must send the secret as a bearer token. For
  OpenAI-style clients that is the API key (`OPENAI_API_KEY=<secret>`, or the
  "API key" field). The ollama CLI cannot send one; run a delegate on that
  machine instead, which is the usual setup anyway.
- **Open without it:** `GET /` and `/api/version` (health checks) and the
  dashboard page. The dashboard asks for the secret when its data is refused,
  and keeps it for that browser tab only.
- **Ollama never sees it:** the delegate removes it before passing a request on.

Requests refused for the secret are answered `401`, logged with the caller's
address, and counted as `unauthorized` in the dashboard's *Turned away* tile and
in `tokenshare_requests_rejected_total`.

The secret must be at least 8 characters of letters, digits and punctuation (no
spaces). When it is set, the config file is saved readable by its owner only on
macOS and Linux. Changing it means changing it on every delegate; until they all
match, the pool splits into the delegates that share each secret.

Turn `trust_localhost` off when something on this machine relays traffic from
elsewhere (an SSH tunnel, a reverse proxy, a container's port forward); such
connections look local too. The `tokenshare` commands then send the configured
secret themselves.

The secret is a password, not encryption. Traffic between delegates is plain
HTTP, so anyone who can watch the network can read it and the prompts. Use it on a
network you trust or over a VPN such as WireGuard or Tailscale, and never expose
the port to the internet.

## The tray app

The macOS menubar app and the Windows taskbar app run the delegate themselves; do
not also run the daemon on the same port.

The icon is a round badge with a symbol, so state does not depend on color alone:

| Icon | Meaning |
| --- | --- |
| green **check** ✅ | a model is available and the delegate is idle |
| blue **lightning bolt** ⚡ | a request is in flight (served here or forwarded) |
| yellow **exclamation mark** ⚠️ | no model is available anywhere, but other delegates are online; turns green by itself when some ollama comes back |
| gray **unplugged** 🔌 | no model is available and no other delegate is online, or the delegate failed to start (the menu says "Unavailable") |

"Available" means a model listed by this machine's ollama or by an online peer; a
reachable ollama with nothing installed does not count. The menu shows state, pool
usage and the delegate list. Quit waits up to 5 seconds for in-flight requests. On
macOS, **Check for Updates…** looks for a new version and, when there is one,
becomes **Install Token Share *x.y.z* and Restart** ([updates](packaging.md#updates)).

**Settings.** Choose *Settings…* in the tray menu to edit the name, the first
contact, ollama hosts (one per line), whether to share your ollama and its
[lending limits](#lending-limits), the pool secret, connections to ollama, listen
port, notifications, update checks and, under Advanced, the advertise URL and ollama port. *Save & Restart* validates the values,
writes the config file and restarts the delegate with them; requests in flight get a
few seconds to finish. Other options (intervals, TTL, grace period) are kept as they
are in the file, so edit it directly for those. On macOS the app shows a Dock icon
while the window is open. The window needs WebView2 on Windows (included in Windows
11 and current Windows 10).

### Notifications

The tray app shows a desktop notification when:

- a delegate it has seen online since starting **goes offline** and stays offline
  for a minute (a missed keepalive alone is not reported), and again when it
  **comes back**;
- a new delegate **joins** the pool;
- a peer **refuses this machine's pool secret** (it answered 401; requests are not
  sent there until it accepts it again);
- the delegate **cannot start**, for example because its port is in use.

Delegates coming and going are not reported for the first minute after the
delegate starts, while it rejoins the pool. Several at once are grouped into one
notification. Turn them off with *Notify me…* in Settings or `notifications =
false`. On macOS they appear as Token Share's only when run from the app bundle,
and the first one may ask for permission.

`GET /_ts/status` returns the same picture as JSON: `name`, `state`, `draining`,
`pool` (including `peer_limit` and `peer_in_use`), `shared_models`, local ollama
hosts and the delegate table.
