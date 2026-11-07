# Changelog

Notable changes to Token Share. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Delegates of different versions can share a pool: gossip fields are only ever
added, and older delegates ignore the ones they do not know. Anything that breaks
this is called out below.

## [Unreleased]

### Added

- **Load-aware routing.** Delegates publish their free slots and the models loaded
  in memory with their keepalives. Requests go to a peer that already has the
  model loaded and has a free slot, spread across equal peers, and skip for a few
  seconds a peer that just turned one away.
  ([#1](https://github.com/linchear/tokenshare/pull/1))
- **Dashboard** at `/_ts/dashboard`, and **Open Dashboard** in the tray menu: the
  delegates with their slots and warm models, the local ollama hosts, and recent
  responses with how they ended. `/_ts/status` includes the same activity.
  ([#1](https://github.com/linchear/tokenshare/pull/1))
- **Prometheus metrics** at `/_ts/metrics`: state, pool and peer gauges, response
  counts by result, time-to-first-byte and duration histograms, and token counts.
  ([#2](https://github.com/linchear/tokenshare/pull/2))
- **Optional pool secret** (`pool_secret`, `TOKENSHARE_POOL_SECRET`, or *Pool
  secret* in Settings). Delegates and clients on other machines must send it as a
  bearer token; apps on the same machine need no change (`trust_localhost`). The
  dashboard asks for it when opened from another machine.
  ([#3](https://github.com/linchear/tokenshare/pull/3))
- **Lending limits** for machines that share their ollama: `max_peer_requests`
  keeps part of `pool_size` free for the machine's own users, and
  `shared_models` offers peers only some models. Both are in Settings and on the
  command line (`--max-peer-requests`, `--share-model`).
- **Delegate names.** Each delegate has a name (`name`, `--name`, or the hostname
  by default) that travels with its gossip and is shown in the dashboard, the
  tray menu, `tokenshare delegates` and metrics. `tokenshare forget` accepts one.
- **Real model info for peers' models.** Delegates publish each shared model's
  size, digest and details (family, parameter size, quantization). `ollama ls`
  and model pickers show peers' models like local ones instead of with size
  `0 B` and a stand-in ID, and the dashboard marks a model that delegates have in
  different builds (`differing_models` in `/_ts/status`).
- **Forget from the dashboard.** Opened on the delegate's own machine, the
  dashboard has a *Forget* button for each peer.
- **`X-Tokenshare-Served-By`** on every response an ollama produced: the name of
  the delegate that ran the request.
- **Tray notifications** when a delegate goes offline (after a minute), comes
  back or joins, when a peer refuses this machine's pool secret, and when the
  delegate cannot start. `notifications = false` or Settings turns them off. The
  dashboard tags a peer that refuses the secret.
- **Busy requests wait.** When every slot that could serve a request is busy,
  here and on peers, the request waits for one for up to `queue_timeout_secs`
  (30 s) instead of getting `503` at once. The dashboard, `/_ts/status`
  (`pool.waiting`) and metrics (`tokenshare_requests_waiting`) show how many are
  waiting.
- **The macOS app updates itself.** It looks for a new release once a day and
  from **Check for Updates…** in its menu, and installs one when you choose
  **Install … and Restart**. Updates are signed, and the app refuses one that is
  not signed with the project's key. `check_for_updates = false` or Settings turns
  the daily check off. Releases attach `latest.json` and a signed
  `.app.tar.gz` for it.
- **Versions in gossip.** Delegates publish the Token Share and ollama versions
  they run. The dashboard shows them and marks a delegate behind the rest of the
  pool; `tokenshare delegates` has a VERSION column, and `tokenshare_info` and
  `tokenshare_peer_info` carry them.
- **Lent and borrowed tokens.** Each delegate tallies, per peer, the requests and
  tokens it served for the peer and the peer served for it, saved across restarts
  in `ledger.json`. Shown on the dashboard, in `/_ts/status` (`ledger`) and as
  `tokenshare_lent_*` and `tokenshare_borrowed_*` metrics.
- **`prefer_warm_peers`** (off by default, under Advanced in Settings): a request
  for a model that is not loaded here goes to a peer that has it loaded and has
  room, instead of waiting for it to load here.
- **Load a model ahead of time from the dashboard.** On the delegate's own
  machine, clicking a model that is not loaded loads it on that machine or peer,
  kept for 30 minutes.
- **Notifications toggle in the tray menu**, saved at once without a restart.

### Changed

- `/_ts/forget` refuses requests from web pages the delegate did not serve
  (checked with the `Origin` header), so another site open in a browser on the
  same machine cannot use it.
- The tray menu says why the delegate could not start (for example
  "Cannot start: port 11343 is already in use"), and **Open Dashboard** is greyed
  out until it is running.

## [0.1.0] - 2026-10-29

First release: a delegate per machine that pools ollama instances over a
peer-to-peer mesh, with the ollama and OpenAI-compatible APIs, masquerade on
ollama's port, client-only and not-sharing modes, the `delegates` and `forget`
commands, and tray apps for macOS and Windows.

[Unreleased]: https://github.com/linchear/tokenshare/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/linchear/tokenshare/releases/tag/v0.1.0
