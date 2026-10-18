# Token Share on Linux

On Linux, Token Share is a headless daemon (`tokenshare`) with no UI. This
guide covers installing it, configuring it, joining other machines and checking
that it works. For how it works in general, see the main [README](../README.md).

## What you need

- 64-bit Linux (amd64) with glibc 2.17 or newer: Debian 8+, Ubuntu 14.04+,
  RHEL/CentOS 7+ and anything more recent.
- [ollama](https://ollama.com) running on this machine (optional; see
  [Client-only](#client-only-no-local-ollama)).
- Other machines running Token Share on the same trusted network (LAN or a VPN
  such as Tailscale). There is **no authentication or encryption**, so never
  expose the port to the internet.

## Install

From the `.deb` (Debian, Ubuntu and derivatives):

```bash
sudo apt install ./tokenshare_0.1.0_amd64.deb
```

This installs:

| Path | What |
| --- | --- |
| `/usr/bin/tokenshare` | the daemon |
| `/usr/lib/systemd/user/tokenshare.service` | a systemd **user** service |
| `/usr/share/man/man1/tokenshare.1.gz` | the manual page (`man tokenshare`) |
| `/usr/share/doc/tokenshare/` | this README and `config.example.toml` |

On other distributions, copy the `tokenshare` binary somewhere on your `PATH`
(for example `~/.local/bin`) and install the service file from
`packaging/linux/tokenshare.service`, changing `ExecStart` to match.

Remove it with `sudo apt remove tokenshare`. Your config and data under your home
directory are left alone.

## Run it

### As a systemd user service (recommended)

```bash
systemctl --user enable --now tokenshare     # start now and at every login
systemctl --user status tokenshare
systemctl --user restart tokenshare          # after changing the config
systemctl --user stop tokenshare
```

A user service stops when you log out. To keep it running without a login
session (a server, for example):

```bash
loginctl enable-linger "$USER"
```

Logs go to the journal:

```bash
journalctl --user -u tokenshare -f
```

Set `RUST_LOG=debug` for more detail, for example with
`systemctl --user edit tokenshare` and:

```ini
[Service]
Environment=RUST_LOG=debug
```

### In a terminal

```bash
tokenshare                                   # uses ~/.config/tokenshare/config.toml
tokenshare --fc http://192.168.2.3:11343     # join another delegate
tokenshare --help
```

`Ctrl-C` or `SIGTERM` shuts down gracefully (see [Stopping](#stopping)).

## Configure

Create `~/.config/tokenshare/config.toml`. Every key is optional. Start from the
example:

```bash
mkdir -p ~/.config/tokenshare
cp /usr/share/doc/tokenshare/config.example.toml ~/.config/tokenshare/config.toml
```

The keys you will most likely want:

```toml
# Another delegate to join through at startup (any delegate will do).
fc = "http://192.168.2.3:11343"

# Ollama instances reachable from this machine.
ollama_hosts = ["http://localhost:11434"]

# Max concurrent requests sent to ollama.
pool_size = 3

# Port for ollama and coordination traffic.
port = 11343
```

Restart the service after editing. `man tokenshare` documents every option and
config key. Command-line flags override the file:

| Flag | Config key | Meaning |
| --- | --- | --- |
| `--config FILE` | | use a different config file |
| `--port N` | `port` | listen port (default 11343) |
| `--fc URL` | `fc` | first-contact delegate |
| `--advertise-url URL` | `advertise_url` | how peers reach this machine |
| `--ollama-host URL` | `ollama_hosts` | ollama host (repeat for several) |
| `--pool-size N` | `pool_size` | concurrent ollama connections |
| `--masquerade` | `masquerade` | also listen on the ollama port, for ollama clients |
| `--ollama-port N` | `ollama_port` | that port (default 11434, ollama's own) |
| `--client-only` | `ollama_hosts = []` | no local ollama |

Other options (`share_with_peers`, keepalive and sync intervals, the shutdown
grace period, and so on) are documented in the README's configuration table.

### Where things are stored

| Path | What |
| --- | --- |
| `~/.config/tokenshare/config.toml` | configuration |
| `~/.local/share/tokenshare/id` | this delegate's random ID (do not copy it to another machine) |
| `~/.local/share/tokenshare/table.json` | the saved list of other delegates |

## Join other machines

1. Install and start Token Share on each machine.
2. Pick any one machine as the **first contact**. It is an ordinary delegate and
   needs no special setup.
3. On every other machine, set `fc = "http://<first-contact-address>:11343"`
   and restart. When a machine starts it introduces itself to the first
   contact, receives the list of delegates, and the first contact announces it to
   everyone else.
4. After the first join, each machine saves the delegate list and rejoins through
   it on restart, so `fc` only matters for the first start.

Open the port between the machines if you run a firewall:

```bash
sudo ufw allow from 192.168.2.0/24 to any port 11343 proto tcp     # ufw
sudo firewall-cmd --add-port=11343/tcp --permanent && sudo firewall-cmd --reload   # firewalld
```

Restrict it to your own network; the service has no authentication.

If a machine is reachable by several addresses (multiple interfaces, a VPN, NAT),
set `advertise_url` to the one other delegates should use, for example
`advertise_url = "http://100.64.0.5:11343"`. Otherwise it is detected from how
the first contact was reached.

## Use it

Point your ollama client at the delegate instead of at ollama:

```bash
export OLLAMA_HOST=http://localhost:11343
ollama list                                  # models across the whole pool
ollama run llama3.2 "hello"                  # served locally or by a peer

curl http://localhost:11343/api/chat -d '{
  "model": "llama3.2",
  "messages": [{"role": "user", "content": "hello"}]
}'
```

A request is served by this machine's ollama when it has the model and a free
slot, and otherwise streamed from a peer that has it. If every candidate is busy
you get `503` with `Retry-After`; if nobody has the model you get `404`.

### Let existing ollama clients work unchanged

With `masquerade = true` (or `--masquerade`) the delegate listens on **two**
ports: the **ollama port** (`ollama_port`, 11434 by default, which is ollama's
own), so apps already pointed at `localhost:11434` use the pool with no changes;
and the tokenshare port
(`port`, 11343 by default), which is how other delegates reach it. Requests that
other delegates forward to the tokenshare port are served from your ollama hosts
while sharing is on (the default), so a masquerading machine, and any other
ollama boxes you list as hosts, still lend their models to the pool. Apps that
connect to the tokenshare port directly are not served from your hosts; for
them it acts as a delegate with **no models** and forwards to other delegates.
Use the ollama port for your own apps. Open 11343 in the firewall for other machines as usual; the ollama port only
needs to be reachable by your own clients.

If you would rather leave a local ollama on 11434, give the delegate another
port instead (`ollama_port = 12434`, or `--ollama-port 12434`) and point your
apps at that port.

That port is normally taken by ollama itself, so move ollama to
another port first. For the packaged ollama service:

```bash
sudo systemctl edit ollama
```

```ini
[Service]
Environment="OLLAMA_HOST=127.0.0.1:11435"
```

```bash
sudo systemctl restart ollama
```

Then in `config.toml`:

```toml
masquerade = true
ollama_hosts = ["http://127.0.0.1:11435"]
```

The delegate refuses to start if an ollama host points back at itself (on either
of its ports), and says so if either port is already in use.

### Client-only (no local ollama)

For a machine with no GPU or no ollama, use only other machines' models:

```toml
ollama_hosts = []
```

or `tokenshare --client-only`.

### Keep your ollama to yourself

Set `share_with_peers = false` so other machines no longer see your models or
send you requests. You still use your own ollama and the rest of the pool.
This works the same while masquerading.

## Check that it works

```bash
curl -s http://localhost:11343/_ts/status | python3 -m json.tool
```

The `state` field says what the delegate sees:

| `state` | Meaning |
| --- | --- |
| `active` | a model is available (here or on an online peer) and it is idle |
| `in-use` | requests are in flight, served here or forwarded |
| `no-models` | no model is available anywhere, but other delegates are online |
| `down` | no model is available and no other delegate is online |

Other useful fields: `ports` (every port it listens on), `sharing` (whether
other delegates may use this machine's ollama), `pool` (slots in use and forwarded requests), `ollama`
(this machine's ollama hosts and their models) and `delegates` (every known
delegate with its URL, status, models and `last_seen`). `draining` is true during
shutdown.

```bash
curl -s http://localhost:11343/api/tags      # models across the pool
```

## Stopping

`systemctl --user stop tokenshare` (or `SIGTERM`/`Ctrl-C`) stops gossiping,
refuses new model requests with `503` so peers pick another delegate, and gives
requests already running up to `shutdown_grace_secs` (default 30) to finish.
Anything still streaming after that is cut off with a broken stream, never a
truncated "complete" one.

## Troubleshooting

| Symptom | Likely cause and fix |
| --- | --- |
| `binding port 11343` / `already in use` on start | another process has the port. Change `port`, or stop the other process. With `masquerade`, port 11434 is ollama's: move ollama as shown above. |
| `no first-contact or known delegates; waiting to be joined` in the log | normal for the first machine. Others will join you, or set `fc`. |
| `cannot reach delegate ... will retry` | the first contact is down, the address is wrong, or a firewall blocks 11343. It retries every 5 minutes (`rejoin_retry_secs`). Check with `curl http://<fc>:11343/_ts/status`. |
| `state` is `no-models` | other delegates are up but none has a model. Start ollama or pull a model somewhere. |
| `state` is `down` | no model here and no other delegate online. Check ollama (`curl localhost:11434/api/tags`) and the network. |
| `404 model 'x' not found` | no online delegate lists that model. `ollama pull x` on one of them; it appears in the pool within about a minute. |
| `503 ... busy` | every machine that has the model is at its `pool_size`. Retry, or raise `pool_size`. |
| a peer is listed but never used | check its `status` in `/_ts/status`: `unavailable` or `inactive` peers are skipped until they answer or send a keepalive. Make sure its URL is reachable from here, and set `advertise_url` there if not. |
| other machines get `localhost` URLs for this one | the first contact was reached as `localhost`. Set `advertise_url` on it to its real address. |
| clocks | "latest `last_seen` wins", so keep clocks roughly in sync (`timedatectl set-ntp true`). |

## Build from source

Needs a Rust toolchain.

```bash
cargo build --release -p tokenshare          # target/release/tokenshare
packaging/linux/build-deb.sh                 # cross-build the amd64 .deb (needs zig, cargo-zigbuild, dpkg)
```
