# Omarchy Gemini ADC Router Plugin (`arch.gemini-router`)

An Omarchy status bar widget (`Quickshell` / `BarWidget`) and CLI (`omarchy-gemini-router-ctl`) that enables 1-click routing of **Claude Code** (`claude`) and **OpenAI Codex** (`codex`) to **Vertex AI Gemini 3.8 Flash** (`gemini-3.8-flash`) and **Gemini 4.0 Argon** (`gemini-4-argon`) using **Application Default Credentials (ADC)** — with **zero API keys**.

## Features

- **Independent or Unified Routing Toggles**:
  - **Claude Code (`~/.claude/settings.json`)**: Toggles `ANTHROPIC_BASE_URL="http://127.0.0.1:4000"` + `ANTHROPIC_AUTH_TOKEN="adc-keyless"` and model tier mappings while preserving all existing hooks, themes, and backing up your prior `env` settings for lossless restore.
  - **OpenAI Codex (`~/.codex/config.toml`)**: Toggles `model_provider = "gemini_adc"` and `[model_providers.gemini_adc]` (`wire_api = "responses"`, `base_url = "http://127.0.0.1:4000/v1"`) while preserving all existing `[features]`, `[tui]`, and `[hooks]` sections.
  - **Route Both / Disable All**: 1-click master switch in the panel header.
- **Live Model & Reasoning Effort Selector**:
  - Switch between **Gemini 3.8 Flash** (`gemini-3.8-flash`) and **Gemini 4.0 Argon** (`gemini-4-argon`).
  - Adjust Gemini thinking level / reasoning effort (`LOW`, `MEDIUM`, `HIGH`, `XHIGH`).
- **Router Daemon & ADC Health Monitor**:
  - Start, stop, or restart `claude-gemini-router.service` (`http://127.0.0.1:4000`).
  - Inspect live Google Cloud ADC token validity, GCP project ID, location, and keyless verification (`api_key_used: false`).
  - Launch `gcloud auth application-default login` with 1 click if ADC credentials expire.

## Installation

```bash
./install.sh
```

## CLI Usage (`omarchy-gemini-router-ctl`)

```bash
# View full JSON status
omarchy-gemini-router-ctl status

# Enable routing for Claude Code, OpenAI Codex, or both
omarchy-gemini-router-ctl enable --target both
omarchy-gemini-router-ctl enable --target claude
omarchy-gemini-router-ctl enable --target codex

# Switch active Gemini model and reasoning effort
omarchy-gemini-router-ctl set-model --model gemini-4-argon --effort high
omarchy-gemini-router-ctl set-model --model gemini-3.8-flash --effort medium

# Disable routing and cleanly restore original ~/.claude/settings.json and ~/.codex/config.toml
omarchy-gemini-router-ctl disable --target both

# Run automated test suite
./tests/test-ctl.sh
```
