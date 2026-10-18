
# Token Share

## Overview

A program to route ollama calls amongst several hosts (peer-to-peer)

It will sit and listen on a configurable port (default 11343) and route to a list of ollama end points.


### Terms
- delegate - the program
- FC - first-contact, the machine that the delegate reaches out to upon startup
- delegate table - a list of URLs pointing to delegates with the status and models its capable of

The program will contain a list of endpoints and their status
eg.

id:
a hash of the mac address of the programs interface that accepts traffic

ollama server list:

| host            | models  |
| --------------- | ------- |
| localhost:11434 | -gemma2 |

a delegate table:



| id  | delegate                 | load | status | type     | models     | vram | ram | last_seen |
| --- | ------------------------ | ---- | ------ | -------- | ---------- | ---- | --- | --------- |
| 12b | http://192.168.2.3:11343 | 0%   | online | delegate | - gemma2   |      |     |           |
| 34a | http://192.168.1.3:11343 | 0%   | online | delegate | - llama3.2 |      |     |           |

### First Contact
upon start, the user must set a "first-contact" entry.

```
fc=http://192.168.2.3:11343
```

When the delegate signs on the first time, it will reach out to the first-contact host and request the delegate table from the fc delegate.

The protocol on first start will be:
- identify self to fc
- fc will add the new delegate to its delegate table, 
	- fc will hash the delegate table (h)
- fc will send its delegate table to new delegate

the `fc` machine will then reach out to each of the delegates in its delegate table to announce the new member:
`new-member` , `id` , `<delegate info>`, `delegate-table-hash`, num entries

### Announce
- When a delegate gets a message from another delegate that a new member has joined, 
- it will add that delegate to its delegate table.

### Ollama Pool
- each delegate will have a configurable number of connections it can send to ollama at any given time. Default is 3.

### Ollama Request
- When a delegate gets an ollama request, it will first check its ollama table to see if it has an entry. 
- If so, it will send forward that request to ollama along and return the result back to the client. This will use an ollama connection pool. 
- If its local ollama pool is full, it will try forwarding the request to a random delegate. 
- If the delegate has no available pools, it will try the next one. 
- If the delegate returns a non OK response, it will try the next delegate. 
- If all its delegates are full, then return 500. 

### Streaming
- Ollama streams `/api/chat` and `/api/generate` as NDJSON (one chunk per token). Streaming is the default, so the proxy must not depend on the client's `stream` flag.
- Request side is buffered: the delegate reads the whole request body (capped, ~256 MB for base64 images) so it can read the `model` field and replay the body to another delegate if the first attempt fails.
- Response side is not buffered: the upstream byte stream is passed straight to the client as it arrives, so first-token latency matches talking to ollama directly. Content-Length and hop-by-hop headers are dropped and the response goes out chunked.
- No total timeout on upstream requests (generations can run for minutes); only a connect timeout.
- The pool slot is held for the lifetime of the response stream and released when the stream ends or is dropped. If the client disconnects mid-generation, the upstream connection is dropped too, so ollama cancels generation and the slot is freed.
- Retry / "try next delegate" only happens before any response byte has been sent to the client (connect errors, a full peer returning 503, non-2xx). If the upstream fails after streaming has started, the client connection is cut and the client sees a truncated stream.
- A forwarded request streams the peer's response through the same way. Forwarded requests carry a visited header and are served locally or rejected by the receiving delegate, never forwarded again.

### Announce & Bookkeeping
- every so often, the delegate will send a small keep-alive message to one of its delegate host table, with its available model and avg GPU/CPU load.
  - if the delegate is unresponsive, mark that targetted delegate as unavailable
    - try another random delegate in the list
- if a delegate receives a keep-alive message, it will update its entry for that delegate and the last seen
  - it will then forward that message to a random delegate -- excluding the one that sent it -- in its table
    - if that delegate does not response, it will mark it as unavailable
      - it will only mark it back as available if it gets a keep alive message from it back

### Delegate Listen
- delegates will use the same port for both ollama requests and coordination requests.

## Networking
- Machines must be on a secure network (LAN or VPN like tailscale)

## Logging
- log requests -- not content, just metrics

## UI
- icon in taskbar/menubar should show green for active, blue for in-use, red for down.

## Technical
- language; rust or c++
- communication; http (TCP)
- macos: menubar app
- windows: taskbar app

## Risks
- does not scale with larger number of pools, as each delegate will announce to every other delegate