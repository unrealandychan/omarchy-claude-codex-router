# Omarchy Claude Code & OpenAI Codex $\rightarrow$ Vertex AI Gemini Router (Rust Edition)

A high-performance, zero-dependency-drift **Rust** proxy server (`claude-gemini-router`) and **Omarchy status bar plugin + CLI** (`arch.gemini-router` / `omarchy-gemini-router-ctl`) that routes **Claude Code** (`claude`) and **OpenAI Codex CLI** (`codex`) directly to **Google Cloud Vertex AI** for **Gemini 3.8 Flash** (`gemini-3.8-flash`), **Gemini 4.0 Argon** (`gemini-4-argon`), and **Gemini 3.1 Pro / 3 Flash** using **Application Default Credentials (ADC)** — with **zero API keys**.

---

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Key Features & Handled Gotchas](#key-features--handled-gotchas)
- [Function Calling, Skills & Tool Search Support](#function-calling-skills--tool-search-support)
- [Model Tier & Reasoning Effort Routing](#model-tier--reasoning-effort-routing)
- [Supported HTTP Endpoints](#supported-http-endpoints)
- [Installation & Quick Start](#installation--quick-start)
  - [1. Full Installation (Daemon + CLI + Omarchy Bar Plugin)](#1-full-installation-daemon--cli--omarchy-bar-plugin)
  - [2. Build from Source with Cargo](#2-build-from-source-with-cargo)
  - [3. One-Command Wrapper Scripts](#3-one-command-wrapper-scripts)
- [CLI Reference (`omarchy-gemini-router-ctl`)](#cli-reference-omarchy-gemini-router-ctl)
- [Router Daemon CLI & Environment Variables](#router-daemon-cli--environment-variables)
- [Testing & Verification](#testing--verification)
- [Project Structure](#project-structure)
- [Author & License](#author--license)

---

## Architecture Overview

```text
┌───────────────────────────┐       ┌───────────────────────────┐
│   Claude Code (`claude`)  │       │   OpenAI Codex (`codex`)  │
│  Anthropic Messages API   │       │  Responses & Chat APIs    │
│      POST /v1/messages    │       │  POST /v1/responses       │
└─────────────┬─────────────┘       └─────────────┬─────────────┘
              │                                   │
              └─────────────────┬─────────────────┘
                                ▼
        ┌───────────────────────────────────────────────┐
        │     claude-gemini-router (Rust / Tokio)       │
        │             http://127.0.0.1:4000             │
        ├───────────────────────────────────────────────┤
        │ • Multi-Protocol Translator (Unary + SSE)     │
        │ • Deep JSON Schema Sanitizer ($ref/$defs/...) │
        │ • ThoughtSignature Cache & Replay Store       │
        │ • Hot-Swappable Model & Reasoning Effort      │
        │ • ADC OAuth2 Token Manager (Keyless Only)     │
        └───────────────────────┬───────────────────────┘
                                │ HTTPS (Bearer ADC Token)
                                ▼
        ┌───────────────────────────────────────────────┐
        │           Google Cloud Vertex AI              │
        │   generateContent / streamGenerateContent     │
        │  gemini-3.8-flash · gemini-4-argon · 3.1-pro  │
        └───────────────────────────────────────────────┘
```

---

## Key Features & Handled Gotchas

1. **Pure Rust Native Binaries (Zero Python / Virtualenv Overhead)**:
   - Built with **Rust + Tokio + Reqwest (Rustls)** into two standalone native binaries:
     - `claude-gemini-router`: Async HTTP/1.1 & Server-Sent Events (SSE) translation proxy.
     - `omarchy-gemini-router-ctl`: Security-hardened (`O_NOFOLLOW`, atomic file replacement, UID verification) CLI and Quickshell backend.
   - Eliminates Python `.venv` breakage, `uv`/`pip` package drift, and startup latency.

2. **Strict Keyless ADC Enforcement**:
   - Automatically strips `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `ANTHROPIC_API_KEY`, and `OPENAI_API_KEY` from the process environment on startup.
   - Sets `GOOGLE_API_USE_CLIENT_CERTIFICATE=false` so corporate/Cloudtop Enterprise Certificate Proxy (ECP) environments never mint mTLS/CBA-bound tokens that fail over standard HTTPS endpoints.
   - Refreshes OAuth2 access tokens directly via `~/.config/gcloud/application_default_credentials.json` with automatic fallback to `gcloud auth application-default print-access-token`.

3. **Full Multi-Protocol Translation (Unary & Streaming SSE)**:
   - **Anthropic Messages API (`/v1/messages`, `/v1/messages/count_tokens`)**: Translates system prompts, user/assistant messages, `thinking` blocks, `tool_use`, `tool_result`, `tool_reference`, and streams spec-compliant Anthropic SSE events (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`).
   - **OpenAI Codex Responses API (`/v1/responses`)**: Native wire protocol support for OpenAI Codex CLI (`wire_api = "responses"`), translating `input_text`, `output_text`, `reasoning` (`encrypted_content` signature preservation), `function_call`, `function_call_output`, and `custom_tool_call` / `custom_tool_call_output` (such as Codex's `apply_patch` freeform grammar tool).
   - **OpenAI Chat Completions API (`/v1/chat/completions`)**: Complete support for `tool_calls`, `role: "tool"`, `reasoning_content`, and `chat.completion.chunk` SSE streaming.

4. **Omarchy Status Bar Integration (`arch.gemini-router`)**:
   - Quickshell bar widget (`GeminiRouter.qml`) showing live routing badge (`ADC Off`, `Claude -> G3.8`, `Codex -> G4`, `C+X -> G3.8`), session token/request telemetry, ADC health, and 1-click toggles for Claude Code, OpenAI Codex, model selection, and thinking effort.
   - Losslessly backs up and restores your pre-existing `~/.claude/settings.json` and `~/.codex/config.toml` when toggling routing on and off.

---

## Function Calling, Skills & Tool Search Support

Vertex AI's `functionDeclarations` and Gemini 3.x / 4.0 `thoughtSignature` validators enforce strict rules that standard Anthropic and OpenAI schemas violate out of the box. `claude-gemini-router` resolves all of these transparently:

| Challenge | How `claude-gemini-router` Solves It |
| :--- | :--- |
| **Complex JSON Schemas in Tools, Skills & MCP** | Recursively resolves local `$ref` pointers against `$defs` / `definitions` (with cycle detection), merges `allOf` branches, unwraps nullable `anyOf`/`oneOf` unions, converts `const` to single-element `enum`, strips unsupported keywords (`$schema`, `additionalProperties`, `exclusiveMinimum`, `default`, `propertyNames`, unsupported `format` values), ensures `OBJECT` schemas have `properties`, and filters `required` arrays to existing property keys. |
| **Gemini `thoughtSignature` Validation on Tool Calls** | Extracts `thoughtSignature` from Vertex AI `functionCall` parts, caches them by `tool_use_id` / `call_id` in `SIGNATURE_STORE`, emits synthetic `thinking` blocks with signatures for stateless clients, and replays `thoughtSignature` directly onto the `functionCall` part in subsequent turns. When a historical tool call lacks a cached signature, injects `"skip_thought_signature_validator"` so Vertex AI never rejects multi-turn conversations with `400 INVALID_ARGUMENT`. |
| **Claude Code `Skill` & `<system-reminder>` Multi-Block User Turns (Argon 4.0 Compatibility)** | When Claude Code invokes a tool or `Skill`, it sends a `user` turn containing `tool_result` along with injected `SKILL.md` prompts (`Base directory for this skill: ...`) and `<system-reminder>` token reminders. While Gemini 3.x tolerates mixed `functionResponse` + `text` parts in a single `user` turn, **Gemini 4.0 Argon (`gemini-4-argon`)** rejects mixed `functionResponse` + `text` turns with `400 INVALID_ARGUMENT: "Requests ending with a model turn are not supported."`. The router folds all accompanying `text` blocks directly into the `functionResponse.response` payload so `functionResponse` turns remain pure and 100% compatible with both `gemini-4-argon` and `gemini-3.8-flash`. |
| **Claude Code `ToolSearch` (`tool_reference` Blocks)** | Converts `{"type": "tool_reference", "tool_name": "..."}` blocks inside `tool_result` content arrays into structured text so deferred MCP/skill tool discovery works seamlessly. |
| **Claude Code Auto Mode Classifier (`safeguard_results`)** | Implements full compatibility with Claude Code's Auto Mode Classifier Billing update (`dangerous-tool-use-2026-09-03`). Emits `safeguard_results` with `{"type": "dangerous_tool_use", "status": {"type": "available", "tool_uses": {"<toolu_id>": {"type": "evaluated", "outcome": "not_flagged"}}}}` in both unary `message` responses and streaming `message_delta` SSE events so Claude Code never falls back to local classifier billing or displays gateway incompatibility warnings. |
| **Undeclared Historical Tools** | Scans conversation history for any `functionCall` / `functionResponse` names not present in the current request's `tools` list and synthesizes stub `functionDeclarations` so Vertex AI never errors on dynamically loaded or unloaded skills/tools. |

---

## Model Tier & Reasoning Effort Routing

| Client Model / Alias | Target Vertex AI Model | Fallback Behavior |
| :--- | :--- | :--- |
| `gemini-3.8`, `gemini-3.8-flash`, `claude-*-haiku*`, `claude-*-sonnet*`, `gpt-5-codex`, `gpt-5`, `o4-mini` | `gemini-3.8-flash` (or active `GEMINI_PRIMARY_MODEL`) | Falls back to `gemini-3-flash-preview` if unavailable |
| `gemini-4.0`, `gemini-4.0-pro`, `gemini-4-argon`, `claude-*-opus*`, `gpt-5-pro`, `o3-pro` | `gemini-4-argon` | Automatically falls back to `gemini-3.8-flash` on `404`/`403`/`400` if the GCP project is not allowlisted for `gemini-4-argon` |
| `gemini-3.1-pro-preview`, `gemini-2.5-pro`, `gemini-2.5-flash` | Passed through directly to Vertex AI | — |

### Reasoning Effort (`thinkingConfig.thinkingLevel`)

- Controlled via model suffix (`-low`, `-medium`, `-high`), request parameters (`reasoning.effort` / `thinking`), runtime `/config` hot-swap, or `GEMINI_REASONING_EFFORT`:
  - `low` $\rightarrow$ `thinkingLevel: "LOW"`
  - `medium` $\rightarrow$ `thinkingLevel: "MEDIUM"`
  - `high` / `xhigh` $\rightarrow$ `thinkingLevel: "HIGH"`

---

## Supported HTTP Endpoints

| Method | Path | Description |
| :--- | :--- | :--- |
| `GET` | `/health`, `/v1/health` | Returns router status, active model/effort, ADC token & project health, stripped env vars, and live request/token telemetry. |
| `GET`, `POST` | `/config`, `/v1/config` | Inspect or hot-swap `primary_model`, `reasoning_effort`, and `hot_swap` at runtime without restarting the daemon or client. |
| `GET` | `/models`, `/v1/models` | Lists available routed Gemini models (`gemini-3.8-flash`, `gemini-4-argon`, `gemini-3.1-pro-preview`, etc.). |
| `POST` | `/messages`, `/v1/messages` | Anthropic Messages API (Unary JSON & Streaming SSE) for Claude Code. |
| `POST` | `/messages/count_tokens`, `/v1/messages/count_tokens` | Token estimation endpoint required by Claude Code prior to requests. |
| `POST` | `/responses`, `/v1/responses` | OpenAI Responses API (Unary JSON & Streaming SSE) for OpenAI Codex CLI. |
| `POST` | `/chat/completions`, `/v1/chat/completions` | OpenAI Chat Completions API (Unary JSON & Streaming SSE). |

---

## Installation & Quick Start

### Prerequisites

1. **Rust Toolchain** (`cargo`, `rustc` 1.75+)
2. **Google Cloud SDK (`gcloud`)** authenticated with Application Default Credentials:
   ```bash
   gcloud auth application-default login
   gcloud config set project YOUR_GCP_PROJECT_ID
   ```

### 1. Full Installation (Daemon + CLI + Omarchy Bar Plugin)

```bash
git clone https://github.com/unrealandychan/omarchy-claude-codex-router.git
cd omarchy-claude-codex-router

./omarchy-plugin/install.sh
```

`./omarchy-plugin/install.sh` performs the following:
- Compiles the optimized release binaries (`claude-gemini-router` and `omarchy-gemini-router-ctl`).
- Installs `claude-gemini-router`, `omarchy-gemini-router-ctl`, `claude-gemini`, and `codex-gemini` into `~/.local/bin/`.
- Installs and starts the systemd user service `claude-gemini-router.service` on `http://127.0.0.1:4000`.
- Installs the Omarchy bar widget into `~/.config/omarchy/plugins/arch.gemini-router` and registers it in `~/.config/omarchy/shell.json`.

### 2. Build from Source with Cargo

```bash
cargo build --release --locked
# Binaries are produced at:
#   ./target/release/claude-gemini-router
#   ./target/release/omarchy-gemini-router-ctl
```

### 3. One-Command Wrapper Scripts

Launch Claude Code or OpenAI Codex routed through the local Rust ADC proxy without modifying your global config files:

```bash
# Launch Claude Code on Gemini 3.8 Flash (default) or Gemini 4.0 Argon
./bin/claude-gemini --3.8
./bin/claude-gemini --4.0

# Launch OpenAI Codex on Gemini 3.8 Flash (default) or Gemini 4.0 Argon
./bin/codex-gemini --3.8
./bin/codex-gemini --4.0 --effort=high
```

---

## CLI Reference (`omarchy-gemini-router-ctl`)

All `omarchy-gemini-router-ctl` commands output structured JSON:

```bash
# View full router, ADC, Claude Code, and Codex status + telemetry
omarchy-gemini-router-ctl status

# Enable routing for Claude Code (~/.claude/settings.json), Codex (~/.codex/config.toml), or both
omarchy-gemini-router-ctl enable --target both
omarchy-gemini-router-ctl enable --target claude --model gemini-4-argon
omarchy-gemini-router-ctl enable --target codex --model gemini-3.8-flash --effort high

# Toggle routing state on/off
omarchy-gemini-router-ctl toggle --target both
omarchy-gemini-router-ctl toggle --target claude
omarchy-gemini-router-ctl toggle --target codex

# Hot-swap active Gemini model & reasoning effort across daemon + client configs
omarchy-gemini-router-ctl set-model --model gemini-4-argon --effort high
omarchy-gemini-router-ctl set-model --model gemini-3.8-flash --effort medium
omarchy-gemini-router-ctl set-model --flip

# Disable routing and losslessly restore original ~/.claude/settings.json and ~/.codex/config.toml
omarchy-gemini-router-ctl disable --target both

# Manage the background router daemon
omarchy-gemini-router-ctl start-router
omarchy-gemini-router-ctl restart-router
omarchy-gemini-router-ctl stop-router
omarchy-gemini-router-ctl logs --lines 50

# Launch interactive gcloud ADC login in your terminal emulator
omarchy-gemini-router-ctl adc-login
```

---

## Router Daemon CLI & Environment Variables

```bash
claude-gemini-router [--host 127.0.0.1] [--port 4000] [--project GCP_PROJECT_ID] [--location global]
```

| Environment Variable | Default | Description |
| :--- | :--- | :--- |
| `ROUTER_HOST` | `127.0.0.1` | Bind address for the HTTP server |
| `ROUTER_PORT` | `4000` | Listen port for the HTTP server |
| `GOOGLE_CLOUD_PROJECT` | Auto-detected from ADC / `gcloud` | Google Cloud Project ID used for Vertex AI requests |
| `GOOGLE_CLOUD_LOCATION` | `global` | Vertex AI region (`global`, `us-central1`, etc.) |
| `GEMINI_PRIMARY_MODEL` | `gemini-3.8-flash` | Default target model (`gemini-3.8-flash` or `gemini-4-argon`) |
| `GEMINI_REASONING_EFFORT` | `medium` | Default thinking effort (`low`, `medium`, `high`, `xhigh`) |
| `GEMINI_HOT_SWAP_ALL` | `0` | When `1` or enabled via `/config`, overrides explicit client model tiers with `GEMINI_PRIMARY_MODEL` |

---

## Testing & Verification

Run both the Rust integration test suite and the Omarchy CLI/plugin security & functional test suite:

```bash
# 1. Run Rust unit & integration tests (ADC stripping, schema sanitizer, multi-turn tool/skill calls, SSE streaming)
cargo test

# 2. Run the 55-assertion Omarchy CTL & security test suite
./omarchy-plugin/tests/test-ctl.sh
```

---

## Project Structure

```text
omarchy-claude-codex-router/
├── Cargo.toml                         # Rust package manifest & dependencies
├── Cargo.lock                         # Locked dependency graph
├── src/
│   ├── lib.rs                         # Library exports
│   ├── adc_auth.rs                    # Keyless Google Cloud ADC credential & token manager
│   ├── translator.rs                  # Anthropic / OpenAI Chat / Codex Responses <-> Vertex AI Gemini translator
│   ├── server.rs                      # Async Tokio HTTP/1.1 + SSE router server & metrics
│   └── bin/
│       ├── claude-gemini-router.rs    # Router daemon CLI entrypoint
│       └── omarchy-gemini-router-ctl.rs # Omarchy plugin backend CLI & config manager
├── tests/
│   └── router_tests.rs                # Integration tests for schemas, tool/skill calling, streaming, and server endpoints
├── bin/
│   ├── claude-gemini                  # Keyless launcher wrapper for Claude Code
│   └── codex-gemini                   # Keyless launcher wrapper for OpenAI Codex
├── omarchy-plugin/
│   ├── manifest.json                  # Omarchy bar-widget plugin manifest (arch.gemini-router)
│   ├── install.sh                     # Builds Rust release binaries, installs systemd service & bar widget
│   ├── uninstall.sh                   # Cleanly restores configs and removes service/plugin
│   ├── plugin/
│   │   └── GeminiRouter.qml           # Quickshell status bar widget & control popup
│   └── tests/
│       └── test-ctl.sh                # 55-test functional & security test suite for omarchy-gemini-router-ctl
├── skills/
│   └── gemini-adc-router/SKILL.md     # Agent skill definition
└── commands/
    └── gemini-router.md               # Slash command definition
```

---

## Author & License

- **Author**: Eddie Chan (<eddie.chan@connect.ust.hk>)
- **Repository**: [https://github.com/unrealandychan/omarchy-claude-codex-router](https://github.com/unrealandychan/omarchy-claude-codex-router)
- **License**: [MIT](LICENSE)
