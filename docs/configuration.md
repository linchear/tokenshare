# Configuration

A TOML file; every key is optional. Command-line flags override it.

```toml
fc = "http://192.168.2.3:11343"
ollama_hosts = ["http://localhost:11434"]
pool_size = 3
```

| Key | Default | Meaning |
| --- | --- | --- |
| `port` | `11343` | Port for ollama and coordination traffic |
| `masquerade` | `false` | Also listen on `ollama_port` for ollama clients; `port` then serves other delegates only ([see below](#masquerade)) |
| `ollama_port` | `11434` | The port ollama clients use while masquerading. 11434 is ollama's own, so existing apps need no change. Shown under *Advanced* in Settings |
| `fc` | none | First-contact delegate URL |
| `advertise_url` | learned | How peers reach this delegate (learned from the first delegate it joins) |
| `ollama_hosts` | `["http://localhost:11434"]` | Ollama instances reachable from this machine. Optional: `[]` makes a [client-only](#client-only) delegate |
| `pool_size` | `3` | Max concurrent requests to ollama |
| `share_with_peers` | `true` | Let other delegates use this machine's ollama ([see below](#not-sharing)) |
| `poll_interval_secs` | `10` | How often to refresh local models |
| `rejoin_retry_secs` | `300` | Wait before retrying when no delegate answered |
| `keepalive_interval_secs` | `30` | Keepalive period |
| `keepalive_ttl` | `3` | Extra relay hops per keepalive |
| `sync_interval_secs` | `60` | Full table pull period |
| `shutdown_grace_secs` | `30` | How long to let in-flight requests finish on shutdown |
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
  forget      Forget a delegate, for example one that is gone for good

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
```

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
usage and the delegate list. Quit waits up to 5 seconds for in-flight requests.

**Settings.** Choose *Settings…* in the tray menu to edit the first contact, ollama
hosts (one per line), whether to share your ollama, connections to ollama, listen
port and, under Advanced, the advertise URL. *Save & Restart* validates the values,
writes the config file and restarts the delegate with them; requests in flight get a
few seconds to finish. Other options (intervals, TTL, grace period) are kept as they
are in the file, so edit it directly for those. On macOS the app shows a Dock icon
while the window is open. The window needs WebView2 on Windows (included in Windows
11 and current Windows 10).

`GET /_ts/status` returns the same picture as JSON: `state`, `draining`, `pool`,
local ollama hosts and the delegate table.
