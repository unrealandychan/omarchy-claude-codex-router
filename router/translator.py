"""Protocol translation between Anthropic (/v1/messages), OpenAI (/v1/responses, /v1/chat/completions), and Vertex AI Gemini.

Handles:
1. Model tier & alias mapping (Claude Code, OpenAI Codex, Gemini 3.8 Flash, Gemini 4.0 Argon)
2. Recursive JSON Schema sanitization for Claude Code, Codex, & MCP tools
3. Prompt caching (`cache_control`) stripping
4. `thoughtSignature` caching and replay across multi-turn tool call chains (Anthropic & OpenAI)
5. Thinking budget / reasoning effort -> Gemini 3.8 / 4.0 `thinkingConfig` (`thinkingLevel`)
6. Unary JSON and Streaming SSE translation for:
   - Anthropic Messages API (`/v1/messages`)
   - OpenAI Responses API (`/v1/responses` - used natively by Codex CLI)
   - OpenAI Chat Completions API (`/v1/chat/completions`)
"""

from __future__ import annotations

from collections import OrderedDict
from collections.abc import AsyncIterator, Callable
import json
import os
import time
import uuid
from typing import Any

# Publisher model IDs on Vertex AI (and internal GenAI proxy)
MODEL_GEMINI_3_8_FLASH = "gemini-3.8-flash"
MODEL_GEMINI_4_ARGON = "gemini-4-argon"

# Strip JSON Schema keywords that Vertex AI Gemini functionDeclarations reject
UNSUPPORTED_SCHEMA_KEYS = frozenset(
    {
        "$schema",
        "$id",
        "$ref",
        "$defs",
        "definitions",
        "additionalProperties",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "default",
        "examples",
        "propertyNames",
        "patternProperties",
        "const",
        "title",
        "cache_control",
        "strict",
    }
)

VALID_GEMINI_TYPES = frozenset(
    {"STRING", "NUMBER", "INTEGER", "BOOLEAN", "ARRAY", "OBJECT"}
)


class ThoughtSignatureStore:
    """In-memory LRU cache mapping tool_use_id / call_id -> Gemini thoughtSignature & tool name."""

    def __init__(self, max_size: int = 2048) -> None:
        self._store: OrderedDict[str, str] = OrderedDict()
        self._tool_names: OrderedDict[str, str] = OrderedDict()
        self._response_contents: OrderedDict[str, list[dict[str, Any]]] = OrderedDict()
        self._max_size = max_size
        self._latest_signature: str | None = None

    def save(
        self,
        tool_use_id: str,
        signature: str | None,
        tool_name: str | None = None,
    ) -> None:
        if tool_name and tool_use_id:
            if tool_use_id in self._tool_names:
                self._tool_names.move_to_end(tool_use_id)
            self._tool_names[tool_use_id] = tool_name
            while len(self._tool_names) > self._max_size:
                self._tool_names.popitem(last=False)

        if not signature:
            return
        self._latest_signature = signature
        if tool_use_id in self._store:
            self._store.move_to_end(tool_use_id)
        self._store[tool_use_id] = signature
        while len(self._store) > self._max_size:
            self._store.popitem(last=False)

    def save_tool_name(self, tool_use_id: str, tool_name: str) -> None:
        if not tool_use_id or not tool_name:
            return
        if tool_use_id in self._tool_names:
            self._tool_names.move_to_end(tool_use_id)
        self._tool_names[tool_use_id] = tool_name
        while len(self._tool_names) > self._max_size:
            self._tool_names.popitem(last=False)

    def get(self, tool_use_id: str) -> str | None:
        sig = self._store.get(tool_use_id)
        if sig:
            self._store.move_to_end(tool_use_id)
        return sig

    def get_tool_name(self, tool_use_id: str) -> str | None:
        name = self._tool_names.get(tool_use_id)
        if name:
            self._tool_names.move_to_end(tool_use_id)
        return name

    def save_response_contents(
        self, response_id: str, contents: list[dict[str, Any]]
    ) -> None:
        if not response_id:
            return
        if response_id in self._response_contents:
            self._response_contents.move_to_end(response_id)
        self._response_contents[response_id] = contents
        while len(self._response_contents) > 256:
            self._response_contents.popitem(last=False)

    def get_response_contents(self, response_id: str) -> list[dict[str, Any]] | None:
        return self._response_contents.get(response_id)

    @property
    def latest(self) -> str | None:
        return self._latest_signature


SIGNATURE_STORE = ThoughtSignatureStore()


def is_fast_background_model(requested_model: str | None) -> bool:
    """Return True if the model name represents a Haiku / small-fast background task."""
    if not requested_model:
        return False
    lowered = requested_model.strip().lower()
    return "haiku" in lowered or "small-fast" in lowered or lowered.endswith("-fast")


def resolve_gemini_model(requested_model: str | None) -> tuple[str, str | None]:
    """Map an incoming Anthropic, OpenAI Codex, or Gemini model alias to (vertex_model_id, thinking_level).

    Behavior:
      - Haiku / small-fast background models always route to `gemini-3.8-flash`.
      - When `GEMINI_HOT_SWAP_ACTIVE=1` (triggered via Omarchy widget `/config`), all main
        agent turns (Claude Opus/Sonnet/default and Codex) hot-swap immediately to the
        active `GEMINI_PRIMARY_MODEL`.
      - When `GEMINI_PRIMARY_MODEL` is explicitly set, both Opus and Sonnet main tiers
        follow `GEMINI_PRIMARY_MODEL` while Haiku remains on `gemini-3.8-flash`.
      - Explicit `-minimal`, `-low`, `-medium`, `-high`, `-xhigh`, `-max` suffixes set
        the per-request thinking level.
    """
    explicit_primary_env = os.environ.get("GEMINI_PRIMARY_MODEL", "").strip().lower()
    hot_swap_active = os.environ.get("GEMINI_HOT_SWAP_ACTIVE", "").strip() == "1"

    if explicit_primary_env in (
        "gemini-4",
        "gemini-4.0",
        "gemini-4.0-pro",
        "gemini-4-pro",
        "gemini-4-argon",
        "argon",
    ):
        default_vertex_model = MODEL_GEMINI_4_ARGON
    else:
        default_vertex_model = MODEL_GEMINI_3_8_FLASH

    raw = (requested_model or default_vertex_model).strip().lower()
    if "[" in raw:
        raw = raw.split("[", 1)[0].strip()

    effort_level: str | None = None
    for suffix, level in (
        ("-minimal", "MINIMAL"),
        ("-low", "LOW"),
        ("-medium", "MEDIUM"),
        ("-xhigh", "HIGH"),
        ("-high", "HIGH"),
        ("-max", "HIGH"),
    ):
        if raw.endswith(suffix):
            effort_level = level
            raw = raw[: -len(suffix)]
            break

    # 1. Haiku / small-fast background calls ALWAYS stay on Gemini 3.8 Flash
    if is_fast_background_model(raw):
        return MODEL_GEMINI_3_8_FLASH, effort_level

    # 2. Live hot-swap mode: when toggled in Omarchy widget, main turns immediately
    #    use the active primary model even in already-running Claude/Codex sessions.
    if hot_swap_active and explicit_primary_env:
        return default_vertex_model, effort_level

    # 3. Claude Opus / Sonnet main work tiers:
    #    Follow GEMINI_PRIMARY_MODEL when explicitly configured; otherwise default
    #    Opus -> 4.0 Argon and Sonnet -> 3.8 Flash.
    if "sonnet" in raw:
        return default_vertex_model, effort_level
    if "opus" in raw:
        if explicit_primary_env:
            return default_vertex_model, effort_level
        return MODEL_GEMINI_4_ARGON, effort_level

    # 4. Explicit Gemini 4.0 / Argon aliases + OpenAI/Codex Pro/Max tiers
    if (
        raw
        in (
            "gemini-4",
            "gemini-4.0",
            "gemini-4.0-pro",
            "gemini-4-pro",
            "gemini-4-argon",
            "argon",
            "gpt-5-pro",
            "gpt-5.4-pro",
            "o3-pro",
            "o1-pro",
            "codex-max",
            "gpt-5-codex-max",
        )
        or raw.startswith("gemini-4")
        or raw.endswith("-argon")
    ):
        return MODEL_GEMINI_4_ARGON, effort_level

    # 5. Explicit Gemini 3.8 Flash aliases
    if raw in ("gemini-3.8", "gemini-3.8-flash", "flash") or raw.startswith("gemini-3.8"):
        return MODEL_GEMINI_3_8_FLASH, effort_level

    # 6. Allow explicit passthrough for other Gemini models (e.g. gemini-3.1-pro-preview)
    if raw.startswith("gemini-"):
        return raw, effort_level

    # 7. OpenAI / Codex generic models ("gpt-5", "gpt-5-codex", "o3", "o4-mini", "codex-mini-latest", etc.)
    return default_vertex_model, effort_level


def sanitize_json_schema(schema: Any) -> dict[str, Any]:
    """Recursively sanitize a JSON Schema for Vertex AI Gemini `functionDeclarations`."""
    if not isinstance(schema, dict):
        return {"type": "OBJECT", "properties": {}}

    cleaned: dict[str, Any] = {}
    nullable = False

    # Handle union types such as ["string", "null"]
    raw_type = schema.get("type")
    if isinstance(raw_type, list):
        non_null = [t for t in raw_type if t != "null"]
        if "null" in raw_type:
            nullable = True
        raw_type = non_null[0] if non_null else "string"

    if isinstance(raw_type, str):
        upper_type = raw_type.upper()
        if upper_type in VALID_GEMINI_TYPES:
            cleaned["type"] = upper_type
        else:
            cleaned["type"] = "STRING"

    if nullable or schema.get("nullable") is True:
        cleaned["nullable"] = True

    if "description" in schema and isinstance(schema["description"], str):
        cleaned["description"] = schema["description"]

    if "enum" in schema and isinstance(schema["enum"], list):
        cleaned["enum"] = [str(v) for v in schema["enum"] if v is not None]
        cleaned["type"] = "STRING"

    # Handle anyOf / oneOf by picking first non-null branch
    for union_key in ("anyOf", "oneOf"):
        if union_key in schema and isinstance(schema[union_key], list):
            branches = [
                b
                for b in schema[union_key]
                if isinstance(b, dict) and b.get("type") != "null"
            ]
            if branches:
                merged = sanitize_json_schema(branches[0])
                if "description" in cleaned and "description" not in merged:
                    merged["description"] = cleaned["description"]
                return merged

    if "properties" in schema and isinstance(schema["properties"], dict):
        cleaned["type"] = "OBJECT"
        props: dict[str, Any] = {}
        for prop_name, prop_schema in schema["properties"].items():
            props[prop_name] = sanitize_json_schema(prop_schema)
        cleaned["properties"] = props

    if "required" in schema and isinstance(schema["required"], list):
        valid_props = set(cleaned.get("properties", {}).keys())
        req_list = [r for r in schema["required"] if isinstance(r, str) and r in valid_props]
        if req_list:
            cleaned["required"] = req_list

    if "items" in schema:
        cleaned["type"] = "ARRAY"
        if isinstance(schema["items"], dict):
            cleaned["items"] = sanitize_json_schema(schema["items"])
        elif isinstance(schema["items"], list) and schema["items"]:
            cleaned["items"] = sanitize_json_schema(schema["items"][0])
        else:
            cleaned["items"] = {"type": "STRING"}

    if cleaned.get("type") == "OBJECT" and "properties" not in cleaned:
        cleaned["properties"] = {}

    if "type" not in cleaned:
        cleaned["type"] = "OBJECT" if "properties" in cleaned else "STRING"

    return cleaned


def _extract_system_instruction(system_field: Any) -> dict[str, Any] | None:
    """Convert Anthropic `system` field into Gemini `systemInstruction`."""
    if not system_field:
        return None
    if isinstance(system_field, str):
        text = system_field.strip()
        return {"parts": [{"text": text}]} if text else None
    if isinstance(system_field, list):
        parts: list[dict[str, Any]] = []
        for block in system_field:
            if isinstance(block, str) and block.strip():
                parts.append({"text": block})
            elif isinstance(block, dict) and block.get("type") in ("text", "input_text"):
                txt = block.get("text", "")
                if txt:
                    parts.append({"text": txt})
        return {"parts": parts} if parts else None
    return None


def _budget_to_thinking_level(budget_tokens: int | None) -> str:
    """Map Anthropic thinking budget_tokens to Gemini 3.8 / 4.0 ThinkingLevel."""
    if budget_tokens is None:
        return "MEDIUM"
    if budget_tokens <= 0:
        return "MINIMAL"
    if budget_tokens <= 4096:
        return "LOW"
    if budget_tokens <= 16384:
        return "MEDIUM"
    return "HIGH"


def _effort_str_to_thinking_level(effort: str | None) -> str | None:
    """Map OpenAI/Omarchy reasoning_effort ('minimal', 'low', 'medium', 'high', 'xhigh', 'max') to Gemini ThinkingLevel."""
    if not effort or not isinstance(effort, str):
        return None
    norm = effort.strip().upper()
    if norm in ("MINIMAL", "LOW", "MEDIUM", "HIGH"):
        return norm
    if norm in ("XHIGH", "MAX"):
        return "HIGH"
    if norm == "NONE":
        return "MINIMAL"
    return "MEDIUM"


def _parse_data_url_image(url: str) -> dict[str, Any] | None:
    """Convert a `data:image/...;base64,...` URL into a Gemini `inlineData` part."""
    if not isinstance(url, str) or not url.startswith("data:"):
        return None
    try:
        header, b64_data = url.split(",", 1)
        mime = header[5:].split(";")[0] or "image/png"
        return {"inlineData": {"mimeType": mime, "data": b64_data}}
    except Exception:
        return None


def _safe_json_loads_args(raw_args: Any) -> dict[str, Any]:
    """Parse tool call arguments from string or dict into a dict."""
    if isinstance(raw_args, dict):
        return raw_args
    if isinstance(raw_args, str) and raw_args.strip():
        try:
            parsed = json.loads(raw_args)
            if isinstance(parsed, dict):
                return parsed
            return {"value": parsed}
        except Exception:
            return {"raw": raw_args}
    return {}


def _normalize_gemini_contents(contents: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Ensure Gemini `contents` strictly satisfies Vertex AI turn-ordering invariants.

    Vertex AI Gemini 3.8 / 4.0 rejects requests with HTTP 400 INVALID_ARGUMENT when:
    1. `contents` is empty or starts with a `model` turn.
    2. `contents` ends with a `model` turn ("Requests ending with a model turn are not supported.").
       This occurs in Claude Code when using assistant prefill, continuing after a thinking-only
       or interrupted assistant turn, or when a trailing user message had empty blocks.
    3. A `model` turn containing `functionCall` parts is not immediately followed by a `user`
       turn with matching `functionResponse` parts (e.g. after an interrupted tool execution).
    """
    merged: list[dict[str, Any]] = []
    for turn in contents:
        if not isinstance(turn, dict):
            continue
        role = "model" if turn.get("role") == "model" else "user"
        parts = [p for p in (turn.get("parts") or []) if isinstance(p, dict)]
        if not parts:
            continue
        if merged and merged[-1]["role"] == role:
            merged[-1]["parts"].extend(parts)
        else:
            merged.append({"role": role, "parts": list(parts)})

    if not merged:
        return [{"role": "user", "parts": [{"text": "Hello"}]}]

    if merged[0]["role"] == "model":
        merged.insert(0, {"role": "user", "parts": [{"text": "Continue."}]})

    normalized: list[dict[str, Any]] = []
    i = 0
    while i < len(merged):
        turn = merged[i]
        normalized.append(turn)
        if turn["role"] == "model":
            fc_parts = [
                p["functionCall"]
                for p in turn["parts"]
                if isinstance(p.get("functionCall"), dict)
            ]
            if fc_parts:
                if i + 1 < len(merged) and merged[i + 1]["role"] == "user":
                    next_parts = list(merged[i + 1]["parts"])
                    existing_fr_count = sum(
                        1 for p in next_parts if "functionResponse" in p
                    )
                    if existing_fr_count < len(fc_parts):
                        missing_frs = [
                            {
                                "functionResponse": {
                                    "name": str(fc.get("name") or "tool"),
                                    "response": {
                                        "result": "Tool execution was skipped or interrupted by user. Please continue.",
                                        "is_error": True,
                                    },
                                }
                            }
                            for fc in fc_parts[existing_fr_count:]
                        ]
                        merged[i + 1] = {
                            "role": "user",
                            "parts": missing_frs + next_parts,
                        }
                else:
                    synthetic_frs = [
                        {
                            "functionResponse": {
                                "name": str(fc.get("name") or "tool"),
                                "response": {
                                    "result": "Tool execution was skipped or interrupted by user. Please continue.",
                                    "is_error": True,
                                },
                            }
                        }
                        for fc in fc_parts
                    ]
                    normalized.append({"role": "user", "parts": synthetic_frs})
        i += 1

    if normalized[-1]["role"] == "model":
        last_parts = normalized[-1]["parts"]
        has_thought = any(p.get("thought") is True for p in last_parts)
        has_non_thought_text = any(
            not p.get("thought") and bool(p.get("text")) for p in last_parts
        )
        if has_thought and not has_non_thought_text:
            prompt = (
                "Please continue from your thinking above and provide your complete "
                "response or tool call."
            )
        else:
            prompt = (
                "Please continue directly from where the assistant message above left off."
            )
        normalized.append({"role": "user", "parts": [{"text": prompt}]})

    return normalized


def _apply_thinking_token_headroom(gen_config: dict[str, Any]) -> None:
    """Ensure `maxOutputTokens` includes headroom for Gemini `thoughtsTokenCount` when thinking is enabled.

    In Vertex AI Gemini 3.8 / 4.0, `maxOutputTokens` caps the sum of `thoughtsTokenCount` +
    `candidatesTokenCount`. Without headroom, a small client `max_tokens` (e.g. 1024-4096) with
    `HIGH` thinking can exhaust `maxOutputTokens` mid-thought and produce zero visible text/tool tokens.
    """
    thinking_cfg = gen_config.get("thinkingConfig")
    max_out = gen_config.get("maxOutputTokens")
    if not isinstance(thinking_cfg, dict) or not isinstance(max_out, int):
        return
    level = str(thinking_cfg.get("thinkingLevel") or "").upper()
    headroom_map = {
        "MINIMAL": 1024,
        "LOW": 4096,
        "MEDIUM": 8192,
        "HIGH": 16384,
    }
    headroom = headroom_map.get(level, 8192)
    gen_config["maxOutputTokens"] = min(max_out + headroom, 65535)


# ============================================================================
# 1. Anthropic Messages API (/v1/messages) Translation
# ============================================================================


def anthropic_to_gemini_request(
    payload: dict[str, Any],
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
) -> tuple[str, dict[str, Any]]:
    """Translate an Anthropic `/v1/messages` request dict into `(gemini_model, gemini_body)`."""
    gemini_model, effort_level = resolve_gemini_model(payload.get("model"))
    gemini_body: dict[str, Any] = {}

    # 1. System Instruction (strips cache_control automatically)
    sys_inst = _extract_system_instruction(payload.get("system"))
    if sys_inst:
        gemini_body["systemInstruction"] = sys_inst

    # 2. First pass over messages: map tool_use_id -> tool_name for tool_result pairing
    tool_id_to_name: dict[str, str] = {}
    messages = payload.get("messages") or []
    for msg in messages:
        content = msg.get("content")
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    tid = block.get("id")
                    tname = block.get("name")
                    if tid and tname:
                        tool_id_to_name[str(tid)] = str(tname)
                        signature_store.save_tool_name(str(tid), str(tname))

    # 3. Second pass: convert messages to Gemini contents and merge consecutive same-role turns
    contents: list[dict[str, Any]] = []
    for msg in messages:
        role = "model" if msg.get("role") == "assistant" else "user"
        content = msg.get("content")
        parts: list[dict[str, Any]] = []
        turn_signature: str | None = None

        if isinstance(content, str):
            if content:
                parts.append({"text": content})
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, str):
                    if block:
                        parts.append({"text": block})
                    continue
                if not isinstance(block, dict):
                    continue
                btype = block.get("type")

                if btype in ("text", "input_text"):
                    txt = block.get("text", "")
                    if txt:
                        parts.append({"text": str(txt)})

                elif btype in ("thinking", "redacted_thinking"):
                    thinking_text = block.get("thinking", "")
                    sig = block.get("signature") or block.get("data")
                    if isinstance(sig, str) and sig:
                        turn_signature = sig
                    part: dict[str, Any] = {"thought": True, "text": thinking_text or " "}
                    if turn_signature:
                        part["thoughtSignature"] = turn_signature
                    parts.append(part)

                elif btype in ("image", "document"):
                    source = block.get("source") or {}
                    if source.get("type") == "base64" and source.get("data"):
                        default_mime = "application/pdf" if btype == "document" else "image/png"
                        parts.append(
                            {
                                "inlineData": {
                                    "mimeType": source.get("media_type", default_mime),
                                    "data": source["data"],
                                }
                            }
                        )
                    elif source.get("type") == "text" and source.get("data"):
                        parts.append({"text": str(source["data"])})
                    elif block.get("text"):
                        parts.append({"text": str(block["text"])})

                elif btype == "tool_use":
                    tid = str(block.get("id") or f"toolu_{uuid.uuid4().hex[:12]}")
                    tname = str(block.get("name") or "tool")
                    tinput = block.get("input")
                    if not isinstance(tinput, dict):
                        tinput = {}

                    fc_part: dict[str, Any] = {
                        "functionCall": {
                            "name": tname,
                            "args": tinput,
                        }
                    }
                    # Restore thoughtSignature required by Gemini 3.8 / 4.0 on functionCall parts
                    sig = (
                        signature_store.get(tid)
                        or turn_signature
                        or signature_store.latest
                    )
                    if sig:
                        fc_part["thoughtSignature"] = sig
                    parts.append(fc_part)

                elif btype == "tool_result":
                    tid = str(block.get("tool_use_id") or "")
                    tname = (
                        tool_id_to_name.get(tid)
                        or signature_store.get_tool_name(tid)
                        or "tool"
                    )
                    raw_res = block.get("content", "")
                    if isinstance(raw_res, list):
                        extracted: list[str] = []
                        for item in raw_res:
                            if isinstance(item, dict) and item.get("type") == "text":
                                extracted.append(str(item.get("text", "")))
                            else:
                                extracted.append(json.dumps(item))
                        res_text = "\n".join(extracted)
                    elif isinstance(raw_res, str):
                        res_text = raw_res
                    else:
                        res_text = json.dumps(raw_res)

                    parts.append(
                        {
                            "functionResponse": {
                                "name": tname,
                                "response": {
                                    "result": res_text,
                                    "is_error": bool(block.get("is_error", False)),
                                },
                            }
                        }
                    )

        if not parts and role == "user":
            parts.append({"text": "Continue."})
        if not parts:
            continue

        if contents and contents[-1]["role"] == role:
            contents[-1]["parts"].extend(parts)
        else:
            contents.append({"role": role, "parts": parts})

    gemini_body["contents"] = _normalize_gemini_contents(contents)

    # 4. Tools & Tool Choice
    tools = payload.get("tools")
    if isinstance(tools, list) and tools:
        func_decls: list[dict[str, Any]] = []
        for t in tools:
            if not isinstance(t, dict):
                continue
            name = t.get("name")
            if not name:
                continue
            input_schema = t.get("input_schema") or t.get("parameters") or {"type": "object"}
            func_decls.append(
                {
                    "name": str(name),
                    "description": str(t.get("description") or f"Tool {name}"),
                    "parameters": sanitize_json_schema(input_schema),
                }
            )
        if func_decls:
            gemini_body["tools"] = [{"functionDeclarations": func_decls}]

    tool_choice = payload.get("tool_choice")
    if isinstance(tool_choice, dict) and "tools" in gemini_body:
        tc_type = tool_choice.get("type")
        if tc_type == "any":
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "ANY"}}
        elif tc_type == "none":
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "NONE"}}
        elif tc_type == "tool" and tool_choice.get("name"):
            gemini_body["toolConfig"] = {
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [str(tool_choice["name"])],
                }
            }

    # 5. GenerationConfig & ThinkingConfig (Gemini 3.8 / 4.0)
    gen_config: dict[str, Any] = {"temperature": 1.0}
    max_tokens = payload.get("max_tokens")
    if isinstance(max_tokens, int) and max_tokens > 0:
        gen_config["maxOutputTokens"] = min(max_tokens, 65535)

    stop_seqs = payload.get("stop_sequences")
    if isinstance(stop_seqs, list) and stop_seqs:
        gen_config["stopSequences"] = [str(s) for s in stop_seqs[:5]]

    env_effort = _effort_str_to_thinking_level(os.environ.get("GEMINI_REASONING_EFFORT"))
    hot_swap_active = os.environ.get("GEMINI_HOT_SWAP_ACTIVE", "").strip() == "1"
    is_fast_bg = is_fast_background_model(payload.get("model"))
    thinking_cfg = payload.get("thinking")
    thinking_disabled = isinstance(thinking_cfg, dict) and thinking_cfg.get("type") == "disabled"
    thinking_enabled = isinstance(thinking_cfg, dict) and thinking_cfg.get("type") == "enabled"

    if not thinking_disabled:
        if thinking_enabled:
            if effort_level:
                level = effort_level
            elif hot_swap_active and env_effort:
                level = env_effort
            elif thinking_cfg.get("budget_tokens") is not None:
                level = _budget_to_thinking_level(thinking_cfg.get("budget_tokens"))
            else:
                level = env_effort or "MEDIUM"
            gen_config["thinkingConfig"] = {
                "includeThoughts": True,
                "thinkingLevel": level,
            }
        elif not is_fast_bg and (effort_level or env_effort):
            gen_config["thinkingConfig"] = {
                "includeThoughts": True,
                "thinkingLevel": effort_level or env_effort,
            }
        elif is_fast_bg and effort_level:
            gen_config["thinkingConfig"] = {
                "includeThoughts": True,
                "thinkingLevel": effort_level,
            }

    _apply_thinking_token_headroom(gen_config)
    gemini_body["generationConfig"] = gen_config
    return gemini_model, gemini_body


def gemini_to_anthropic_response(
    gemini_resp: dict[str, Any],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
) -> dict[str, Any]:
    """Convert a Vertex AI Gemini GenerateContentResponse into an Anthropic Message object."""
    candidates = gemini_resp.get("candidates") or []
    parts = (
        candidates[0].get("content", {}).get("parts", [])
        if candidates
        else []
    )

    content_blocks: list[dict[str, Any]] = []
    has_tool_use = False
    latest_sig: str | None = None

    for part in parts:
        if not isinstance(part, dict):
            continue
        sig = part.get("thoughtSignature")
        if isinstance(sig, str) and sig:
            latest_sig = sig

        if part.get("thought") is True:
            thinking_block: dict[str, Any] = {
                "type": "thinking",
                "thinking": part.get("text", ""),
            }
            if latest_sig:
                thinking_block["signature"] = latest_sig
            content_blocks.append(thinking_block)

        elif "functionCall" in part:
            has_tool_use = True
            fc = part["functionCall"]
            tool_id = str(fc.get("id") or f"toolu_{uuid.uuid4().hex[:16]}")
            tool_name = str(fc.get("name", "tool"))
            signature_store.save(tool_id, latest_sig, tool_name=tool_name)
            content_blocks.append(
                {
                    "type": "tool_use",
                    "id": tool_id,
                    "name": tool_name,
                    "input": fc.get("args") or {},
                }
            )

        elif "text" in part and part["text"] is not None:
            text_val = str(part["text"])
            if text_val:
                content_blocks.append({"type": "text", "text": text_val})

    if not content_blocks:
        content_blocks.append({"type": "text", "text": ""})

    usage = gemini_resp.get("usageMetadata") or {}
    input_tokens = int(usage.get("promptTokenCount", 0))
    output_tokens = int(usage.get("candidatesTokenCount", 0)) + int(
        usage.get("thoughtsTokenCount", 0)
    )

    stop_reason = "tool_use" if has_tool_use else "end_turn"
    if candidates and candidates[0].get("finishReason") == "MAX_TOKENS" and not has_tool_use:
        stop_reason = "max_tokens"

    return {
        "id": f"msg_{uuid.uuid4().hex[:24]}",
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": None,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": int(usage.get("cachedContentTokenCount", 0)),
        },
    }


def anthropic_message_to_sse_events(msg: dict[str, Any]) -> list[str]:
    """Serialize an Anthropic Message dict into a complete Anthropic SSE stream."""
    events: list[str] = []

    def _sse(event_name: str, data: dict[str, Any]) -> str:
        return f"event: {event_name}\ndata: {json.dumps(data)}\n\n"

    events.append(
        _sse(
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": msg["id"],
                    "type": "message",
                    "role": "assistant",
                    "model": msg["model"],
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {
                        "input_tokens": msg["usage"]["input_tokens"],
                        "output_tokens": 1,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": msg["usage"].get(
                            "cache_read_input_tokens", 0
                        ),
                    },
                },
            },
        )
    )

    for idx, block in enumerate(msg.get("content", [])):
        btype = block.get("type")
        if btype == "thinking":
            events.append(
                _sse(
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "thinking", "thinking": ""},
                    },
                )
            )
            if block.get("thinking"):
                events.append(
                    _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": block["thinking"],
                            },
                        },
                    )
                )
            if block.get("signature"):
                events.append(
                    _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {
                                "type": "signature_delta",
                                "signature": block["signature"],
                            },
                        },
                    )
                )
            events.append(
                _sse(
                    "content_block_stop",
                    {"type": "content_block_stop", "index": idx},
                )
            )

        elif btype == "text":
            events.append(
                _sse(
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "text", "text": ""},
                    },
                )
            )
            text_val = block.get("text", "")
            if text_val:
                events.append(
                    _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {"type": "text_delta", "text": text_val},
                        },
                    )
                )
            events.append(
                _sse(
                    "content_block_stop",
                    {"type": "content_block_stop", "index": idx},
                )
            )

        elif btype == "tool_use":
            events.append(
                _sse(
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": block["id"],
                            "name": block["name"],
                            "input": {},
                        },
                    },
                )
            )
            input_json = json.dumps(block.get("input") or {})
            events.append(
                _sse(
                    "content_block_delta",
                    {
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": input_json,
                        },
                    },
                )
            )
            events.append(
                _sse(
                    "content_block_stop",
                    {"type": "content_block_stop", "index": idx},
                )
            )

    events.append(
        _sse(
            "message_delta",
            {
                "type": "message_delta",
                "delta": {
                    "stop_reason": msg.get("stop_reason", "end_turn"),
                    "stop_sequence": None,
                },
                "usage": {"output_tokens": msg["usage"]["output_tokens"]},
            },
        )
    )
    events.append(_sse("message_stop", {"type": "message_stop"}))
    return events


# ============================================================================
# 2. OpenAI Chat Completions API (/v1/chat/completions) Translation
# ============================================================================


def openai_chat_to_gemini_request(
    payload: dict[str, Any],
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
) -> tuple[str, dict[str, Any]]:
    """Translate an OpenAI `/v1/chat/completions` request dict into `(gemini_model, gemini_body)`."""
    gemini_model, effort_level = resolve_gemini_model(payload.get("model"))
    gemini_body: dict[str, Any] = {}

    messages = payload.get("messages") or []
    system_parts: list[dict[str, Any]] = []
    tool_id_to_name: dict[str, str] = {}

    # Pass 1: collect system/developer messages and tool_call_id -> function name mappings
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        role = msg.get("role", "")
        if role in ("system", "developer"):
            content = msg.get("content")
            if isinstance(content, str) and content.strip():
                system_parts.append({"text": content.strip()})
            elif isinstance(content, list):
                for item in content:
                    if isinstance(item, dict) and item.get("text"):
                        system_parts.append({"text": str(item["text"])})
        elif role == "assistant":
            for tc in msg.get("tool_calls") or []:
                if isinstance(tc, dict):
                    tc_id = str(tc.get("id") or "")
                    fn = tc.get("function") or {}
                    fn_name = str(fn.get("name") or "")
                    if tc_id and fn_name:
                        tool_id_to_name[tc_id] = fn_name
                        signature_store.save_tool_name(tc_id, fn_name)

    if system_parts:
        gemini_body["systemInstruction"] = {"parts": system_parts}

    # Pass 2: build Gemini contents
    contents: list[dict[str, Any]] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        role = msg.get("role", "user")
        if role in ("system", "developer"):
            continue

        gemini_role = "model" if role == "assistant" else "user"
        parts: list[dict[str, Any]] = []
        turn_sig: str | None = msg.get("thought_signature")

        if role == "tool" or role == "function":
            tc_id = str(msg.get("tool_call_id") or "")
            fn_name = (
                msg.get("name")
                or tool_id_to_name.get(tc_id)
                or signature_store.get_tool_name(tc_id)
                or "tool"
            )
            raw_content = msg.get("content", "")
            if not isinstance(raw_content, str):
                raw_content = json.dumps(raw_content)
            parts.append(
                {
                    "functionResponse": {
                        "name": str(fn_name),
                        "response": {"result": raw_content},
                    }
                }
            )
        else:
            # Reasoning content if present on assistant turn
            reasoning_text = msg.get("reasoning_content") or msg.get("thinking")
            if isinstance(reasoning_text, str) and reasoning_text:
                thought_part: dict[str, Any] = {"thought": True, "text": reasoning_text}
                if turn_sig:
                    thought_part["thoughtSignature"] = turn_sig
                parts.append(thought_part)

            content = msg.get("content")
            if isinstance(content, str) and content:
                parts.append({"text": content})
            elif isinstance(content, list):
                for block in content:
                    if isinstance(block, str) and block:
                        parts.append({"text": block})
                    elif isinstance(block, dict):
                        btype = block.get("type")
                        if btype in ("text", "input_text", "output_text") and block.get("text"):
                            parts.append({"text": str(block["text"])})
                        elif btype == "image_url":
                            img_url = (block.get("image_url") or {}).get("url", "")
                            img_part = _parse_data_url_image(img_url)
                            if img_part:
                                parts.append(img_part)

            if role == "assistant":
                for tc in msg.get("tool_calls") or []:
                    if not isinstance(tc, dict):
                        continue
                    tc_id = str(tc.get("id") or f"call_{uuid.uuid4().hex[:12]}")
                    fn = tc.get("function") or {}
                    fn_name = str(fn.get("name") or "tool")
                    fn_args = _safe_json_loads_args(fn.get("arguments"))
                    fc_part: dict[str, Any] = {
                        "functionCall": {"name": fn_name, "args": fn_args}
                    }
                    sig = (
                        signature_store.get(tc_id)
                        or turn_sig
                        or signature_store.latest
                    )
                    if sig:
                        fc_part["thoughtSignature"] = sig
                    parts.append(fc_part)

        if not parts and gemini_role == "user":
            parts.append({"text": "Continue."})
        if not parts:
            continue
        if contents and contents[-1]["role"] == gemini_role:
            contents[-1]["parts"].extend(parts)
        else:
            contents.append({"role": gemini_role, "parts": parts})

    gemini_body["contents"] = _normalize_gemini_contents(contents)

    # Tools
    tools = payload.get("tools")
    if isinstance(tools, list) and tools:
        func_decls: list[dict[str, Any]] = []
        for t in tools:
            if not isinstance(t, dict):
                continue
            fn = t.get("function") if isinstance(t.get("function"), dict) else t
            name = fn.get("name")
            if not name:
                continue
            params = fn.get("parameters") or fn.get("input_schema") or {"type": "object"}
            func_decls.append(
                {
                    "name": str(name),
                    "description": str(fn.get("description") or f"Tool {name}"),
                    "parameters": sanitize_json_schema(params),
                }
            )
        if func_decls:
            gemini_body["tools"] = [{"functionDeclarations": func_decls}]

    tool_choice = payload.get("tool_choice")
    if "tools" in gemini_body and tool_choice:
        if tool_choice == "none":
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "NONE"}}
        elif tool_choice in ("required", "any"):
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "ANY"}}
        elif isinstance(tool_choice, dict):
            fn_name = (tool_choice.get("function") or {}).get("name") or tool_choice.get("name")
            if fn_name:
                gemini_body["toolConfig"] = {
                    "functionCallingConfig": {
                        "mode": "ANY",
                        "allowedFunctionNames": [str(fn_name)],
                    }
                }

    # GenerationConfig & ThinkingConfig
    gen_config: dict[str, Any] = {"temperature": 1.0}
    max_tokens = payload.get("max_completion_tokens") or payload.get("max_tokens")
    if isinstance(max_tokens, int) and max_tokens > 0:
        gen_config["maxOutputTokens"] = min(max_tokens, 65535)

    stop = payload.get("stop")
    if isinstance(stop, str) and stop:
        gen_config["stopSequences"] = [stop]
    elif isinstance(stop, list) and stop:
        gen_config["stopSequences"] = [str(s) for s in stop[:5]]

    req_effort = payload.get("reasoning_effort")
    if not req_effort and isinstance(payload.get("reasoning"), dict):
        req_effort = payload["reasoning"].get("effort")
    hot_swap_active = os.environ.get("GEMINI_HOT_SWAP_ACTIVE", "").strip() == "1"
    env_effort = _effort_str_to_thinking_level(os.environ.get("GEMINI_REASONING_EFFORT"))
    req_level = _effort_str_to_thinking_level(req_effort)
    is_fast_bg = is_fast_background_model(payload.get("model"))

    if is_fast_bg and not effort_level and not req_level:
        level = None
    elif effort_level:
        level = effort_level
    elif hot_swap_active and env_effort:
        level = env_effort
    else:
        level = req_level or env_effort

    if level:
        gen_config["thinkingConfig"] = {
            "includeThoughts": True,
            "thinkingLevel": level,
        }

    _apply_thinking_token_headroom(gen_config)
    gemini_body["generationConfig"] = gen_config
    return gemini_model, gemini_body


def gemini_to_openai_chat_response(
    gemini_resp: dict[str, Any],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
) -> dict[str, Any]:
    """Convert a Vertex AI Gemini GenerateContentResponse into an OpenAI ChatCompletion object."""
    candidates = gemini_resp.get("candidates") or []
    parts = (
        candidates[0].get("content", {}).get("parts", [])
        if candidates
        else []
    )

    text_chunks: list[str] = []
    thought_chunks: list[str] = []
    tool_calls: list[dict[str, Any]] = []
    latest_sig: str | None = None

    for part in parts:
        if not isinstance(part, dict):
            continue
        sig = part.get("thoughtSignature")
        if isinstance(sig, str) and sig:
            latest_sig = sig

        if part.get("thought") is True:
            if part.get("text"):
                thought_chunks.append(str(part["text"]))
        elif "functionCall" in part:
            fc = part["functionCall"]
            call_id = str(fc.get("id") or f"call_{uuid.uuid4().hex[:16]}")
            fn_name = str(fc.get("name", "tool"))
            fn_args = json.dumps(fc.get("args") or {})
            signature_store.save(call_id, latest_sig, tool_name=fn_name)
            tool_calls.append(
                {
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": fn_name,
                        "arguments": fn_args,
                    },
                }
            )
        elif "text" in part and part["text"] is not None:
            text_chunks.append(str(part["text"]))

    full_text = "".join(text_chunks)
    full_thought = "".join(thought_chunks)

    message: dict[str, Any] = {
        "role": "assistant",
        "content": full_text if (full_text or not tool_calls) else None,
    }
    if full_thought:
        message["reasoning_content"] = full_thought
    if latest_sig:
        message["thought_signature"] = latest_sig
    if tool_calls:
        message["tool_calls"] = tool_calls

    finish_reason = "tool_calls" if tool_calls else "stop"
    if candidates and candidates[0].get("finishReason") == "MAX_TOKENS" and not tool_calls:
        finish_reason = "length"

    usage = gemini_resp.get("usageMetadata") or {}
    prompt_tokens = int(usage.get("promptTokenCount", 0))
    thought_tokens = int(usage.get("thoughtsTokenCount", 0))
    cand_tokens = int(usage.get("candidatesTokenCount", 0))
    completion_tokens = cand_tokens + thought_tokens

    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:24]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": requested_model,
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
            "prompt_tokens_details": {
                "cached_tokens": int(usage.get("cachedContentTokenCount", 0)),
            },
            "completion_tokens_details": {
                "reasoning_tokens": thought_tokens,
            },
        },
    }


def openai_chat_to_sse_events(chat_resp: dict[str, Any]) -> list[str]:
    """Serialize an OpenAI ChatCompletion dict into an OpenAI SSE chunk stream."""
    events: list[str] = []
    cid = chat_resp["id"]
    created = chat_resp["created"]
    model = chat_resp["model"]
    choice = chat_resp["choices"][0]
    msg = choice["message"]
    finish_reason = choice["finish_reason"]

    def _chunk(delta: dict[str, Any], finish: str | None = None, usage: Any = None) -> str:
        payload: dict[str, Any] = {
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": delta,
                    "finish_reason": finish,
                }
            ],
        }
        if usage is not None:
            payload["usage"] = usage
        return f"data: {json.dumps(payload)}\n\n"

    # 1. Initial role chunk
    events.append(_chunk({"role": "assistant", "content": ""}))

    # 2. Reasoning delta if present
    if msg.get("reasoning_content"):
        events.append(_chunk({"reasoning_content": msg["reasoning_content"]}))

    # 3. Text content delta
    if msg.get("content"):
        events.append(_chunk({"content": msg["content"]}))

    # 4. Tool calls delta
    for idx, tc in enumerate(msg.get("tool_calls") or []):
        events.append(
            _chunk(
                {
                    "tool_calls": [
                        {
                            "index": idx,
                            "id": tc["id"],
                            "type": "function",
                            "function": {
                                "name": tc["function"]["name"],
                                "arguments": tc["function"]["arguments"],
                            },
                        }
                    ]
                }
            )
        )

    # 5. Final chunk with finish_reason and usage
    events.append(_chunk({}, finish=finish_reason, usage=chat_resp.get("usage")))
    events.append("data: [DONE]\n\n")
    return events


# ============================================================================
# 3. OpenAI Responses API (/v1/responses) Translation (Used by Codex CLI)
# ============================================================================


def _extract_responses_output_text(raw_output: Any) -> str:
    """Normalize `function_call_output.output` (string or content list) into a string."""
    if isinstance(raw_output, str):
        return raw_output
    if isinstance(raw_output, list):
        chunks: list[str] = []
        for item in raw_output:
            if isinstance(item, str):
                chunks.append(item)
            elif isinstance(item, dict):
                if "text" in item and item["text"] is not None:
                    chunks.append(str(item["text"]))
                else:
                    chunks.append(json.dumps(item))
        return "\n".join(chunks)
    if raw_output is None:
        return ""
    return json.dumps(raw_output)


def openai_responses_to_gemini_request(
    payload: dict[str, Any],
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
) -> tuple[str, dict[str, Any], set[str]]:
    """Translate an OpenAI `/v1/responses` request (Codex CLI) into `(gemini_model, gemini_body, custom_tool_names)`."""
    gemini_model, effort_level = resolve_gemini_model(payload.get("model"))
    gemini_body: dict[str, Any] = {}
    custom_tool_names: set[str] = set()

    system_parts: list[dict[str, Any]] = []
    instructions = payload.get("instructions")
    if isinstance(instructions, str) and instructions.strip():
        system_parts.append({"text": instructions.strip()})

    raw_input = payload.get("input")
    input_items: list[Any]
    if isinstance(raw_input, str):
        input_items = [{"type": "message", "role": "user", "content": raw_input}]
    elif isinstance(raw_input, list):
        input_items = raw_input
    else:
        input_items = []

    # Pass 1: map call_id -> function name across all function_call / custom_tool_call items
    call_id_to_name: dict[str, str] = {}
    for item in input_items:
        if not isinstance(item, dict):
            continue
        itype = item.get("type")
        if itype in ("function_call", "custom_tool_call"):
            cid = str(item.get("call_id") or item.get("id") or "")
            name = str(item.get("name") or "")
            if cid and name:
                call_id_to_name[cid] = name
                signature_store.save_tool_name(cid, name)
        elif itype == "local_shell_call":
            cid = str(item.get("call_id") or item.get("id") or "")
            if cid:
                call_id_to_name[cid] = "local_shell"
                signature_store.save_tool_name(cid, "local_shell")

    # Start with previous response contents if previous_response_id is supplied
    contents: list[dict[str, Any]] = []
    prev_id = payload.get("previous_response_id")
    if isinstance(prev_id, str) and prev_id:
        prev_contents = signature_store.get_response_contents(prev_id)
        if prev_contents:
            contents.extend([dict(c, parts=list(c.get("parts", []))) for c in prev_contents])

    def _append_turn(role: str, parts: list[dict[str, Any]]) -> None:
        if not parts:
            return
        if contents and contents[-1]["role"] == role:
            contents[-1]["parts"].extend(parts)
        else:
            contents.append({"role": role, "parts": parts})

    pending_signature: str | None = None

    for item in input_items:
        if isinstance(item, str):
            if item:
                _append_turn("user", [{"text": item}])
            continue
        if not isinstance(item, dict):
            continue

        itype = item.get("type")
        role = item.get("role")

        # 1. Message items (or role-based items)
        if itype == "message" or (itype is None and role):
            msg_role = str(role or "user")
            content = item.get("content")
            if msg_role in ("system", "developer"):
                if isinstance(content, str) and content.strip():
                    system_parts.append({"text": content.strip()})
                elif isinstance(content, list):
                    for block in content:
                        if isinstance(block, str) and block.strip():
                            system_parts.append({"text": block.strip()})
                        elif isinstance(block, dict) and block.get("text"):
                            system_parts.append({"text": str(block["text"])})
                continue

            gemini_role = "model" if msg_role == "assistant" else "user"
            parts: list[dict[str, Any]] = []
            if isinstance(content, str):
                if content:
                    parts.append({"text": content})
            elif isinstance(content, list):
                for block in content:
                    if isinstance(block, str) and block:
                        parts.append({"text": block})
                    elif isinstance(block, dict):
                        btype = block.get("type")
                        if btype in ("input_text", "output_text", "text") and block.get("text"):
                            parts.append({"text": str(block["text"])})
                        elif btype in ("summary_text", "reasoning_text") and block.get("text"):
                            parts.append({"thought": True, "text": str(block["text"])})
                        elif btype == "input_image":
                            img_url = block.get("image_url") or ""
                            if isinstance(img_url, dict):
                                img_url = img_url.get("url", "")
                            img_part = _parse_data_url_image(str(img_url))
                            if img_part:
                                parts.append(img_part)
            _append_turn(gemini_role, parts)

        # 2. Reasoning items
        elif itype == "reasoning":
            enc = item.get("encrypted_content")
            if isinstance(enc, str) and enc:
                pending_signature = enc
            summary_list = item.get("summary") or item.get("content") or []
            thought_texts: list[str] = []
            if isinstance(summary_list, list):
                for s in summary_list:
                    if isinstance(s, dict) and s.get("text"):
                        thought_texts.append(str(s["text"]))
            thought_part: dict[str, Any] = {
                "thought": True,
                "text": "\n".join(thought_texts) if thought_texts else " ",
            }
            if pending_signature:
                thought_part["thoughtSignature"] = pending_signature
            _append_turn("model", [thought_part])

        # 3. Function Call / Custom Tool Call items
        elif itype in ("function_call", "custom_tool_call", "local_shell_call"):
            cid = str(item.get("call_id") or item.get("id") or f"call_{uuid.uuid4().hex[:12]}")
            if itype == "custom_tool_call":
                fn_name = str(item.get("name") or "custom_tool")
                fn_args = {"input": str(item.get("input") or "")}
            elif itype == "local_shell_call":
                fn_name = "local_shell"
                action = item.get("action") or {}
                fn_args = {"command": action.get("command") or []}
            else:
                fn_name = str(item.get("name") or "tool")
                fn_args = _safe_json_loads_args(item.get("arguments"))

            fc_part: dict[str, Any] = {
                "functionCall": {
                    "name": fn_name,
                    "args": fn_args,
                }
            }
            sig = (
                signature_store.get(cid)
                or pending_signature
                or signature_store.latest
            )
            if sig:
                fc_part["thoughtSignature"] = sig
            _append_turn("model", [fc_part])

        # 4. Function Call Output / Custom Tool Call Output items
        elif itype in ("function_call_output", "custom_tool_call_output", "local_shell_call_output"):
            cid = str(item.get("call_id") or "")
            fn_name = (
                item.get("name")
                or call_id_to_name.get(cid)
                or signature_store.get_tool_name(cid)
                or "tool"
            )
            out_text = _extract_responses_output_text(item.get("output"))
            _append_turn(
                "user",
                [
                    {
                        "functionResponse": {
                            "name": str(fn_name),
                            "response": {"result": out_text},
                        }
                    }
                ],
            )

    if system_parts:
        gemini_body["systemInstruction"] = {"parts": system_parts}

    gemini_body["contents"] = _normalize_gemini_contents(contents)

    # Tools
    tools = payload.get("tools")
    if isinstance(tools, list) and tools:
        func_decls: list[dict[str, Any]] = []
        for t in tools:
            if not isinstance(t, dict):
                continue
            ttype = t.get("type", "function")
            if ttype == "function":
                fn = t.get("function") if isinstance(t.get("function"), dict) else t
                name = fn.get("name")
                if not name:
                    continue
                params = fn.get("parameters") or fn.get("input_schema") or {"type": "object"}
                func_decls.append(
                    {
                        "name": str(name),
                        "description": str(fn.get("description") or f"Tool {name}"),
                        "parameters": sanitize_json_schema(params),
                    }
                )
            elif ttype == "custom":
                name = t.get("name")
                if not name:
                    continue
                custom_tool_names.add(str(name))
                func_decls.append(
                    {
                        "name": str(name),
                        "description": str(t.get("description") or f"Custom tool {name}"),
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "input": {
                                    "type": "STRING",
                                    "description": f"Raw input payload for {name}",
                                }
                            },
                            "required": ["input"],
                        },
                    }
                )
            elif ttype == "local_shell":
                func_decls.append(
                    {
                        "name": "local_shell",
                        "description": "Execute a shell command",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "command": {
                                    "type": "ARRAY",
                                    "items": {"type": "STRING"},
                                    "description": "Command and arguments array",
                                }
                            },
                            "required": ["command"],
                        },
                    }
                )
        if func_decls:
            gemini_body["tools"] = [{"functionDeclarations": func_decls}]

    tool_choice = payload.get("tool_choice")
    if "tools" in gemini_body and tool_choice:
        if tool_choice == "none":
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "NONE"}}
        elif tool_choice in ("required", "any"):
            gemini_body["toolConfig"] = {"functionCallingConfig": {"mode": "ANY"}}
        elif isinstance(tool_choice, dict) and tool_choice.get("name"):
            gemini_body["toolConfig"] = {
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [str(tool_choice["name"])],
                }
            }

    # GenerationConfig & ThinkingConfig
    gen_config: dict[str, Any] = {"temperature": 1.0}
    max_out = payload.get("max_output_tokens") or payload.get("max_tokens")
    if isinstance(max_out, int) and max_out > 0:
        gen_config["maxOutputTokens"] = min(max_out, 65535)

    reasoning = payload.get("reasoning")
    req_effort = reasoning.get("effort") if isinstance(reasoning, dict) else None
    hot_swap_active = os.environ.get("GEMINI_HOT_SWAP_ACTIVE", "").strip() == "1"
    env_effort = _effort_str_to_thinking_level(os.environ.get("GEMINI_REASONING_EFFORT"))
    req_level = _effort_str_to_thinking_level(req_effort)
    is_fast_bg = is_fast_background_model(payload.get("model"))

    if is_fast_bg and not effort_level and not req_level:
        level = None
    elif effort_level:
        level = effort_level
    elif hot_swap_active and env_effort:
        level = env_effort
    else:
        level = req_level or env_effort

    if level:
        gen_config["thinkingConfig"] = {
            "includeThoughts": True,
            "thinkingLevel": level,
        }

    _apply_thinking_token_headroom(gen_config)
    gemini_body["generationConfig"] = gen_config
    return gemini_model, gemini_body, custom_tool_names


def gemini_to_openai_responses_object(
    gemini_resp: dict[str, Any],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
    custom_tool_names: set[str] | None = None,
    request_contents: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Convert a Vertex AI Gemini GenerateContentResponse into an OpenAI Responses API `response` object."""
    custom_names = custom_tool_names or set()
    candidates = gemini_resp.get("candidates") or []
    parts = (
        candidates[0].get("content", {}).get("parts", [])
        if candidates
        else []
    )

    output_items: list[dict[str, Any]] = []
    thought_chunks: list[str] = []
    text_chunks: list[str] = []
    tool_items: list[dict[str, Any]] = []
    latest_sig: str | None = None

    for part in parts:
        if not isinstance(part, dict):
            continue
        sig = part.get("thoughtSignature")
        if isinstance(sig, str) and sig:
            latest_sig = sig

        if part.get("thought") is True:
            if part.get("text"):
                thought_chunks.append(str(part["text"]))
        elif "functionCall" in part:
            fc = part["functionCall"]
            call_id = str(fc.get("id") or f"call_{uuid.uuid4().hex[:16]}")
            fn_name = str(fc.get("name", "tool"))
            fn_args_dict = fc.get("args") or {}
            signature_store.save(call_id, latest_sig, tool_name=fn_name)

            if fn_name in custom_names:
                raw_input = (
                    fn_args_dict.get("input")
                    if isinstance(fn_args_dict, dict) and "input" in fn_args_dict
                    else json.dumps(fn_args_dict)
                )
                tool_items.append(
                    {
                        "id": f"ctc_{uuid.uuid4().hex[:16]}",
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": fn_name,
                        "input": str(raw_input),
                    }
                )
            else:
                tool_items.append(
                    {
                        "id": f"fc_{uuid.uuid4().hex[:16]}",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": fn_name,
                        "arguments": json.dumps(fn_args_dict),
                    }
                )
        elif "text" in part and part["text"] is not None:
            text_val = str(part["text"])
            if text_val:
                text_chunks.append(text_val)

    full_thought = "\n".join(thought_chunks).strip()
    full_text = "".join(text_chunks)

    if full_thought or latest_sig:
        reasoning_item: dict[str, Any] = {
            "id": f"rs_{uuid.uuid4().hex[:16]}",
            "type": "reasoning",
            "status": "completed",
            "summary": (
                [{"type": "summary_text", "text": full_thought}]
                if full_thought
                else []
            ),
        }
        if latest_sig:
            reasoning_item["encrypted_content"] = latest_sig
        output_items.append(reasoning_item)

    if full_text or not tool_items:
        output_items.append(
            {
                "id": f"msg_{uuid.uuid4().hex[:16]}",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": full_text,
                        "annotations": [],
                    }
                ],
            }
        )

    output_items.extend(tool_items)

    usage = gemini_resp.get("usageMetadata") or {}
    input_tokens = int(usage.get("promptTokenCount", 0))
    cached_tokens = int(usage.get("cachedContentTokenCount", 0))
    thought_tokens = int(usage.get("thoughtsTokenCount", 0))
    cand_tokens = int(usage.get("candidatesTokenCount", 0))
    output_tokens = cand_tokens + thought_tokens

    resp_id = f"resp_{uuid.uuid4().hex[:24]}"
    if request_contents is not None and parts:
        updated_contents = list(request_contents) + [{"role": "model", "parts": parts}]
        signature_store.save_response_contents(resp_id, updated_contents)

    return {
        "id": resp_id,
        "object": "response",
        "created_at": int(time.time()),
        "status": "completed",
        "error": None,
        "incomplete_details": None,
        "model": requested_model,
        "output": output_items,
        "output_text": full_text,
        "parallel_tool_calls": True,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {
                "cached_tokens": cached_tokens,
            },
            "output_tokens": output_tokens,
            "output_tokens_details": {
                "reasoning_tokens": thought_tokens,
            },
            "total_tokens": input_tokens + output_tokens,
        },
    }


def openai_responses_to_sse_events(resp_obj: dict[str, Any]) -> list[str]:
    """Serialize an OpenAI Responses API `response` dict into a complete SSE event stream for Codex CLI."""
    events: list[str] = []
    seq = 0

    def _sse(event_type: str, payload: dict[str, Any]) -> str:
        nonlocal seq
        data = {"type": event_type, "sequence_number": seq, **payload}
        seq += 1
        return f"event: {event_type}\ndata: {json.dumps(data)}\n\n"

    in_progress_resp = {
        **resp_obj,
        "status": "in_progress",
        "output": [],
        "output_text": "",
    }
    events.append(_sse("response.created", {"response": in_progress_resp}))
    events.append(_sse("response.in_progress", {"response": in_progress_resp}))

    for output_index, item in enumerate(resp_obj.get("output", [])):
        itype = item.get("type")
        item_id = item["id"]

        if itype == "reasoning":
            added_item = {**item, "summary": []}
            events.append(
                _sse(
                    "response.output_item.added",
                    {"output_index": output_index, "item": added_item},
                )
            )
            for summary_index, part in enumerate(item.get("summary", [])):
                txt = part.get("text", "")
                events.append(
                    _sse(
                        "response.reasoning_summary_part.added",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": summary_index,
                            "part": {"type": "summary_text", "text": ""},
                        },
                    )
                )
                if txt:
                    events.append(
                        _sse(
                            "response.reasoning_summary_text.delta",
                            {
                                "item_id": item_id,
                                "output_index": output_index,
                                "summary_index": summary_index,
                                "delta": txt,
                            },
                        )
                    )
                events.append(
                    _sse(
                        "response.reasoning_summary_text.done",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": summary_index,
                            "text": txt,
                        },
                    )
                )
                events.append(
                    _sse(
                        "response.reasoning_summary_part.done",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": summary_index,
                            "part": {"type": "summary_text", "text": txt},
                        },
                    )
                )
            events.append(
                _sse(
                    "response.output_item.done",
                    {"output_index": output_index, "item": item},
                )
            )

        elif itype == "message":
            added_item = {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            }
            events.append(
                _sse(
                    "response.output_item.added",
                    {"output_index": output_index, "item": added_item},
                )
            )
            for content_index, part in enumerate(item.get("content", [])):
                txt = part.get("text", "")
                events.append(
                    _sse(
                        "response.content_part.added",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        },
                    )
                )
                if txt:
                    events.append(
                        _sse(
                            "response.output_text.delta",
                            {
                                "item_id": item_id,
                                "output_index": output_index,
                                "content_index": content_index,
                                "delta": txt,
                            },
                        )
                    )
                events.append(
                    _sse(
                        "response.output_text.done",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "text": txt,
                        },
                    )
                )
                events.append(
                    _sse(
                        "response.content_part.done",
                        {
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "part": {"type": "output_text", "text": txt, "annotations": []},
                        },
                    )
                )
            events.append(
                _sse(
                    "response.output_item.done",
                    {"output_index": output_index, "item": item},
                )
            )

        elif itype == "function_call":
            added_item = {
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "call_id": item["call_id"],
                "name": item["name"],
                "arguments": "",
            }
            events.append(
                _sse(
                    "response.output_item.added",
                    {"output_index": output_index, "item": added_item},
                )
            )
            args_str = item.get("arguments", "{}")
            events.append(
                _sse(
                    "response.function_call_arguments.delta",
                    {
                        "item_id": item_id,
                        "output_index": output_index,
                        "call_id": item["call_id"],
                        "delta": args_str,
                    },
                )
            )
            events.append(
                _sse(
                    "response.function_call_arguments.done",
                    {
                        "item_id": item_id,
                        "output_index": output_index,
                        "call_id": item["call_id"],
                        "name": item["name"],
                        "arguments": args_str,
                    },
                )
            )
            events.append(
                _sse(
                    "response.output_item.done",
                    {"output_index": output_index, "item": item},
                )
            )

        elif itype == "custom_tool_call":
            added_item = {
                "id": item_id,
                "type": "custom_tool_call",
                "status": "in_progress",
                "call_id": item["call_id"],
                "name": item["name"],
                "input": "",
            }
            events.append(
                _sse(
                    "response.output_item.added",
                    {"output_index": output_index, "item": added_item},
                )
            )
            inp_str = item.get("input", "")
            events.append(
                _sse(
                    "response.custom_tool_call_input.delta",
                    {
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": inp_str,
                    },
                )
            )
            events.append(
                _sse(
                    "response.custom_tool_call_input.done",
                    {
                        "item_id": item_id,
                        "output_index": output_index,
                        "input": inp_str,
                    },
                )
            )
            events.append(
                _sse(
                    "response.output_item.done",
                    {"output_index": output_index, "item": item},
                )
            )

    events.append(_sse("response.completed", {"response": resp_obj}))
    return events


# ============================================================================
# 4. Real-Time Upstream SSE Stream Translators (:streamGenerateContent?alt=sse)
# ============================================================================


async def iter_anthropic_sse_from_gemini_chunks(
    gemini_chunks: AsyncIterator[dict[str, Any]],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
    on_usage: Callable[[int, int, int, int], None] | None = None,
) -> AsyncIterator[str]:
    """Stream Anthropic `/v1/messages` SSE events live from Vertex AI `:streamGenerateContent` chunks."""

    def _sse(event_name: str, data: dict[str, Any]) -> str:
        return f"event: {event_name}\ndata: {json.dumps(data)}\n\n"

    msg_id = f"msg_{uuid.uuid4().hex[:24]}"
    started = False
    current_block_type: str | None = None  # "thinking" | "text"
    block_index = -1
    has_tool_use = False
    latest_sig: str | None = None
    emitted_sig_in_thinking = False
    input_tokens = 0
    output_tokens = 0
    thought_tokens = 0
    cached_tokens = 0
    finish_reason = "STOP"

    async for chunk in gemini_chunks:
        usage = chunk.get("usageMetadata") or {}
        if "promptTokenCount" in usage:
            input_tokens = int(usage.get("promptTokenCount", 0))
        if "cachedContentTokenCount" in usage:
            cached_tokens = int(usage.get("cachedContentTokenCount", 0))
        if "thoughtsTokenCount" in usage:
            thought_tokens = int(usage.get("thoughtsTokenCount", 0))
        if "candidatesTokenCount" in usage:
            output_tokens = int(usage.get("candidatesTokenCount", 0)) + thought_tokens

        if not started:
            started = True
            yield _sse(
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": msg_id,
                        "type": "message",
                        "role": "assistant",
                        "model": requested_model,
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {
                            "input_tokens": input_tokens,
                            "output_tokens": 1,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": cached_tokens,
                        },
                    },
                },
            )

        candidates = chunk.get("candidates") or []
        if not candidates:
            continue
        cand0 = candidates[0]
        if cand0.get("finishReason"):
            finish_reason = str(cand0["finishReason"])
        parts = (cand0.get("content") or {}).get("parts") or []

        for part in parts:
            if not isinstance(part, dict):
                continue
            sig = part.get("thoughtSignature")
            if isinstance(sig, str) and sig:
                latest_sig = sig

            if part.get("thought") is True:
                if current_block_type != "thinking":
                    if current_block_type is not None:
                        yield _sse(
                            "content_block_stop",
                            {"type": "content_block_stop", "index": block_index},
                        )
                    block_index += 1
                    current_block_type = "thinking"
                    emitted_sig_in_thinking = False
                    yield _sse(
                        "content_block_start",
                        {
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {"type": "thinking", "thinking": ""},
                        },
                    )
                txt = part.get("text", "")
                if txt:
                    yield _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "thinking_delta", "thinking": str(txt)},
                        },
                    )
                if sig and not emitted_sig_in_thinking:
                    emitted_sig_in_thinking = True
                    yield _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "signature_delta", "signature": sig},
                        },
                    )

            elif "functionCall" in part:
                if current_block_type == "thinking" and latest_sig and not emitted_sig_in_thinking:
                    emitted_sig_in_thinking = True
                    yield _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "signature_delta", "signature": latest_sig},
                        },
                    )
                if current_block_type is not None:
                    yield _sse(
                        "content_block_stop",
                        {"type": "content_block_stop", "index": block_index},
                    )
                    current_block_type = None

                has_tool_use = True
                fc = part["functionCall"]
                tool_id = str(fc.get("id") or f"toolu_{uuid.uuid4().hex[:16]}")
                tool_name = str(fc.get("name", "tool"))
                signature_store.save(tool_id, latest_sig, tool_name=tool_name)

                block_index += 1
                yield _sse(
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": tool_name,
                            "input": {},
                        },
                    },
                )
                yield _sse(
                    "content_block_delta",
                    {
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": json.dumps(fc.get("args") or {}),
                        },
                    },
                )
                yield _sse(
                    "content_block_stop",
                    {"type": "content_block_stop", "index": block_index},
                )

            elif "text" in part and part["text"] is not None:
                txt = str(part["text"])
                if not txt:
                    continue
                if current_block_type == "thinking" and latest_sig and not emitted_sig_in_thinking:
                    emitted_sig_in_thinking = True
                    yield _sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "signature_delta", "signature": latest_sig},
                        },
                    )
                if current_block_type != "text":
                    if current_block_type is not None:
                        yield _sse(
                            "content_block_stop",
                            {"type": "content_block_stop", "index": block_index},
                        )
                    block_index += 1
                    current_block_type = "text"
                    yield _sse(
                        "content_block_start",
                        {
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {"type": "text", "text": ""},
                        },
                    )
                yield _sse(
                    "content_block_delta",
                    {
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {"type": "text_delta", "text": txt},
                    },
                )

    if not started:
        yield _sse(
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": requested_model,
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 1,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            },
        )

    if current_block_type == "thinking" and latest_sig and not emitted_sig_in_thinking:
        yield _sse(
            "content_block_delta",
            {
                "type": "content_block_delta",
                "index": block_index,
                "delta": {"type": "signature_delta", "signature": latest_sig},
            },
        )
    if current_block_type is not None:
        yield _sse(
            "content_block_stop",
            {"type": "content_block_stop", "index": block_index},
        )
    elif block_index < 0:
        block_index = 0
        yield _sse(
            "content_block_start",
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
        )
        yield _sse("content_block_stop", {"type": "content_block_stop", "index": 0})

    stop_reason = "tool_use" if has_tool_use else "end_turn"
    if finish_reason == "MAX_TOKENS" and not has_tool_use:
        stop_reason = "max_tokens"

    if on_usage:
        on_usage(input_tokens, output_tokens, thought_tokens, cached_tokens)

    yield _sse(
        "message_delta",
        {
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": None},
            "usage": {"output_tokens": output_tokens},
        },
    )
    yield _sse("message_stop", {"type": "message_stop"})


async def iter_openai_chat_sse_from_gemini_chunks(
    gemini_chunks: AsyncIterator[dict[str, Any]],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
    on_usage: Callable[[int, int, int, int], None] | None = None,
) -> AsyncIterator[str]:
    """Stream OpenAI `/v1/chat/completions` SSE chunks live from Vertex AI `:streamGenerateContent`."""
    cid = f"chatcmpl-{uuid.uuid4().hex[:24]}"
    created = int(time.time())
    started = False
    tool_idx = 0
    has_tools = False
    latest_sig: str | None = None
    finish_reason = "STOP"
    input_tokens = 0
    output_tokens = 0
    thought_tokens = 0
    cached_tokens = 0

    def _chunk(delta: dict[str, Any], finish: str | None = None, usage: Any = None) -> str:
        payload: dict[str, Any] = {
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": requested_model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }
        if usage is not None:
            payload["usage"] = usage
        return f"data: {json.dumps(payload)}\n\n"

    async for chunk in gemini_chunks:
        if not started:
            started = True
            yield _chunk({"role": "assistant", "content": ""})

        usage = chunk.get("usageMetadata") or {}
        if "promptTokenCount" in usage:
            input_tokens = int(usage.get("promptTokenCount", 0))
        if "cachedContentTokenCount" in usage:
            cached_tokens = int(usage.get("cachedContentTokenCount", 0))
        if "thoughtsTokenCount" in usage:
            thought_tokens = int(usage.get("thoughtsTokenCount", 0))
        if "candidatesTokenCount" in usage:
            output_tokens = int(usage.get("candidatesTokenCount", 0)) + thought_tokens

        candidates = chunk.get("candidates") or []
        if not candidates:
            continue
        cand0 = candidates[0]
        if cand0.get("finishReason"):
            finish_reason = str(cand0["finishReason"])
        parts = (cand0.get("content") or {}).get("parts") or []

        for part in parts:
            if not isinstance(part, dict):
                continue
            sig = part.get("thoughtSignature")
            if isinstance(sig, str) and sig:
                latest_sig = sig

            if part.get("thought") is True:
                if part.get("text"):
                    yield _chunk({"reasoning_content": str(part["text"])})
            elif "functionCall" in part:
                has_tools = True
                fc = part["functionCall"]
                call_id = str(fc.get("id") or f"call_{uuid.uuid4().hex[:16]}")
                fn_name = str(fc.get("name", "tool"))
                fn_args = json.dumps(fc.get("args") or {})
                signature_store.save(call_id, latest_sig, tool_name=fn_name)
                yield _chunk(
                    {
                        "tool_calls": [
                            {
                                "index": tool_idx,
                                "id": call_id,
                                "type": "function",
                                "function": {"name": fn_name, "arguments": fn_args},
                            }
                        ]
                    }
                )
                tool_idx += 1
            elif "text" in part and part["text"] is not None:
                txt = str(part["text"])
                if txt:
                    yield _chunk({"content": txt})

    if not started:
        yield _chunk({"role": "assistant", "content": ""})

    final_finish = "tool_calls" if has_tools else ("length" if finish_reason == "MAX_TOKENS" else "stop")
    if on_usage:
        on_usage(input_tokens, output_tokens, thought_tokens, cached_tokens)

    usage_dict = {
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "prompt_tokens_details": {"cached_tokens": cached_tokens},
        "completion_tokens_details": {"reasoning_tokens": thought_tokens},
    }
    yield _chunk({}, finish=final_finish, usage=usage_dict)
    yield "data: [DONE]\n\n"


async def iter_openai_responses_sse_from_gemini_chunks(
    gemini_chunks: AsyncIterator[dict[str, Any]],
    requested_model: str,
    signature_store: ThoughtSignatureStore = SIGNATURE_STORE,
    custom_tool_names: set[str] | None = None,
    request_contents: list[dict[str, Any]] | None = None,
    on_usage: Callable[[int, int, int, int], None] | None = None,
) -> AsyncIterator[str]:
    """Stream OpenAI Codex `/v1/responses` SSE events live from Vertex AI `:streamGenerateContent`."""
    custom_names = custom_tool_names or set()
    seq = 0
    resp_id = f"resp_{uuid.uuid4().hex[:24]}"
    created_at = int(time.time())

    def _sse(event_type: str, payload: dict[str, Any]) -> str:
        nonlocal seq
        data = {"type": event_type, "sequence_number": seq, **payload}
        seq += 1
        return f"event: {event_type}\ndata: {json.dumps(data)}\n\n"

    base_resp = {
        "id": resp_id,
        "object": "response",
        "created_at": created_at,
        "status": "in_progress",
        "error": None,
        "incomplete_details": None,
        "model": requested_model,
        "output": [],
        "output_text": "",
        "parallel_tool_calls": True,
    }
    yield _sse("response.created", {"response": base_resp})
    yield _sse("response.in_progress", {"response": base_resp})

    output_items: list[dict[str, Any]] = []
    all_parts: list[dict[str, Any]] = []
    output_index = 0
    reasoning_open = False
    reasoning_id = f"rs_{uuid.uuid4().hex[:16]}"
    reasoning_idx = -1
    thought_chunks: list[str] = []

    message_open = False
    message_id = f"msg_{uuid.uuid4().hex[:16]}"
    message_idx = -1
    text_chunks: list[str] = []

    latest_sig: str | None = None
    input_tokens = 0
    output_tokens = 0
    thought_tokens = 0
    cached_tokens = 0

    async for chunk in gemini_chunks:
        usage = chunk.get("usageMetadata") or {}
        if "promptTokenCount" in usage:
            input_tokens = int(usage.get("promptTokenCount", 0))
        if "cachedContentTokenCount" in usage:
            cached_tokens = int(usage.get("cachedContentTokenCount", 0))
        if "thoughtsTokenCount" in usage:
            thought_tokens = int(usage.get("thoughtsTokenCount", 0))
        if "candidatesTokenCount" in usage:
            output_tokens = int(usage.get("candidatesTokenCount", 0)) + thought_tokens

        candidates = chunk.get("candidates") or []
        if not candidates:
            continue
        parts = (candidates[0].get("content") or {}).get("parts") or []

        for part in parts:
            if not isinstance(part, dict):
                continue
            all_parts.append(part)
            sig = part.get("thoughtSignature")
            if isinstance(sig, str) and sig:
                latest_sig = sig

            if part.get("thought") is True:
                txt = str(part.get("text") or "")
                if not reasoning_open:
                    reasoning_open = True
                    reasoning_idx = output_index
                    output_index += 1
                    yield _sse(
                        "response.output_item.added",
                        {
                            "output_index": reasoning_idx,
                            "item": {
                                "id": reasoning_id,
                                "type": "reasoning",
                                "status": "in_progress",
                                "summary": [],
                            },
                        },
                    )
                    yield _sse(
                        "response.reasoning_summary_part.added",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""},
                        },
                    )
                if txt:
                    thought_chunks.append(txt)
                    yield _sse(
                        "response.reasoning_summary_text.delta",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "delta": txt,
                        },
                    )

            elif "text" in part and part["text"] is not None:
                txt = str(part["text"])
                if not txt:
                    continue
                if reasoning_open:
                    full_thought = "".join(thought_chunks).strip()
                    yield _sse(
                        "response.reasoning_summary_text.done",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "text": full_thought,
                        },
                    )
                    yield _sse(
                        "response.reasoning_summary_part.done",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": full_thought},
                        },
                    )
                    done_reasoning: dict[str, Any] = {
                        "id": reasoning_id,
                        "type": "reasoning",
                        "status": "completed",
                        "summary": (
                            [{"type": "summary_text", "text": full_thought}]
                            if full_thought
                            else []
                        ),
                    }
                    if latest_sig:
                        done_reasoning["encrypted_content"] = latest_sig
                    output_items.append(done_reasoning)
                    yield _sse(
                        "response.output_item.done",
                        {"output_index": reasoning_idx, "item": done_reasoning},
                    )
                    reasoning_open = False

                if not message_open:
                    message_open = True
                    message_idx = output_index
                    output_index += 1
                    yield _sse(
                        "response.output_item.added",
                        {
                            "output_index": message_idx,
                            "item": {
                                "id": message_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            },
                        },
                    )
                    yield _sse(
                        "response.content_part.added",
                        {
                            "item_id": message_id,
                            "output_index": message_idx,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        },
                    )
                text_chunks.append(txt)
                yield _sse(
                    "response.output_text.delta",
                    {
                        "item_id": message_id,
                        "output_index": message_idx,
                        "content_index": 0,
                        "delta": txt,
                    },
                )

            elif "functionCall" in part:
                if reasoning_open:
                    full_thought = "".join(thought_chunks).strip()
                    yield _sse(
                        "response.reasoning_summary_text.done",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "text": full_thought,
                        },
                    )
                    yield _sse(
                        "response.reasoning_summary_part.done",
                        {
                            "item_id": reasoning_id,
                            "output_index": reasoning_idx,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": full_thought},
                        },
                    )
                    done_reasoning = {
                        "id": reasoning_id,
                        "type": "reasoning",
                        "status": "completed",
                        "summary": (
                            [{"type": "summary_text", "text": full_thought}]
                            if full_thought
                            else []
                        ),
                    }
                    if latest_sig:
                        done_reasoning["encrypted_content"] = latest_sig
                    output_items.append(done_reasoning)
                    yield _sse(
                        "response.output_item.done",
                        {"output_index": reasoning_idx, "item": done_reasoning},
                    )
                    reasoning_open = False

                if message_open:
                    full_text = "".join(text_chunks)
                    yield _sse(
                        "response.output_text.done",
                        {
                            "item_id": message_id,
                            "output_index": message_idx,
                            "content_index": 0,
                            "text": full_text,
                        },
                    )
                    yield _sse(
                        "response.content_part.done",
                        {
                            "item_id": message_id,
                            "output_index": message_idx,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": full_text, "annotations": []},
                        },
                    )
                    done_msg = {
                        "id": message_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": full_text, "annotations": []}],
                    }
                    output_items.append(done_msg)
                    yield _sse(
                        "response.output_item.done",
                        {"output_index": message_idx, "item": done_msg},
                    )
                    message_open = False

                fc = part["functionCall"]
                call_id = str(fc.get("id") or f"call_{uuid.uuid4().hex[:16]}")
                fn_name = str(fc.get("name", "tool"))
                fn_args_dict = fc.get("args") or {}
                signature_store.save(call_id, latest_sig, tool_name=fn_name)
                cur_idx = output_index
                output_index += 1

                if fn_name in custom_names:
                    item_id = f"ctc_{uuid.uuid4().hex[:16]}"
                    raw_inp = str(
                        fn_args_dict.get("input")
                        if isinstance(fn_args_dict, dict) and "input" in fn_args_dict
                        else json.dumps(fn_args_dict)
                    )
                    yield _sse(
                        "response.output_item.added",
                        {
                            "output_index": cur_idx,
                            "item": {
                                "id": item_id,
                                "type": "custom_tool_call",
                                "status": "in_progress",
                                "call_id": call_id,
                                "name": fn_name,
                                "input": "",
                            },
                        },
                    )
                    yield _sse(
                        "response.custom_tool_call_input.delta",
                        {"item_id": item_id, "output_index": cur_idx, "delta": raw_inp},
                    )
                    yield _sse(
                        "response.custom_tool_call_input.done",
                        {"item_id": item_id, "output_index": cur_idx, "input": raw_inp},
                    )
                    done_ctc = {
                        "id": item_id,
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": fn_name,
                        "input": raw_inp,
                    }
                    output_items.append(done_ctc)
                    yield _sse(
                        "response.output_item.done",
                        {"output_index": cur_idx, "item": done_ctc},
                    )
                else:
                    item_id = f"fc_{uuid.uuid4().hex[:16]}"
                    args_str = json.dumps(fn_args_dict)
                    yield _sse(
                        "response.output_item.added",
                        {
                            "output_index": cur_idx,
                            "item": {
                                "id": item_id,
                                "type": "function_call",
                                "status": "in_progress",
                                "call_id": call_id,
                                "name": fn_name,
                                "arguments": "",
                            },
                        },
                    )
                    yield _sse(
                        "response.function_call_arguments.delta",
                        {
                            "item_id": item_id,
                            "output_index": cur_idx,
                            "call_id": call_id,
                            "delta": args_str,
                        },
                    )
                    yield _sse(
                        "response.function_call_arguments.done",
                        {
                            "item_id": item_id,
                            "output_index": cur_idx,
                            "call_id": call_id,
                            "name": fn_name,
                            "arguments": args_str,
                        },
                    )
                    done_fc = {
                        "id": item_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": fn_name,
                        "arguments": args_str,
                    }
                    output_items.append(done_fc)
                    yield _sse(
                        "response.output_item.done",
                        {"output_index": cur_idx, "item": done_fc},
                    )

    if reasoning_open:
        full_thought = "".join(thought_chunks).strip()
        yield _sse(
            "response.reasoning_summary_text.done",
            {
                "item_id": reasoning_id,
                "output_index": reasoning_idx,
                "summary_index": 0,
                "text": full_thought,
            },
        )
        yield _sse(
            "response.reasoning_summary_part.done",
            {
                "item_id": reasoning_id,
                "output_index": reasoning_idx,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": full_thought},
            },
        )
        done_reasoning = {
            "id": reasoning_id,
            "type": "reasoning",
            "status": "completed",
            "summary": (
                [{"type": "summary_text", "text": full_thought}]
                if full_thought
                else []
            ),
        }
        if latest_sig:
            done_reasoning["encrypted_content"] = latest_sig
        output_items.append(done_reasoning)
        yield _sse(
            "response.output_item.done",
            {"output_index": reasoning_idx, "item": done_reasoning},
        )

    full_text_final = "".join(text_chunks)
    if message_open:
        yield _sse(
            "response.output_text.done",
            {
                "item_id": message_id,
                "output_index": message_idx,
                "content_index": 0,
                "text": full_text_final,
            },
        )
        yield _sse(
            "response.content_part.done",
            {
                "item_id": message_id,
                "output_index": message_idx,
                "content_index": 0,
                "part": {"type": "output_text", "text": full_text_final, "annotations": []},
            },
        )
        done_msg = {
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": full_text_final, "annotations": []}],
        }
        output_items.append(done_msg)
        yield _sse(
            "response.output_item.done",
            {"output_index": message_idx, "item": done_msg},
        )

    if request_contents is not None and all_parts:
        updated_contents = list(request_contents) + [{"role": "model", "parts": all_parts}]
        signature_store.save_response_contents(resp_id, updated_contents)

    if on_usage:
        on_usage(input_tokens, output_tokens, thought_tokens, cached_tokens)

    completed_resp = {
        **base_resp,
        "status": "completed",
        "output": output_items,
        "output_text": full_text_final,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": cached_tokens},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": thought_tokens},
            "total_tokens": input_tokens + output_tokens,
        },
    }
    yield _sse("response.completed", {"response": completed_resp})
