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
- **Forget from the dashboard.** Opened on the delegate's own machine, the
  dashboard has a *Forget* button for each peer.

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
