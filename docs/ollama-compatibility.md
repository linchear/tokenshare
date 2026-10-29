# Ollama compatibility

Token Share speaks ollama's API on its listening port, so existing clients work
unchanged. With [masquerade](configuration.md#masquerade) on, that port is 11434,
ollama's own; otherwise point clients at `http://host:11343`.

## What is routed across the pool

| Endpoint | Behavior |
| --- | --- |
| `POST /api/chat`, `/api/generate`, `/api/embed`, `/api/embeddings` | routed by `model`: local ollama if it has the model and a free slot, otherwise a peer that does |
| `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | the same, for OpenAI-compatible clients (SSE streams pass through untouched) |
| `POST /api/show` | routed by model like a chat request, but does not use a pool slot. `ollama run` and `ollama show` start here. The model may be sent as `model` or `name` |
| `GET /api/tags` | the union of models across this machine and every online peer. This is what `ollama ls` calls |
| `GET /api/ps` | passed to the local ollama; an empty list when there is no local ollama |
| `GET`/`HEAD /` | answered by the delegate ("Ollama is running") |
| `GET /api/version` | passed to the local ollama if there is one; otherwise a fixed `0.12.0` |
| everything else (`pull`, `create`, `delete`, `copy`, ...) | this machine's own ollama only; `503` without one |

A model nobody has is a `404`, which is what makes the `ollama` CLI offer to pull
it. If every candidate is busy you get `503` with `Retry-After`.

## Details that matter to clients

The `ollama` CLI and most clients check `HEAD /` (and sometimes `/api/version`)
before anything else, and treat a failure as "no server running". The CLI then tries
to launch Ollama.app. A delegate with no local ollama must therefore answer these
itself, and it does, on both of its ports.

`/api/tags` lists this machine's models exactly as ollama reports them, plus peers'
models with the fields filled in as far as the delegate knows them. A peer model
carries:

- a **stand-in digest**, a stable hash of the name (not the real content digest);
- a **size of 0**;
- a **modified time** equal to when the peer was last heard from.

`ollama ls` slices the first 12 characters of every digest and crashes on an empty
one, which is why every field is filled in.

## Using `ollama` against it

```bash
export OLLAMA_HOST=http://localhost:11343   # or nothing, with --masquerade
ollama ls
ollama run llama3.2 "hello"
ollama show llama3.2
ollama ps
```

`ollama ls`, `run`, `show` and `ps` all work through a client-only delegate. Running
`ollama run` for a model nobody has ends with "no local ollama available", because
the CLI then tries to `pull`, which needs a local ollama.
