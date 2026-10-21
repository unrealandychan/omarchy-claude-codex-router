"""Unit and live ADC integration tests for the Claude Code & OpenAI Codex -> Gemini 3.8 / 4.0 Router."""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path

from fastapi.testclient import TestClient

from router.adc_auth import ADCAuthManager
from router.server import app
from router.translator import (
    MODEL_GEMINI_3_8_FLASH,
    MODEL_GEMINI_4_ARGON,
    ThoughtSignatureStore,
    anthropic_to_gemini_request,
    gemini_to_anthropic_response,
    gemini_to_openai_chat_response,
    gemini_to_openai_responses_object,
    is_fast_background_model,
    iter_anthropic_sse_from_gemini_chunks,
    iter_openai_chat_sse_from_gemini_chunks,
    iter_openai_responses_sse_from_gemini_chunks,
    openai_chat_to_gemini_request,
    openai_chat_to_sse_events,
    openai_responses_to_gemini_request,
    openai_responses_to_sse_events,
    resolve_gemini_model,
    sanitize_json_schema,
)


def test_model_resolution() -> None:
    os.environ.pop("GEMINI_PRIMARY_MODEL", None)
    os.environ.pop("GEMINI_REASONING_EFFORT", None)
    os.environ.pop("GEMINI_HOT_SWAP_ACTIVE", None)

    assert resolve_gemini_model("gemini-3.8") == (MODEL_GEMINI_3_8_FLASH, None)
    assert resolve_gemini_model("gemini-3.8-flash") == (MODEL_GEMINI_3_8_FLASH, None)
    assert resolve_gemini_model("gemini-3.8-flash-high") == (MODEL_GEMINI_3_8_FLASH, "HIGH")
    assert resolve_gemini_model("gemini-3.8-flash-xhigh") == (MODEL_GEMINI_3_8_FLASH, "HIGH")
    assert resolve_gemini_model("claude-3-5-haiku-20241022") == (MODEL_GEMINI_3_8_FLASH, None)
    assert resolve_gemini_model("claude-sonnet-4-20250514") == (MODEL_GEMINI_3_8_FLASH, None)
    assert resolve_gemini_model("gpt-5-codex") == (MODEL_GEMINI_3_8_FLASH, None)
    assert resolve_gemini_model("o4-mini") == (MODEL_GEMINI_3_8_FLASH, None)

    assert resolve_gemini_model("gemini-4.0") == (MODEL_GEMINI_4_ARGON, None)
    assert resolve_gemini_model("gemini-4.0-pro") == (MODEL_GEMINI_4_ARGON, None)
    assert resolve_gemini_model("gemini-4-argon") == (MODEL_GEMINI_4_ARGON, None)
    assert resolve_gemini_model("gemini-4-argon-low") == (MODEL_GEMINI_4_ARGON, "LOW")
    assert resolve_gemini_model("claude-opus-4-20250514") == (MODEL_GEMINI_4_ARGON, None)
    assert resolve_gemini_model("gpt-5-pro") == (MODEL_GEMINI_4_ARGON, None)
    assert resolve_gemini_model("o3-pro") == (MODEL_GEMINI_4_ARGON, None)


def test_hot_swap_tier_mapping_and_xhigh_effort() -> None:
    try:
        os.environ["GEMINI_PRIMARY_MODEL"] = MODEL_GEMINI_4_ARGON
        os.environ["GEMINI_REASONING_EFFORT"] = "xhigh"
        os.environ["GEMINI_HOT_SWAP_ACTIVE"] = "1"

        # Fast background Haiku calls always stay on 3.8 Flash with no thinking overhead
        assert is_fast_background_model("claude-haiku-4-5") is True
        haiku_model, haiku_body = anthropic_to_gemini_request(
            {
                "model": "claude-haiku-4-5",
                "messages": [{"role": "user", "content": "Summarize title"}],
            }
        )
        assert haiku_model == MODEL_GEMINI_3_8_FLASH
        assert "thinkingConfig" not in haiku_body.get("generationConfig", {})

        # Already-running Claude session that originally booted with gemini-3.8-flash[1m]
        # immediately hot-swaps to gemini-4-argon with HIGH thinking (from xhigh)
        main_model, main_body = anthropic_to_gemini_request(
            {
                "model": "gemini-3.8-flash[1m]",
                "messages": [{"role": "user", "content": "Refactor module"}],
            }
        )
        assert main_model == MODEL_GEMINI_4_ARGON
        assert main_body["generationConfig"]["thinkingConfig"] == {
            "includeThoughts": True,
            "thinkingLevel": "HIGH",
        }

        # Opus and Sonnet aliases both track primary_model when GEMINI_PRIMARY_MODEL=gemini-3.8-flash
        os.environ["GEMINI_PRIMARY_MODEL"] = MODEL_GEMINI_3_8_FLASH
        os.environ.pop("GEMINI_HOT_SWAP_ACTIVE", None)
        assert resolve_gemini_model("claude-opus-4-6")[0] == MODEL_GEMINI_3_8_FLASH
        assert resolve_gemini_model("claude-sonnet-4-6")[0] == MODEL_GEMINI_3_8_FLASH
    finally:
        os.environ.pop("GEMINI_PRIMARY_MODEL", None)
        os.environ.pop("GEMINI_REASONING_EFFORT", None)
        os.environ.pop("GEMINI_HOT_SWAP_ACTIVE", None)


def test_trailing_model_turn_and_interrupted_tool_normalization() -> None:
    # 1. Assistant prefill / thinking-only continuation ending with role="assistant"
    _, body_prefill = anthropic_to_gemini_request(
        {
            "model": "gemini-4-argon-high",
            "max_tokens": 2048,
            "messages": [
                {"role": "user", "content": "Return JSON output"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Preparing JSON...",
                            "signature": "sig_1",
                        }
                    ],
                },
            ],
        }
    )
    assert body_prefill["contents"][0]["role"] == "user"
    assert body_prefill["contents"][-1]["role"] == "user"
    assert body_prefill["generationConfig"]["maxOutputTokens"] == 2048 + 16384

    # 2. Trailing assistant tool_use without a following tool_result
    _, body_dangling_tool = anthropic_to_gemini_request(
        {
            "model": "gemini-4-argon",
            "messages": [
                {"role": "user", "content": "Run ls"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_123",
                            "name": "Bash",
                            "input": {"command": "ls"},
                        }
                    ],
                },
            ],
        }
    )
    assert body_dangling_tool["contents"][-1]["role"] == "user"
    assert "functionResponse" in body_dangling_tool["contents"][-1]["parts"][0]
    assert (
        body_dangling_tool["contents"][-1]["parts"][0]["functionResponse"]["name"]
        == "Bash"
    )



def test_incremental_sse_translators() -> None:
    async def fake_gemini_stream():
        yield {
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [
                            {
                                "thought": True,
                                "text": "Analyzing command...",
                                "thoughtSignature": "sig_stream_123",
                            }
                        ],
                    }
                }
            ],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 2},
        }
        yield {
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [
                            {"text": "Running uname now."},
                            {
                                "functionCall": {
                                    "id": "call_stream_1",
                                    "name": "Bash",
                                    "args": {"command": "uname -s"},
                                },
                                "thoughtSignature": "sig_stream_123",
                            },
                        ],
                    },
                    "finishReason": "STOP",
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 8,
                "thoughtsTokenCount": 3,
            },
        }

    async def run_all():
        store = ThoughtSignatureStore()
        anth_events = [
            ev
            async for ev in iter_anthropic_sse_from_gemini_chunks(
                fake_gemini_stream(),
                requested_model="gemini-3.8-flash",
                signature_store=store,
            )
        ]
        anth_raw = "".join(anth_events)
        assert "event: message_start" in anth_raw
        assert "thinking_delta" in anth_raw
        assert "signature_delta" in anth_raw
        assert "text_delta" in anth_raw
        assert "input_json_delta" in anth_raw
        assert '"stop_reason": "tool_use"' in anth_raw
        assert store.get("call_stream_1") == "sig_stream_123"

        chat_events = [
            ev
            async for ev in iter_openai_chat_sse_from_gemini_chunks(
                fake_gemini_stream(),
                requested_model="gpt-5-codex",
                signature_store=store,
            )
        ]
        chat_raw = "".join(chat_events)
        assert "chat.completion.chunk" in chat_raw
        assert '"finish_reason": "tool_calls"' in chat_raw
        assert chat_events[-1].strip() == "data: [DONE]"

        resp_events = [
            ev
            async for ev in iter_openai_responses_sse_from_gemini_chunks(
                fake_gemini_stream(),
                requested_model="gpt-5-codex",
                signature_store=store,
                custom_tool_names=set(),
            )
        ]
        resp_raw = "".join(resp_events)
        assert "event: response.created" in resp_raw
        assert "event: response.reasoning_summary_text.delta" in resp_raw
        assert "event: response.output_text.delta" in resp_raw
        assert "event: response.function_call_arguments.delta" in resp_raw
        assert "event: response.completed" in resp_raw

    asyncio.run(run_all())


def test_adc_strips_api_keys() -> None:
    os.environ["GEMINI_API_KEY"] = "should-be-stripped"
    os.environ["GOOGLE_API_KEY"] = "should-also-be-stripped"
    mgr = ADCAuthManager()
    assert "GEMINI_API_KEY" not in os.environ
    assert "GOOGLE_API_KEY" not in os.environ
    assert "GEMINI_API_KEY" in mgr.stripped_api_key_vars
    status = mgr.status_dict()
    assert status["api_key_used"] is False
    assert status["token_valid"] is True


def test_schema_sanitization_and_thinking_translation() -> None:
    raw_schema = {
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": False,
        "properties": {
            "command": {"type": "string", "default": "ls"},
            "timeout": {
                "type": ["integer", "null"],
                "exclusiveMinimum": 0,
                "exclusiveMaximum": 600,
            },
        },
        "required": ["command", "non_existent_prop"],
    }
    cleaned = sanitize_json_schema(raw_schema)
    assert "$schema" not in cleaned
    assert "additionalProperties" not in cleaned
    assert cleaned["type"] == "OBJECT"
    assert cleaned["required"] == ["command"]
    assert "default" not in cleaned["properties"]["command"]
    assert cleaned["properties"]["timeout"]["type"] == "INTEGER"
    assert cleaned["properties"]["timeout"]["nullable"] is True
    assert "exclusiveMinimum" not in cleaned["properties"]["timeout"]


def test_thought_signature_roundtrip() -> None:
    store = ThoughtSignatureStore()
    fake_gemini_resp = {
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "thought": True,
                            "text": "Checking files first.",
                            "thoughtSignature": "sig_opaque_xyz123",
                        },
                        {
                            "functionCall": {
                                "id": "call_abc",
                                "name": "Bash",
                                "args": {"command": "ls -la"},
                            },
                            "thoughtSignature": "sig_opaque_xyz123",
                        },
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 8},
    }
    anth_msg = gemini_to_anthropic_response(
        fake_gemini_resp, requested_model="gemini-3.8-flash", signature_store=store
    )
    assert anth_msg["stop_reason"] == "tool_use"
    assert store.get("call_abc") == "sig_opaque_xyz123"

    # Simulate Claude Code sending the next turn with tool_result (and even omitting thinking block)
    next_turn_payload = {
        "model": "gemini-4.0",
        "system": [
            {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}}
        ],
        "thinking": {"type": "enabled", "budget_tokens": 8000},
        "messages": [
            {"role": "user", "content": "List files"},
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "call_abc",
                        "name": "Bash",
                        "input": {"command": "ls -la"},
                    }
                ],
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_abc",
                        "content": "README.md\npyproject.toml",
                    }
                ],
            },
        ],
    }
    model_id, gemini_req = anthropic_to_gemini_request(next_turn_payload, signature_store=store)
    assert model_id == MODEL_GEMINI_4_ARGON
    assert gemini_req["generationConfig"]["temperature"] == 1.0
    assert gemini_req["generationConfig"]["thinkingConfig"] == {
        "includeThoughts": True,
        "thinkingLevel": "MEDIUM",
    }
    model_turn = gemini_req["contents"][1]
    assert model_turn["parts"][0]["thoughtSignature"] == "sig_opaque_xyz123"
    user_result_turn = gemini_req["contents"][2]
    assert user_result_turn["parts"][0]["functionResponse"]["name"] == "Bash"


def test_openai_chat_completions_roundtrip_and_sse() -> None:
    store = ThoughtSignatureStore()
    fake_gemini_resp = {
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "thought": True,
                            "text": "Planning shell execution.",
                            "thoughtSignature": "sig_chat_789",
                        },
                        {
                            "functionCall": {
                                "id": "call_chat_1",
                                "name": "exec_command",
                                "args": {"cmd": "pwd"},
                            },
                            "thoughtSignature": "sig_chat_789",
                        },
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 15, "candidatesTokenCount": 9, "thoughtsTokenCount": 4},
    }
    chat_resp = gemini_to_openai_chat_response(
        fake_gemini_resp, requested_model="gpt-5-codex", signature_store=store
    )
    assert chat_resp["object"] == "chat.completion"
    choice = chat_resp["choices"][0]
    assert choice["finish_reason"] == "tool_calls"
    assert choice["message"]["tool_calls"][0]["id"] == "call_chat_1"
    assert choice["message"]["tool_calls"][0]["function"]["name"] == "exec_command"
    assert store.get("call_chat_1") == "sig_chat_789"

    sse_events = openai_chat_to_sse_events(chat_resp)
    assert any("chat.completion.chunk" in ev for ev in sse_events)
    assert sse_events[-1].strip() == "data: [DONE]"

    # Next turn with role="tool"
    chat_turn2 = {
        "model": "gpt-5-codex",
        "reasoning_effort": "high",
        "messages": [
            {"role": "system", "content": "You are a helpful coding agent."},
            {"role": "user", "content": "Print working directory"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": choice["message"]["tool_calls"],
            },
            {
                "role": "tool",
                "tool_call_id": "call_chat_1",
                "content": "/home/eddieckc/Projects/claude-gemini-adc-router",
            },
        ],
    }
    model_id, gemini_req = openai_chat_to_gemini_request(chat_turn2, signature_store=store)
    assert model_id == MODEL_GEMINI_3_8_FLASH
    assert gemini_req["generationConfig"]["thinkingConfig"]["thinkingLevel"] == "HIGH"
    assert gemini_req["contents"][1]["parts"][0]["thoughtSignature"] == "sig_chat_789"
    assert gemini_req["contents"][2]["parts"][0]["functionResponse"]["name"] == "exec_command"


def test_openai_responses_codex_roundtrip_and_sse() -> None:
    store = ThoughtSignatureStore()
    codex_req_1 = {
        "model": "gemini-3.8-flash",
        "instructions": "You are Codex running on Gemini ADC.",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Run a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"],
                },
            },
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a file patch",
            },
        ],
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Patch README and run pwd"}],
            }
        ],
    }
    model_id, gemini_req, custom_tools = openai_responses_to_gemini_request(
        codex_req_1, signature_store=store
    )
    assert model_id == MODEL_GEMINI_3_8_FLASH
    assert "apply_patch" in custom_tools
    decls = gemini_req["tools"][0]["functionDeclarations"]
    assert {d["name"] for d in decls} == {"exec_command", "apply_patch"}

    fake_gemini_resp = {
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "thought": True,
                            "text": "I will apply the patch first.",
                            "thoughtSignature": "sig_codex_custom_999",
                        },
                        {
                            "functionCall": {
                                "id": "call_patch_1",
                                "name": "apply_patch",
                                "args": {"input": "*** Begin Patch\n*** End Patch"},
                            },
                            "thoughtSignature": "sig_codex_custom_999",
                        },
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 25, "candidatesTokenCount": 14, "thoughtsTokenCount": 5},
    }
    resp_obj = gemini_to_openai_responses_object(
        fake_gemini_resp,
        requested_model="gemini-3.8-flash",
        signature_store=store,
        request_contents=gemini_req["contents"],
        custom_tool_names=custom_tools,
    )
    assert resp_obj["object"] == "response"
    assert resp_obj["status"] == "completed"
    output_types = [item["type"] for item in resp_obj["output"]]
    assert "reasoning" in output_types
    assert "custom_tool_call" in output_types
    custom_call = [item for item in resp_obj["output"] if item["type"] == "custom_tool_call"][0]
    assert custom_call["name"] == "apply_patch"
    assert custom_call["input"] == "*** Begin Patch\n*** End Patch"

    sse_chunks = openai_responses_to_sse_events(resp_obj)
    joined_sse = "".join(sse_chunks)
    assert "event: response.created" in joined_sse
    assert "event: response.custom_tool_call_input.delta" in joined_sse
    assert "event: response.completed" in joined_sse

    # Turn 2: Codex sends back reasoning + custom_tool_call + custom_tool_call_output
    codex_req_2 = {
        "model": "gemini-3.8-flash",
        "tools": codex_req_1["tools"],
        "input": [
            *codex_req_1["input"],
            *resp_obj["output"],
            {
                "type": "custom_tool_call_output",
                "call_id": custom_call["call_id"],
                "output": "Patch applied cleanly",
            },
        ],
    }
    _, gemini_req_2, _ = openai_responses_to_gemini_request(codex_req_2, signature_store=store)
    model_parts = gemini_req_2["contents"][1]["parts"]
    assert any(p.get("thoughtSignature") == "sig_codex_custom_999" for p in model_parts)
    user_resp_part = gemini_req_2["contents"][2]["parts"][0]
    assert user_resp_part["functionResponse"]["name"] == "apply_patch"


def test_runtime_config_endpoint_and_plugin_manifests() -> None:
    client = TestClient(app)
    res = client.post(
        "/config",
        json={"primary_model": "gemini-4-argon", "reasoning_effort": "xhigh", "hot_swap": True},
    )
    assert res.status_code == 200
    body = res.json()
    assert body["primary_model"] == MODEL_GEMINI_4_ARGON
    assert body["reasoning_effort"] == "xhigh"
    assert body["hot_swap_active"] is True
    assert "stats" in body
    assert "total_requests" in body["stats"]

    # Reset back to default
    res2 = client.post(
        "/config",
        json={"primary_model": "gemini-3.8-flash", "reasoning_effort": "medium", "hot_swap": False},
    )
    assert res2.status_code == 200
    assert res2.json()["primary_model"] == MODEL_GEMINI_3_8_FLASH
    assert res2.json()["hot_swap_active"] is False

    # Verify Claude & Codex plugin manifests exist and are valid JSON
    root = Path(__file__).resolve().parent.parent
    claude_plugin = json.loads((root / ".claude-plugin" / "plugin.json").read_text())
    codex_plugin = json.loads((root / ".codex-plugin" / "plugin.json").read_text())
    codex_market = json.loads((root / ".agents" / "plugins" / "marketplace.json").read_text())
    assert claude_plugin["name"] == "gemini-adc-router"
    assert codex_plugin["name"] == "gemini-adc-router"
    assert codex_market["plugins"][0]["name"] == "gemini-adc-router"
    assert (root / "skills" / "gemini-adc-router" / "SKILL.md").is_file()


def test_live_vertex_adc_multi_turn_tool_and_stream() -> None:
    """End-to-end live test against Vertex AI via ADC for Anthropic `/v1/messages`."""
    client = TestClient(app)

    # 1. Health check
    health_resp = client.get("/health")
    assert health_resp.status_code == 200
    assert health_resp.json()["adc"]["api_key_used"] is False
    assert health_resp.json()["adc"]["token_valid"] is True

    # 2. Turn 1: Trigger a tool call on gemini-3.8-flash with Claude Code's schema quirks
    turn1_req = {
        "model": "gemini-3.8",
        "max_tokens": 1024,
        "system": [
            {
                "type": "text",
                "text": "You are a coding assistant. Always use the Bash tool when asked to run a command.",
                "cache_control": {"type": "ephemeral"},
            }
        ],
        "tools": [
            {
                "name": "Bash",
                "description": "Execute a shell command",
                "input_schema": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "additionalProperties": False,
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"}
                    },
                    "required": ["command"],
                },
            }
        ],
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [
            {"role": "user", "content": "Run `uname -s` using the Bash tool."}
        ],
    }
    r1 = client.post("/v1/messages", json=turn1_req)
    assert r1.status_code == 200, r1.text
    msg1 = r1.json()
    assert msg1["stop_reason"] == "tool_use"
    tool_blocks = [b for b in msg1["content"] if b["type"] == "tool_use"]
    assert len(tool_blocks) == 1
    tool_use_id = tool_blocks[0]["id"]

    # 3. Turn 2: Send tool_result back in streaming SSE mode (verifies thoughtSignature replay!)
    turn2_req = {
        **turn1_req,
        "stream": True,
        "messages": [
            *turn1_req["messages"],
            {"role": "assistant", "content": msg1["content"]},
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": "Linux",
                    }
                ],
            },
        ],
    }
    with client.stream("POST", "/v1/messages", json=turn2_req) as r2:
        assert r2.status_code == 200
        raw_sse = "".join(r2.iter_text())
    assert "event: message_start" in raw_sse
    assert "text_delta" in raw_sse
    assert "event: message_stop" in raw_sse


def test_live_vertex_adc_codex_responses_multi_turn_and_stream() -> None:
    """End-to-end live test against Vertex AI via ADC for OpenAI Codex `/v1/responses`."""
    client = TestClient(app)

    turn1_req = {
        "model": "gemini-3.8-flash",
        "instructions": "You are a coding agent. Always call the exec_command tool when asked to run a shell command.",
        "reasoning": {"effort": "low", "summary": "auto"},
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Execute a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string", "description": "Command to execute"}
                    },
                    "required": ["cmd"],
                },
            }
        ],
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Run `uname -s` using exec_command.",
                    }
                ],
            }
        ],
    }
    r1 = client.post("/v1/responses", json=turn1_req)
    assert r1.status_code == 200, r1.text
    resp1 = r1.json()
    assert resp1["object"] == "response"
    fn_calls = [item for item in resp1["output"] if item["type"] == "function_call"]
    assert len(fn_calls) >= 1
    call_id = fn_calls[0]["call_id"]

    # Turn 2: Send function_call_output back in streaming SSE mode
    turn2_req = {
        **turn1_req,
        "stream": True,
        "input": [
            *turn1_req["input"],
            *resp1["output"],
            {
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Linux\n",
            },
        ],
    }
    with client.stream("POST", "/v1/responses", json=turn2_req) as r2:
        assert r2.status_code == 200
        raw_sse = "".join(r2.iter_text())
    assert "event: response.created" in raw_sse
    assert "event: response.output_text.delta" in raw_sse
    assert "event: response.completed" in raw_sse
