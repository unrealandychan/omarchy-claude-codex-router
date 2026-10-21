---
name: gemini-adc-router
description: Manage, inspect, and configure the keyless (ADC-only) Claude Code and OpenAI Codex router targeting Vertex AI Gemini 3.8 Flash and Gemini 4.0 Argon, or control the Omarchy Gemini Router plugin.
---

# Gemini ADC Router Skill (`gemini-adc-router`)

Use this skill when the user wants to:
- Check the health, active GCP project, ADC token status, or active Gemini model of the local Claude/Codex -> Vertex AI Gemini ADC router (`http://127.0.0.1:4000`).
- Switch the routed model between **Gemini 3.8 Flash** (`gemini-3.8-flash`) and **Gemini 4.0 Argon** (`gemini-4-argon`) or adjust reasoning effort (`low`, `medium`, `high`, `xhigh`).
- Enable or disable routing for **Claude Code** (`~/.claude/settings.json`) or **OpenAI Codex** (`~/.codex/config.toml`) via the Omarchy plugin CLI (`omarchy-gemini-router-ctl`).

## Quick Commands

### 1. Check Router & ADC Health
```bash
curl -fsS http://127.0.0.1:4000/health | python3 -m json.tool
```

### 2. Switch Active Model or Reasoning Effort at Runtime
```bash
# Switch to Gemini 4.0 (gemini-4-argon) with high reasoning effort
curl -fsS -X POST http://127.0.0.1:4000/config \
  -H "Content-Type: application/json" \
  -d '{"primary_model": "gemini-4-argon", "reasoning_effort": "high"}'

# Switch to Gemini 3.8 Flash (gemini-3.8-flash) with medium reasoning effort
curl -fsS -X POST http://127.0.0.1:4000/config \
  -H "Content-Type: application/json" \
  -d '{"primary_model": "gemini-3.8-flash", "reasoning_effort": "medium"}'
```

### 3. Control Routing via Omarchy Plugin CLI (`omarchy-gemini-router-ctl`)
```bash
# Full status (ADC, router daemon, Claude routing, Codex routing, active model)
omarchy-gemini-router-ctl status

# Enable / disable routing for Claude Code, OpenAI Codex, or both
omarchy-gemini-router-ctl enable --target both
omarchy-gemini-router-ctl enable --target claude
omarchy-gemini-router-ctl enable --target codex
omarchy-gemini-router-ctl disable --target both

# Switch model & reasoning effort across router + Claude + Codex configs
omarchy-gemini-router-ctl set-model --model gemini-3.8-flash --effort medium
omarchy-gemini-router-ctl set-model --model gemini-4-argon --effort high
```
