# Praxis Relay

One small binary that turns a **ChatGPT subscription** (Plus/Pro) into a **local
OpenAI-compatible LLM API** — a frontier-model brain for any agent framework, at a flat
subscription price.

Point your agent at `http://127.0.0.1:5011/v1` with any placeholder string as the API key,
and it talks to the GPT-5.6 family with tool calling, vision input, streaming, and real
usage accounting. No OpenAI API account, no per-token billing, no key to leak. The relay
signs into your own ChatGPT account once and speaks the Codex protocol to the backend on
your behalf.

Русская памятка по запуску — [ПАМЯТКА.md](ПАМЯТКА.md).

## Why it is more than a proxy

- **It meets clients where they are.** Chat Completions answers with and without the `/v1`
  prefix; `"stream": true` gets SSE, everything else gets one aggregated JSON response.
  And it impersonates a llama.cpp-style "local model" server well enough that frameworks
  with a local-llama lane plug in unmodified: the context window is advertised under every
  field name such clients read (`meta.n_ctx_train`, `context_window`, `context_length`),
  and the `local-model` slug those clients hardcode resolves to a configurable real model.
- **Arbitrary tool schemas survive the strict backend.** The subscription backend validates
  function schemas against a strict subset — `additionalProperties: false`, an exhaustive
  `required`, a `type` on every node — that ordinary framework registries and MCP servers
  do not speak, and one non-conforming function fails the whole request. The relay rewrites
  every schema on the fly, preserving optionality through nullable types. Proven live on an
  agent turn carrying 98 tools.
- **Subscription-grade resilience.** Two account slots with automatic failover when a quota
  runs out, bounded cooldowns, deliberate switching over the API, token refresh driven by
  the token's actual expiry, and machine-readable terminal errors that distinguish "quota
  exhausted" from "credentials dead" from "upstream tore the stream" — so a client can
  react instead of blindly retrying.
- **Private by construction.** The server binds to loopback only. Credentials live in one
  directory next to the binary — never `~/.codex`, never the shell environment — and the
  API key your client presents is decorative.

## Quick start (Windows)

Build the native executable (or take a prebuilt `praxis-relay.exe`):

```powershell
cargo build --release --locked
Copy-Item .\target\release\codex-proxy-server.exe .\praxis-relay.exe
.\praxis-relay.exe
```

Menu `3` logs the relay into your ChatGPT account (a browser opens; Python 3 is needed only
for this step — `python.exe`, `py -3`, and `python3.exe` are discovered automatically, or
set `RELAY_PYTHON`). Menu `1` starts the server. Check `http://127.0.0.1:5011/health`.

Once the server is running the console hides into a **system tray icon**: double-click the
icon to show or hide the window (with its live log), and quit from the tray menu. Adding a
second subscription: run menu `3` again and answer `2` to the slot question — the existing
login migrates to slot `primary` automatically; restart the server to pick the new slot up.

On Linux, `cargo run` behaves the same; the Dockerfile provides a container build. The
process deliberately binds only to loopback — expose it through a reverse proxy only as an
explicit deployment decision.

## Endpoints

- `POST /chat/completions` (alias: `POST /v1/chat/completions`) — Chat Completions,
  streaming (SSE) when the request carries `"stream": true`, a single aggregated
  `chat.completion` JSON object otherwise. Tool calls and image input are supported.
- `GET /v1/models` (alias: `GET /models`) — the supported model list
  (`gpt-5.6-sol/terra/luna`, `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`) with advertised context
  metadata.
- `GET /v1/limits` — remaining subscription quota as reported by the backend.
- `GET /v1/account` / `POST /v1/account/switch` — inspect and deliberately switch the
  active subscription slot (see multi-account below).
- `GET /health` — liveness plus the account-router state.

## Configuration (environment variables)

- `RELAY_PORT` — listen port, default `5011` (always loopback-only).
- `RELAY_DEFAULT_MODEL` — what the `local-model` alias resolves to, default `gpt-5.4`.
- `RELAY_CONTEXT_LENGTH` — context window advertised in `/v1/models`, default `400000`
  (advisory metadata for clients; the real limit is enforced upstream).
- `RELAY_AUTH_DIR` — credential directory, default `local_auth` next to the executable.
- `RELAY_PYTHON` — explicit Python 3 interpreter for the login helper.
- `RELAY_REASONING_EFFORT` — default reasoning effort applied when a request carries none
  (`none` by default; requests may override via their own `reasoning_effort` field).
- `RELAY_INSTRUCTIONS` — `minimal` (default: a ~60-word stub; your agent's own system
  prompt travels in the input either way) or `codex` (the full ~5k-token Codex-CLI
  preamble on every call). A rejected minimal request automatically retries with the
  full prompt.
- `RELAY_PARALLEL_TOOL_CALLS` — `true` (default) or `false`.
- `RELAY_ACCOUNT_COOLDOWN_SECONDS` — how long an exhausted subscription slot stays parked.
- `RELAY_LOG_DIR` — log directory, default `logs` under the working directory.

## Separate relay authentication

The relay never reads `~/.codex/auth.json` or `~/.opencode/auth.json`. Its credentials live
only in `local_auth/auth.json` next to the executable (or `RELAY_AUTH_DIR`).

On a clean installation, start the executable and choose menu option `3` to authorize the
relay account, then `1` to serve the API.

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
  still a strict-list 404, so typos in real model names keep failing loudly.
- Requests without `"stream": true` get a single aggregated JSON response.
- `tool_choice` travels through — `"required"`, `"none"`, and named-function forms included
  (the Chat-Completions `{"type":"function","function":{"name":…}}` shape is reshaped for
  the Responses API), so a client's mandatory tool call stays mandatory. `response_format`
  maps to the Responses `text.format`: `json_object` and flattened `json_schema` (whose
  schema passes the same strictifier). Both verified live.
- Function tools are sent upstream with `strict: true` (the relay default), and every tool
  schema is normalized to the strict subset the backend validates: `additionalProperties:
  false` plus a full `required` on every object, a `type` synthesized for shapeless nodes
  (a bare `{}` becomes `"string"`; enums infer from their members), with original
  optionality preserved by adding `null` to the type — and to the enum — of properties that
  were not originally required. A client that explicitly sets `strict: false` on a function
  gets its schema forwarded untouched.

The Ollama-native protocol (`/api/tags`, `/api/chat`) is not spoken; use a framework's
OpenAI-compatible mode.

## Authentication and privacy

The relay reads authentication only from its dedicated auth directory. Never commit
`auth.json`, API keys, session files, or relay logs. This repository contains source code
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
