# Praxis Relay

Version 1.1.2 adds per-request Fast mode. It includes Ouroboros 7.9.0
cache-hinted request support introduced in 1.1.1.

A local OpenAI-compatible API backed by your ChatGPT subscription. A native
**Tauri desktop app** and a separate **headless executable** share one Rust
service for login, logout, quotas and account switching.

The relay is an independent program. The displayed Codex version identifies the
upstream protocol; Codex CLI is not an executable dependency.

## Platform downloads

Version 1.1 adds macOS **Intel + Apple Silicon** application bundles and a DMG,
Linux x86_64 **DEB and RPM** packages, and standalone Unix headless archives.
The same beige, frameless interface and shared account service run on each OS.
Linux packages include both executables and integrate with the application menu.
See [platform packaging and requirements](packaging/README.md).

macOS bundles are signed ad hoc, not notarized with Apple Developer ID; macOS
can require approval in Privacy & Security for an Internet download. The minimum
macOS version is 13.3. The Linux desktop uses GTK 3 and WebKitGTK 4.1; DEB/RPM
metadata installs the runtime dependencies. Debian 12 and Fedora 44 are the
installed-package validation targets. A headless archive needs no display.

## Desktop

Run `praxis-relay-desktop.exe`. The Russian interface provides two account slots,
browser sign-in/cancellation, replacement sign-in, logout, deliberate switching,
per-account quotas, API start/stop, port settings and session activity.
Sign in and start the API, then copy the base URL into your client.

Minimize and close hide the native window from the taskbar into its tray icon.
Click the icon to restore it. Its **Exit and stop the relay** item quits the app.
There is no companion console window. A failed tray initialization cannot hide
the window and strand the running program.

## Headless

`praxis-relay-headless` has no Tauri, WebView, tray or display dependency.

```text
praxis-relay-headless serve --headless
praxis-relay-headless status
praxis-relay-headless login primary
praxis-relay-headless login secondary
praxis-relay-headless switch secondary
praxis-relay-headless limits primary
praxis-relay-headless logout primary
praxis-relay-headless logout all
praxis-relay-headless cancel_login
praxis-relay-headless stop
```

Use a second terminal for controls while the service runs. Applicable commands
also work offline. `login` defaults to `primary`. Put both executables in the
same directory to share storage; run one server mode per installation at a time.
`--help` and `--version` require no credentials. Startup without an account keeps
management available after the last logout.

Headless control requires a random per-run capability from `local_auth/control.json`.
The control route rejects missing/wrong capabilities and browser Origin headers.
Do not share this file or the data directory.

For a Linux service, use a writable persistent directory for every command:

```sh
RELAY_AUTH_DIR=/var/lib/praxis-relay ./praxis-relay-headless serve --headless
```

OAuth requires a browser and a loopback callback on port 1455. A remote headless
host needs that callback forwarded to the browser's computer. Serving requests
requires neither a browser nor Python.

## Login and storage

Python **3.10+** is needed only for login. Windows discovery probes `python.exe`,
`py -3`, then `python3.exe` and verifies a real interpreter with SSL. Microsoft
Store aliases are rejected. `RELAY_PYTHON` is an explicit executable override;
an invalid override is reported rather than silently ignored.

On Windows the data directory defaults to `local_auth` next to the executable.
macOS uses `~/Library/Application Support/praxis-relay`; Linux uses
`$XDG_DATA_HOME/praxis-relay` or `~/.local/share/praxis-relay`. Both GUI and CLI
select the same path. A pre-existing adjacent `local_auth` remains portable.
`RELAY_AUTH_DIR` overrides it. Embedding hosts with `RELAY_LOCAL=1` retain the
working-directory `local_auth` contract. Login, serving, refresh and logout all
use the same directory. The relay does not discover, borrow or modify credentials
from `~/.codex` or `~/.opencode`.

Both legacy `local_auth/auth.json` and existing
`local_auth/accounts/primary/auth.json` layouts are supported. The second slot is
`local_auth/accounts/secondary/auth.json`. A failed or cancelled sign-in preserves
the previous account; a successful, validated sign-in replaces it atomically.
Logout removes the selected relay slot. Existing requests may finish their leased
turn; new requests re-read current credentials. It does not remotely revoke an
upstream session or sign out another application.

## Client connection and API

Default base URL: `http://127.0.0.1:5011/v1`. Copy the client API key from the
desktop connection card or headless `status`. A new installation generates a
47-character key starting with `sk-` and stores it in `local_auth/api-key.txt`.
The same key survives restarts, account changes and switching between desktop
and headless. Missing or incorrect client keys are refused for generation.
`RELAY_API_KEY` explicitly overrides it for embedding hosts without modifying
the saved default. This local credential is separate from subscription tokens.
The listener always binds to loopback; do not share the key or data directory.

- `POST /v1/chat/completions` (also `/chat/completions`): final JSON with `stream`
  false/absent, SSE with true; tool calls and vision. Omitted tool `strict` stays
  false, explicit schemas are preserved, successful completion closes the choice,
  and interrupted visible output is not silently replayed.
  Optional `cache_control` metadata on content blocks and tool declarations is
  accepted and ignored for compatible clients such as Ouroboros. These hints do
  not enable Anthropic cache/TTL semantics on the Codex subscription route;
  `prompt_cache_key` retains its existing behavior. Other content/tool fields
  remain strictly validated.
- `GET /v1/models`: backend-discovered catalog with bounded caching and fallback.
  Optional `service_tiers` and `additional_speed_tiers` preserve the selected
  model's advertised processing capabilities. Missing metadata is unknown;
  explicit empty arrays advertise no additional tier. The desktop/headless
  `models` control includes the same model documents in its `data` field.
- `GET /v1/limits`: active-account usage. Authoritative exhaustion may cause
  failover. Desktop per-account inspection is read-only and spends no model turn.
- `GET /v1/account`, `POST /v1/account/switch` with `{"slot":"secondary"}`:
  inspect/select slots. Browser-origin account mutations are refused.
- `POST /v1/images/generations`, `POST /v1/images/edits` (also without `/v1`):
  final image JSON through the same subscription, initially `gpt-image-2`.
  An interrupted image request is not automatically retried.
- `GET /v1/images/models`, `GET /health`: capability metadata and liveness.

### Fast mode

Clients can opt in per request on either chat endpoint, in desktop or headless
mode. Set `"service_tier":"fast"` or `"service_tier":"priority"`. The relay
sends the canonical `priority` value to Codex Responses and keeps the requested
reasoning effort and conversation input independent of the processing tier.

```json
{"model":"gpt-5.6-sol","service_tier":"fast","stream":true,
 "messages":[{"role":"user","content":"Hello"}]}
```

The optional field also accepts `auto`, `default` and `flex`. Omission or `null`
keeps the existing upstream default. Invalid values and JSON types return
HTTP400 with `error.param` set to `service_tier`, before an upstream request.
Account and model availability are determined by Codex. Explicit processing-tier
requests retain their reasoning, cache and instruction parameters when rejected,
without a conservative retry that changes those values.

JSON responses and SSE chunks preserve the `service_tier` observed upstream.
An early observation is carried into later chunks, and a terminal observation
takes precedence. An explicit upstream null clears the earlier observation. A request for Fast mode may therefore return `default` if
upstream selects standard processing. If upstream never reports the field, the relay
also omits it. Fast mode can consume subscription allowance or credits at a
higher rate. Check your account's applicable terms before enabling it.

See [OpenAI Fast mode documentation](https://developers.openai.com/api/docs/guides/fast-mode)
and [Codex speed configuration](https://learn.chatgpt.com/docs/agent-configuration/speed).

A listed model is not proof of account entitlement. Quota exhaustion may move
new requests to a distinct configured account. The exhausted account is then
skipped as a failover target until the reset time the vendor reported, kept
between 60 seconds and 7 days, or for `RELAY_ACCOUNT_COOLDOWN_SECONDS`
(15 minutes by default) when no usable reset was reported. A deliberate switch
clears that wait. Additional model quota buckets remain separate; switching
accounts does not merge or transfer them.

## Desktop preferences and headless configuration

The frameless desktop uses its own window controls; close and minimize hide it
in the tray. Ctrl+1/2/3 and Ctrl+, open sections; Ctrl+Q exits. Paper palettes,
light/dark/system themes, six accents, text size, density, handwriting, grain
and reduced motion apply immediately. The client key is masked until revealed.

`local_auth/settings.json` also stores account labels, optional quota polling
while the window is visible, start-in-tray and API-on-launch preferences, port,
and request defaults (model, reasoning, parallel tools and instructions).
Existing port-only settings migrate without changing credentials or the key.
Changed API defaults are applied on the next API start; a running desktop
shows a restart action. Appearance changes do not restart the server.
Explicit environment variables take priority and their fields are marked read-only.

The headless CLI uses the same validated settings, offline or through the
authenticated controls of an already running process:

```sh
praxis-relay-headless configure patch.json
```

Example patch (unspecified fields are preserved):

```json
{"appearance":{"theme":"dark"},"labels":{"primary":"Work"},"defaults":{"reasoning":"high"}}
```

A missing/blank chat model uses the saved model, or the first listed catalog
model when no default was chosen. Explicit request values retain priority.
Additional instructions accompany either built-in instruction mode.
Fresh installations use the model's default reasoning depth. The desktop's
reasoning choices follow the selected model's advertised `reasoning_efforts`.
A missing catalog offers the model default until metadata is available.
Legacy saved `none` remains readable and is applied as the model default, with
an explanation in the desktop. Reading old settings leaves their file intact.
Explicit API requests retain the existing catalog-validation compatibility.

An existing nonempty `instructions.txt` / `RELAY_INSTRUCTIONS_FILE` remains a
complete per-request override; the desktop explains this and disables the
superseded instruction fields. No provider entitlement is inferred from a setting.

## Build

Rust 1.94+. The desktop also needs the Tauri platform prerequisites:
https://v2.tauri.app/start/prerequisites/
Its HTML/CSS/JavaScript frontend is embedded; no Node build or package install.

```sh
cargo test --locked -- --test-threads=1
cargo build --release --locked --bin praxis-relay-headless
cargo build --manifest-path desktop/Cargo.toml --release --locked --target-dir target
```

Outputs: `target/release/praxis-relay-headless` and
`target/release/praxis-relay-desktop` (`.exe` on Windows). Headless builds do not
compile desktop dependencies. Both lockfiles are committed. Docker builds only
the headless service; use host networking for its loopback listener.

Configuration: `RELAY_PORT`, `RELAY_AUTH_DIR`, `RELAY_LOCAL`, `RELAY_PYTHON`,
`RELAY_API_KEY`, `RELAY_CODEX_VERSION`, `RELAY_DEFAULT_MODEL`, `RELAY_EXTRA_MODELS`,
`RELAY_MODELS_TTL`, `RELAY_MODEL_DISCOVERY`, `RELAY_CONTEXT_LENGTH`,
`RELAY_INSTRUCTIONS`, `RELAY_INSTRUCTIONS_FILE`, `RELAY_REASONING_EFFORT`,
`RELAY_PARALLEL_TOOL_CALLS`, `RELAY_ACCOUNT_STATE_FILE`,
`RELAY_ACCOUNT_COOLDOWN_SECONDS`, `RUST_LOG`.

The protocol version is resolved at startup, with a built-in fallback and an
explicit override. Subscription access is confirmed by actual upstream requests,
separately from local build and UI checks. MIT license; vendored OpenAI login
components retain Apache-2.0 notices. See `LICENSE`, `LICENSE-APACHE`, `NOTICE`.
