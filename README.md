# Praxis Relay

One small binary that turns a **ChatGPT subscription** (Plus/Pro) into a **local
OpenAI-compatible LLM API** — a frontier-model brain for any agent framework, at a flat
subscription price.

Point your agent at `http://127.0.0.1:5011/v1`, select a model listed by `GET /v1/models`,
and choose streaming or a final JSON response. The relay provides tool calling, vision input, and usage accounting
through your own ChatGPT subscription. It signs in once and speaks the Codex protocol to
the backend on your behalf; a separate OpenAI API account is not required.

With `RELAY_API_KEY` unset, the client may use a placeholder API key. If the relay or its
host application sets `RELAY_API_KEY`, use that exact value as the client's API key.
The local Bearer key and the ChatGPT subscription login are separate credentials.

Русская памятка по запуску — [ПАМЯТКА.md](ПАМЯТКА.md).

## Why it is more than a proxy

- **It meets clients where they are.** Chat Completions answers with and without the `/v1`
  prefix. Chat calls return SSE with `"stream": true`, or one final JSON completion
  with `false` or an omitted flag. Standalone image calls return final JSON.
  And it impersonates a llama.cpp-style "local model" server well enough that frameworks
  with a local-llama lane plug in unmodified: the context window is advertised under every
  field name such clients read (`meta.n_ctx_train`, `context_window`, `context_length`),
  and the `local-model` slug those clients hardcode resolves to a configurable real model.
- **Ordinary Chat tool schemas keep their semantics.** Missing or null `strict` stays
  non-strict, so optional properties and open objects are forwarded without silently
  enabling stricter validation. Explicit `strict: true` or `false` is preserved, along
  with the caller's parameters; an opted-in strict schema must satisfy upstream rules.
- **A successful stream finishes its Chat choice.** The upstream completion produces
  `finish_reason: "stop"` or `"tool_calls"`, including responses with no final output or
  usage object. Suppressing repeated final text keeps the finish reason and usage.
  Finished tool items alone do not mark a response successful before upstream completes.
- **Subscription-grade resilience.** Two account slots with automatic failover when a quota
  runs out, bounded cooldowns, deliberate switching over the API, token refresh driven by
  the token's actual expiry, and machine-readable terminal errors that distinguish "quota
  exhausted" from "credentials dead" from "upstream tore the stream" — so a client can
  react instead of blindly retrying.
- **A dropped client stops the upstream call.** When the caller hangs up mid-stream — a
  cancelled agent turn, a closed tab — the relay cancels the request it is holding upstream
  instead of reading it to the end. Subscription quota is spent on answers someone is still
  waiting for, and a torn stream is reported as such rather than as an empty reply.
- **The answer is the model's text, not the transport's.** Streaming text is taken from the
  finished message item with its annotations, so inline citation markers and tracking
  parameters do not leak into what the model actually said.
- **Local access is configurable.** The server binds to loopback only. Set `RELAY_API_KEY`
  to require a matching Bearer credential for generation calls. Use `RELAY_LOCAL=1` when
  signing in to keep relay credentials under `./local_auth` in its working directory;
  without that flag, login can use the existing Codex/OpenCode credential locations.

## Image generation and edits

The standalone Codex Images API uses the same subscription login as text calls:
`POST /v1/images/generations` and `POST /v1/images/edits` (the `/v1` prefix is optional).
`GET /v1/images/models` lists the built-in image model separately from chat models.
The initial model is `gpt-image-2`; account access is confirmed by a successful request,
not by its presence in that list.

```json
{"model":"gpt-image-2","prompt":"A blue ceramic cup on a wooden table","n":1,"quality":"low","size":"1024x1024","background":"opaque"}
```

Edits add `"images":[{"image_url":"data:image/png;base64,..."}]` or a backend
`file_id` and use the edits route. Responses preserve `data[].b64_json`, generation
IDs, usage and the `x-codex-imagegen-request-id` header. Send a stable
`x-codex-image-turn-id` for tracing. Decode the returned image to determine its actual
dimensions; the requested size is not an output guarantee.

An interrupted request is never automatically retried: it may already have generated
an image. Image quota errors are returned intact and do not disable the account's text
lane. Transport limits are 144 MiB per request, 64 MiB per response and 32 MiB per image.

## Quick start (Windows)

Build the native executable:

```powershell
cargo build --release --locked
Copy-Item .\target\release\codex-proxy-server.exe .\praxis-relay.exe
$env:RELAY_LOCAL = "1"
.\praxis-relay.exe
```

Menu `3` logs the relay into your ChatGPT account (a browser opens; Python 3 is needed only
for this step — `python.exe`, `py -3`, and `python3.exe` are discovered automatically, or
set `RELAY_PYTHON`). Menu `1` starts the server. Check `http://127.0.0.1:5011/health`.

Menu `7` shows the text the relay is currently sending above every request and opens
it in an editor — see below for why that field is worth owning.

Once the server is running the console hides into a **system tray icon**: double-click the
icon to show or hide the window (with its live log), and quit from the tray menu. Adding a
second subscription: run menu `3` again and answer `2` to the slot question — the existing
login migrates to slot `primary` automatically; restart the server to pick the new slot up.

On Linux, `cargo run` behaves the same; the Dockerfile provides a container build. The
process deliberately binds only to loopback — expose it through a reverse proxy only as an
explicit deployment decision.

## The instructions field, and why you get to write it

Every request carries an `instructions` string that sits **above the entire
conversation** — above your agent's own system prompt, which travels inside the input
where it cannot reach this slot. Whatever stands there frames how the model reads
everything after it, and it is sent on every single call.

Codex fills that slot with ~5k tokens announcing that the model is a terminal coding
assistant. For anything that is not Codex, that is both a per-call tax and a claim
about identity that competes with the one you wrote. So the default is a ~60-word
neutral stub instead, and the retry to the full preamble exists only for the case
where upstream rejects the short form.

The stub is deliberately generic, which means it is not right for anyone in
particular. `instructions.txt` (menu `7`) hands the slot over: your words, at the top
of every call, versioned in a file you own. It is read fresh per request, so tuning it
is a loop rather than a deploy. Delete the file and the built-in text returns.

Two practical notes. The field is part of the cached prefix, so changing it mid-session
costs one cache miss — edit between conversations when that matters. And upstream
validates this field: the built-ins are known-accepted, and if your text is refused the
request retries once on the full preamble rather than failing.

## Endpoints

- `POST /chat/completions` (alias: `POST /v1/chat/completions`) — Chat Completions over
  SSE when `"stream": true`; otherwise one `application/json` Chat completion.
  Tool calls, image input and optional measured usage are preserved in either mode.
- `GET /v1/models` — the models the backend serves this
  subscription right now, read from its own catalog (see below): today
  `gpt-6-astra`, `gpt-5.6-sol/terra/luna`, `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`,
  `gpt-5.3-codex-spark`, each with its context window, input modalities and
  reasoning levels.
- `GET /v1/limits` — remaining subscription quota as reported by the backend.
- `GET /v1/account` / `POST /v1/account/switch` — inspect and deliberately switch the
  active subscription slot (see multi-account below).
- `GET /health` — liveness, the account-router state, where the model catalog came
  from (`live` / `stale` / `fallback` / `static`) and the Codex client version presented.

## The model list is the backend's, not ours

The relay used to carry the model slugs as a constant, edited by hand each time OpenAI
shipped one — a rebuild and a redeploy for a fact the backend already publishes. It
now asks the backend (`GET backend-api/codex/models?client_version=…`, the same call
Codex itself makes) once every `RELAY_MODELS_TTL` seconds and serves `/v1/models` from
the answer. A request for a slug the cached answer does not know forces one early
refresh, so a model released an hour ago works the moment someone asks for it; a typo
is still a loud 404 that lists what is on offer. When the backend cannot be asked, the
last good answer keeps serving, and with no answer ever a built-in list stands in —
`/health` says which.

Two things the catalog taught this relay, both measured live on 2026-09-04:

- The backend gates the list on the **client version** it sees. `gpt-6-astra` carries
  `minimal_client_version: 0.153.0`; the relay now presents the newest released Codex
  CLI (0.153.3) and `RELAY_CODEX_VERSION` overrides it when the next model hides behind
  a newer one.
- Models differ in the **reasoning levels** they accept. `gpt-6-astra` answers
  `400 unsupported_value` to `reasoning.effort: "none"` (its levels are low..max),
  which used to cost a failed call plus a retry carrying the full 5k-token Codex
  preamble. The relay now clamps the effort to the nearest level the model takes
  before sending; a model the catalog lists no levels for is left untouched.

## Codex-Spark, and the quota nobody is spending

`gpt-5.3-codex-spark` is worth calling out because it is not simply one more slug.
It is metered from a **separate bucket**: `/v1/limits` reports it under
`additional_rate_limits` with its own five-hour and weekly windows, so it keeps
answering after the main subscription allowance is spent. On one long generation
through this relay it produced ~400 characters per second against ~219 for
`gpt-5.4`, finishing in about a third of the wall clock — it writes faster and
shorter. Two such calls consumed about 4% of the five-hour window, so the bucket
is separate, not bottomless.

The trade is real: it is text-only (no image input), its context is 128k rather
than the 272k the other models report, and it is tuned for code rather than
conversation. Treat it as a fast lane, not a default.

## Configuration (environment variables)

- `RELAY_PORT` — listen port, default `5011` (always loopback-only).
- `RELAY_DEFAULT_MODEL` — what the `local-model` alias resolves to, default `gpt-5.4`.
- `RELAY_CONTEXT_LENGTH` — overrides the context window advertised in `/v1/models`.
  By default each model carries the number the backend reports for it (272000 today);
  `400000` stands in for slugs the catalog does not describe. Advisory metadata for
  clients; the real limit is enforced upstream.
- `RELAY_CODEX_VERSION` — explicit override for the Codex CLI version presented upstream.
  When unset, startup looks up the latest Codex version and falls back to its built-in
  version if discovery fails. The backend can gate model visibility on this version.
- `RELAY_MODELS_TTL` — seconds between model-catalog refreshes, default `600`.
- `RELAY_EXTRA_MODELS` — comma-separated slugs accepted and advertised on top of the
  catalog (a slug the backend hides, or one it lists only for a newer client).
- `RELAY_MODEL_DISCOVERY` — `off` serves the built-in list only and never asks the backend.
- `RELAY_LOCAL` — set to `1` when signing in to store credentials under `./local_auth`
  in the working directory. Serving prefers that directory when an auth file exists.
- `RELAY_API_KEY` — optional local Bearer key required by generation endpoints when set.
  With it unset, client API keys are not checked. This does not replace subscription login.
- `RELAY_PYTHON` — explicit Python 3 interpreter for the login helper.
- `RELAY_REASONING_EFFORT` — default reasoning effort applied when a request carries none
  (`none` by default; requests may override via their own `reasoning_effort` field).
- `RELAY_INSTRUCTIONS` — `minimal` (default: a ~60-word stub; your agent's own system
  prompt travels in the input either way) or `codex` (the full ~5k-token Codex-CLI
  preamble on every call). A rejected minimal request automatically retries with the
  full prompt.
- `RELAY_INSTRUCTIONS_FILE` — path to a plain-text file whose contents replace the
  instructions outright, outranking `RELAY_INSTRUCTIONS`. Unset, the relay reads
  `instructions.txt` next to the executable. Re-read per request, so an edit lands on
  the next call with no restart; a missing or blank file just means "no override".
- `RELAY_EDITOR` — editor for menu `7` (falls back to `VISUAL`, `EDITOR`, then
  `notepad` on Windows and `nano` elsewhere).
- `RELAY_PARALLEL_TOOL_CALLS` — `true` (default) or `false`.
- `RELAY_ACCOUNT_COOLDOWN_SECONDS` — how long an exhausted subscription slot stays parked.
- `RELAY_LOG_DIR` — log directory, default `logs` under the working directory.

## Separate relay authentication

For a separate relay login, set `RELAY_LOCAL=1`, start the executable from its own
working directory, and choose menu option `3` to authorize, then `1` to serve the API.
Login writes `./local_auth/auth.json`; serving prefers that local directory when its auth
file exists. Without the local-login flag, the legacy login flow can use `~/.codex` or
`~/.opencode`. Keep the working directory consistent between login and serving.

If `RELAY_API_KEY` is set, clients must present it as `Authorization: Bearer <key>`.
Otherwise a placeholder client key is accepted. This local key is independent of the
subscription credentials stored by login.

### Multi-account

Two subscription slots with automatic failover are supported: place credentials in
`local_auth/accounts/primary/auth.json` and `local_auth/accounts/secondary/auth.json`.
A confirmed quota exhaustion (or a broken active profile) parks the active slot for a
bounded cooldown and switches to the standby; `POST /v1/account/switch {"slot": "..."}`
switches deliberately and clears the cooldown. A single legacy `local_auth/auth.json` keeps
working as the sole `primary` slot.

## The local-model contract, in detail

The relay can stand in for a llama.cpp-style localhost server, so agent frameworks with a
"local model" lane can point that lane at a subscription instead:

- `/v1/models` entries carry the context window under every field name common localhost
  clients read: `meta.n_ctx_train` (llama-cpp-python convention), `context_window`, and
  `context_length` (LM Studio/OpenRouter convention). Clients that size their history by
  asking the endpoint no longer see 0.
- The exact model slug `local-model` — hardcoded by clients built against llama-cpp-python,
  which ignores the field — resolves to `RELAY_DEFAULT_MODEL`. Any other unknown slug is
  still a 404 against the live catalog, so typos in real model names keep failing loudly.
- Chat calls honor `stream`: true returns SSE; false or omitted returns final JSON.
  Successful choices finish with `stop` or `tool_calls`. SSE closes without a separate
  `[DONE]` event. JSON failures retain a top-level `error` and optional `relay_terminal`
  under HTTP 200 (as SSE terminals do), never partial executable tool calls or diagnostic
  text as a successful answer. Missing usage stays absent. Dropping either response body
  cancels the same upstream request; aggregation never starts a second generation.
- Function tools retain the parameters provided by the client. Missing or null `strict`
  maps to `false`; explicit `true` and `false` are preserved. The relay does not rewrite
  optional properties, add `additionalProperties: false`, or make every property required.
- Tool selection currently uses `auto`. Chat `tool_choice` constraints and
  `response_format` translation are not implemented by this adapter.

The Ollama-native protocol (`/api/tags`, `/api/chat`) is not spoken; use a framework's
OpenAI-compatible mode.

## Authentication and privacy

Use `RELAY_LOCAL=1` during login for credentials dedicated to the relay. Serving prefers
existing local credentials and otherwise checks legacy Codex/OpenCode locations. Never
commit `auth.json`, API keys, session files, or relay logs. This repository contains source code
only; it deliberately contains no account data.

## License and provenance

This project is distributed under the **MIT License** — see [LICENSE](LICENSE). Take it,
fork it, vendor it, ship it closed — the only condition is that the copyright notice
travels with the code.

It is derived from
[unluckyjori/Codex-Proxy-Server](https://github.com/unluckyjori/Codex-Proxy-Server)
at upstream revision `57417d107dc100d4dfd15fd3fcf11350e9b71088`, also MIT-licensed. The
original copyright notice is carried in [LICENSE](LICENSE) alongside this project's own,
as the MIT license requires.

Parts of the sign-in path and `src/core/prompt.md` descend, through that upstream, from the
[OpenAI Codex CLI](https://github.com/openai/codex) (Apache-2.0, Copyright 2025 OpenAI).
[NOTICE](NOTICE) names those files and states that they were modified, as Apache-2.0
requires of anyone redistributing them.

This project is independently maintained and is not affiliated with or endorsed by OpenAI.
It speaks the Codex protocol to the ChatGPT backend; treat it as an unofficial bridge and
use it with your own subscription at your own risk.
