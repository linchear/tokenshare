# How it works

This is the long version of the [README](../README.md#how-it-works).

## The delegate table

Every delegate keeps a table of all delegates: id, URL, status, models and
`last_seen`. It is persisted to disk, so a restarted delegate remembers its peers.
There is no coordinator. The *first contact* (`fc`) is just an ordinary delegate
that a newcomer happens to ask first.

- **Joining.** A delegate with `fc = "http://host:11343"` introduces itself to it at
  startup and receives its table. The first contact then tells everyone else
  about the newcomer. On restart a delegate tries the peers in its saved table
  (random order), then `fc`. It marks unreachable ones `inactive`, and if none
  answer it retries after `rejoin_retry_secs` (5 minutes).
- **Keepalives.** Every `keepalive_interval_secs` a delegate sends its entry
  (models, `last_seen`) to one random peer. The receiver merges it and relays it
  to one random other peer, up to `keepalive_ttl` hops, dropping duplicates by
  message ID. A peer that does not answer is marked `unavailable`; it comes back
  only when a keepalive from it arrives.
- **Table sync.** Every `sync_interval_secs` a delegate pulls the full table from
  one random peer and merges it. For each entry the later `last_seen` wins, and a
  delegate's own entry is never overwritten by others. This repairs missed
  announcements.

## Routing

For `/api/chat`, `/api/generate`, `/api/embed`, `/api/embeddings` and
`/v1/{chat/completions,completions,embeddings}`, the delegate reads `model` from
the request body, then:

1. serves it locally if its ollama has the model and a pool slot is free
   (`pool_size`, default 3);
2. otherwise tries online peers that have the model, in random order, moving on if
   a peer is full (503), errors, or is unreachable;
3. otherwise returns `503` with `Retry-After` (or `404` if nobody has the model).

A forwarded request carries `X-TokenShare-Visited`; the receiver serves it itself
or rejects it and never forwards again, so there are no loops.

`POST /api/show` is routed by model the same way but does not use a pool slot.
`GET /api/tags` returns the union of models across the pool. Everything else
(`pull`, `create`, `delete`, ...) goes to this machine's own ollama only. See
[Ollama compatibility](ollama-compatibility.md).

## Streaming

- The request body is buffered (up to 256 MB) so it can be read and replayed to
  the next peer. The response is never buffered: ollama's NDJSON and
  OpenAI-style SSE are passed through as they arrive, whatever the client's
  `stream` flag.
- There is no total timeout, only a connect timeout, because generations can run
  for minutes.
- Retrying on another peer happens only **before the first response byte**. If an
  upstream dies mid-stream, the client connection is broken, so a truncated
  response is never mistaken for a complete one.
- If the client disconnects, the upstream request is dropped and ollama stops
  generating. The pool slot is held until the stream ends or is dropped.

## Stale delegates

- **Our own address.** A delegate never reuses its own address from the saved
  table. It uses `advertise_url` if set, otherwise it learns it again from the
  delegate it joins. A wrong address would otherwise be handed to every peer and
  stay wrong across restarts.
- **Expiry.** A live delegate refreshes its `last_seen` with every keepalive, so
  one not heard from for `peer_expiry_secs` (a day by default) is dead. Each
  delegate drops such entries after a successful table sync and also refuses to
  take them back from others' gossip, so a dead delegate ages out of the whole
  mesh instead of circulating forever. A delegate that was only offline for a
  while returns by rejoining.
- **Forget.** In the tray menu, under *Delegates*, each delegate has *Forget this
  delegate*; headless machines use `tokenshare delegates` to list them and
  `tokenshare forget <id|id-prefix|url>` to forget one (these ask the delegate
  running on this machine, on `--port` or the configured port, and it refuses them
  from anywhere else). It is removed and kept out (the choice is saved in
  `forgotten.json`) until it is heard from again with a newer `last_seen`, so
  forgetting a dead one sticks while forgetting one that is alive only lasts until
  its next keepalive.

## Shutdown

On SIGINT/SIGTERM (or Quit in the tray) a delegate stops gossiping, refuses new
model requests with 503 (peers treat that as "try the next one"), and waits up to
`shutdown_grace_secs` for requests in flight, local or forwarded, to finish.
Anything still streaming after that is cut off with a broken stream.

## Limits worth knowing

- **Full mesh.** Every delegate learns about every other, and a join is announced
  to all of them. This does not scale to large pools; plan for tens of delegates,
  not thousands. Partial gossip views would be the next step.
- **Clocks.** "Later `last_seen` wins" compares timestamps from different
  machines, so keep clocks roughly in sync (NTP).
- **`last_seen` is not "seen by me".** A relayed keepalive refreshes an entry with
  the origin's timestamp, so an entry can be marked online on the word of a third
  party until a direct request fails.
- **Load balancing is random**, not load-aware. CPU/GPU/RAM metrics are not
  collected.

## Differences from the original design

The first sketch is in [`design/design.md`](../design/design.md), and
[`design/plan.md`](../design/plan.md) records the decisions made while building it.
Where the code differs:

- "All full" returns **503** with `Retry-After`, not 500.
- The delegate ID is a persisted random UUID, not a hash of the MAC address.
- The tray apps embed the delegate instead of talking to a separate daemon.
