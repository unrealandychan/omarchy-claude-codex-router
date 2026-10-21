# Claude Code & OpenAI Codex $\rightarrow$ Gemini 3.8 & Gemini 4.0 Router (ADC-Only, Zero API Keys)

A lightweight, keyless proxy and plugin suite that translates **Claude Code**'s Anthropic Messages API (`/v1/messages`) and **OpenAI Codex**'s Responses & Chat Completions APIs (`/v1/responses`, `/v1/chat/completions`) into **Google Cloud Vertex AI** calls for **Gemini 3.8** (`gemini-3.8-flash`) and **Gemini 4.0** (`gemini-4-argon`) using **Application Default Credentials (ADC)**.

Includes an **Omarchy Status Bar Plugin** (`arch.gemini-router`) to toggle Claude Code and Codex routing, switch Gemini models/thinking effort, and monitor ADC health with 1 click.

## Key Features & Handled Gotchas

1. **Zero API Keys (Strict ADC Enforcement)**:
   - Strips `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `ANTHROPIC_API_KEY`, and `OPENAI_API_KEY` at startup.
   - Exchanges the refresh token from `~/.config/gcloud/application_default_credentials.json` (and sets `GOOGLE_API_USE_CLIENT_CERTIFICATE=false` so Cloudtop ECP never mints a Certificate-Bound `CBA/mTLS` token that fails over standard HTTPS).
2. **Multi-Protocol Translation (Claude Code + OpenAI Codex)**:
   - **Anthropic Messages API (`/v1/messages`, `/v1/messages/count_tokens`)**: Full support for Claude Code tool calls, thinking blocks, and SSE streaming.
   - **OpenAI Responses API (`/v1/responses`)**: Native wire protocol support for OpenAI Codex CLI (`codex`), including `function_call`, `function_call_output`, `custom_tool_call` (`apply_patch`), `reasoning` items (`encrypted_content` signature preservation), and `response.*` SSE streaming.
   - **OpenAI Chat Completions API (`/v1/chat/completions`)**: Full tool-calling and SSE streaming compatibility.
3. **Model Tier & Alias Routing**:
   - `gemini-3.8`, `gemini-3.8-flash`, `claude-*-haiku*`, `claude-*-sonnet*`, `gpt-5-codex`, `gpt-5`, `o4-mini` $\rightarrow$ **`gemini-3.8-flash`** (or active `GEMINI_PRIMARY_MODEL`)
   - `gemini-4.0`, `gemini-4.0-pro`, `gemini-4-argon`, `claude-*-opus*`, `gpt-5-pro`, `o3-pro` $\rightarrow$ **`gemini-4-argon`** (with automatic fallback to `gemini-3.8-flash` if the active GCP project isn't allowlisted for `gemini-4-argon`).
   - Effort suffixes (`-low`, `-medium`, `-high`) and `GEMINI_REASONING_EFFORT` map directly to Gemini's `thinkingConfig.thinkingLevel`.
4. **Schema Sanitizer & `thoughtSignature` Replay**:
   - Strips `$schema`, `additionalProperties`, `exclusiveMinimum`, `default`, and normalizes nullable union types (`["integer", "null"]`) so Vertex AI `functionDeclarations` never throw `400 INVALID_ARGUMENT`.
   - Caches and replays Gemini 3.8 / 4.0 `thoughtSignature` tokens across multi-turn tool call chains for both Claude Code and Codex.

## Quick Start

### 1. One-Command Launch with Wrapper Scripts

```bash
git clone https://github.com/eddieckc/claude-gemini-adc-router.git
cd claude-gemini-adc-router

# Launch Claude Code on Gemini 3.8 Flash (default) or Gemini 4.0 Argon
./bin/claude-gemini --3.8
./bin/claude-gemini --4.0

# Launch OpenAI Codex on Gemini 3.8 Flash (default) or Gemini 4.0 Argon
./bin/codex-gemini --3.8
./bin/codex-gemini --4.0
```

### 2. Install as a Claude Code / Codex Plugin

```bash
# Install in Claude Code (v2.1.275+)
/plugin install gemini-adc-router --marketplace eddieckc/claude-gemini-adc-router
```

### 3. Install the Omarchy Status Bar Plugin (`arch.gemini-router`)

```bash
./omarchy-plugin/install.sh

# Control routing from the CLI or from the Omarchy top bar widget:
omarchy-gemini-router-ctl status
omarchy-gemini-router-ctl enable --target both
omarchy-gemini-router-ctl set-model --model gemini-4-argon --effort high
omarchy-gemini-router-ctl disable --target both
```

## License

[MIT](LICENSE)

