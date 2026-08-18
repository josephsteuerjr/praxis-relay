# Praxis Relay for Windows

`relay` is a standalone Rust/Axum service that exposes a small OpenAI-compatible API on
`127.0.0.1:5011`, backed by a ChatGPT subscription (Plus/Pro) instead of API billing.

This Windows-focused edition tracks the production relay used by the Praxis deployment
(`/opt/relay/Code`, synced at revision `84e6911`) and adds on top of it: a native Windows
build, Windows-aware Python discovery for the login flow, a relay-owned OAuth store that
stays isolated from the user's normal Codex credentials, non-streaming responses, and
route aliases for picky OpenAI clients. A quick-start guide in Russian lives in
[ПАМЯТКА.md](ПАМЯТКА.md).

## Upstream and license

This project is derived from
[unluckyjori/Codex-Proxy-Server](https://github.com/unluckyjori/Codex-Proxy-Server)
at upstream revision `57417d107dc100d4dfd15fd3fcf11350e9b71088`.
The original project and this derivative are distributed under the MIT License. The original
copyright and permission notice are preserved in [LICENSE](LICENSE).

This project is independently maintained and is not affiliated with or endorsed by OpenAI.
It speaks the Codex protocol to the ChatGPT backend; treat it as an unofficial bridge and
use it with your own subscription at your own risk.

## Endpoints

- `POST /chat/completions` (alias: `POST /v1/chat/completions`) — Chat Completions,
  streaming (SSE) when the request carries `"stream": true`, a single aggregated
  `chat.completion` JSON object otherwise. Tool calls and image input are supported.
- `GET /v1/models` (alias: `GET /models`) — the supported model list
  (`gpt-5.6-sol/terra/luna`, `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`).
- `GET /v1/limits` — remaining subscription quota as reported by the backend.
- `GET /v1/account` / `POST /v1/account/switch` — inspect and deliberately switch
  the active subscription slot (see multi-account below).
- `GET /health` — liveness plus the account-router state.

The API key presented by clients is ignored; any placeholder string works. Authentication
towards the backend is the relay's own OAuth store.

### Local-model contract

The relay can stand in for a llama.cpp-style localhost server, so agent frameworks with a
"local model" lane (e.g. Ouroboros) can point that lane at a subscription instead:

- `/v1/models` entries carry the context window under every field name common localhost
  clients read: `meta.n_ctx_train` (llama-cpp-python convention), `context_window`, and
  `context_length` (LM Studio/OpenRouter convention). Clients that size their history by
  asking the endpoint no longer see 0.
- The exact model slug `local-model` — hardcoded by clients built against llama-cpp-python,
  which ignores the field — resolves to `RELAY_DEFAULT_MODEL`. Any other unknown slug is
  still a strict-list 404, so typos in real model names keep failing loudly.
- Requests without `"stream": true` get a single aggregated JSON response.

The Ollama-native protocol (`/api/tags`, `/api/chat`) is not spoken; use a framework's
OpenAI-compatible mode.

## Run locally

Install a current Rust toolchain, then:

```bash
cargo run
```

Choose `3` in the menu to log in, then `1` to start the server. The Dockerfile provides a
container build for Linux deployments; the process deliberately binds only to loopback, so
expose it through a separate reverse proxy only when that is an explicit deployment decision.

### Windows

Build and run the native executable from PowerShell:

```powershell
cargo build --release --locked
Copy-Item .\target\release\codex-proxy-server.exe .\praxis-relay.exe
.\praxis-relay.exe
```

The server itself does not require Python. Menu option `3` (interactive ChatGPT login) uses
the first working Python 3 launcher among `python.exe`, `py.exe -3`, and `python3.exe`.
Set `RELAY_PYTHON` to an explicit interpreter path if automatic discovery is unsuitable.

### Configuration (environment variables)

- `RELAY_PORT` — listen port, default `5011` (always loopback-only).
- `RELAY_DEFAULT_MODEL` — what the `local-model` alias resolves to, default `gpt-5.4`.
- `RELAY_CONTEXT_LENGTH` — context window advertised in `/v1/models`, default `400000`
  (advisory metadata for clients; the real limit is enforced upstream).
- `RELAY_AUTH_DIR` — credential directory, default `local_auth` next to the executable.
- `RELAY_PYTHON` — explicit Python 3 interpreter for the login helper.
- `RELAY_REASONING_EFFORT` — default reasoning effort applied when a request carries none
  (`none` by default; requests may override via their own `reasoning_effort` field).
- `RELAY_INSTRUCTIONS` — `codex` (default) or `minimal` system-instructions mode.
- `RELAY_PARALLEL_TOOL_CALLS` — `true` (default) or `false`.
- `RELAY_ACCOUNT_COOLDOWN_SECONDS` — how long an exhausted subscription slot stays parked.
- `RELAY_LOG_DIR` — log directory, default `logs` under the working directory.

## Separate relay authentication

The relay never reads `~/.codex/auth.json` or `~/.opencode/auth.json`. Its credentials live
only in `local_auth/auth.json` next to the executable (or `RELAY_AUTH_DIR`), matching the
isolated `/app/local_auth` mount used on the server.

On a clean installation, start the executable and choose menu option `3` to authorize the
relay account, then `1` to serve the API.

### Multi-account

Two subscription slots with automatic failover are supported: place credentials in
`local_auth/accounts/primary/auth.json` and `local_auth/accounts/secondary/auth.json`.
A confirmed quota exhaustion (or a broken active profile) parks the active slot for a
bounded cooldown and switches to the standby; `POST /v1/account/switch {"slot": "..."}`
switches deliberately and clears the cooldown. A single legacy `local_auth/auth.json` keeps
working as the sole `primary` slot.

## Authentication and privacy

The relay reads authentication only from its dedicated auth directory. Never commit
`auth.json`, API keys, session files, or relay logs. This public copy contains source code
and test fixtures only; it deliberately contains no account data.
