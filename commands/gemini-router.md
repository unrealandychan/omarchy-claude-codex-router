---
description: Check status or switch models on the Vertex AI Gemini ADC Router for Claude Code & Codex
allowed-tools: Bash(omarchy-gemini-router-ctl:*)
---

Inspect or update the local Vertex AI Gemini ADC Router (`http://127.0.0.1:4000`):

1. Run `omarchy-gemini-router-ctl status` to check ADC authentication health, GCP project ID, and active model routing.
2. If the user passed arguments (`$ARGUMENTS`), update the model (`gemini-3.8-flash` or `gemini-4-argon`) or routing target (`claude`, `codex`, `both`, `off`) accordingly via `omarchy-gemini-router-ctl`.
