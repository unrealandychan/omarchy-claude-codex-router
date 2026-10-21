"""FastAPI server exposing Anthropic (`/v1/messages`) and OpenAI/Codex (`/v1/responses`, `/v1/chat/completions`) APIs backed by Vertex AI Gemini (ADC-only)."""

from __future__ import annotations

import argparse
from collections.abc import AsyncIterator
import json
import logging
import os
import time
from typing import Any

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse, StreamingResponse
import httpx
import uvicorn

from router.adc_auth import ADCAuthManager
from router.translator import (
    MODEL_GEMINI_3_8_FLASH,
    MODEL_GEMINI_4_ARGON,
    SIGNATURE_STORE,
    anthropic_message_to_sse_events,
    anthropic_to_gemini_request,
    gemini_to_anthropic_response,
    gemini_to_openai_chat_response,
    gemini_to_openai_responses_object,
    iter_anthropic_sse_from_gemini_chunks,
    iter_openai_chat_sse_from_gemini_chunks,
    iter_openai_responses_sse_from_gemini_chunks,
    openai_chat_to_gemini_request,
    openai_chat_to_sse_events,
    openai_responses_to_gemini_request,
    openai_responses_to_sse_events,
    resolve_gemini_model,
)

logging.basicConfig(
    level=os.environ.get("LOG_LEVEL", "INFO").upper(),
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)
logger = logging.getLogger("claude-gemini-router")

app = FastAPI(
    title="Claude Code & OpenAI Codex -> Gemini 3.8 / 4.0 ADC Router",
    version="0.3.0",
)

auth_manager = ADCAuthManager()


class RouterMetrics:
    """In-memory session telemetry for routed Claude Code & OpenAI Codex requests."""

    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        self.total_requests: int = 0
        self.claude_requests: int = 0
        self.codex_requests: int = 0
        self.chat_requests: int = 0
        self.input_tokens: int = 0
        self.output_tokens: int = 0
        self.reasoning_tokens: int = 0
        self.cached_tokens: int = 0
        self.fallback_count: int = 0
        self.error_count: int = 0
        self.last_client: str | None = None
        self.last_requested_model: str | None = None
        self.last_vertex_model: str | None = None
        self.last_latency_ms: int | None = None
        self.last_request_at: int | None = None

    def record_request(
        self,
        client_kind: str,
        requested_model: str,
        vertex_model: str,
        latency_ms: int,
        input_tokens: int = 0,
        output_tokens: int = 0,
        reasoning_tokens: int = 0,
        cached_tokens: int = 0,
    ) -> None:
        self.total_requests += 1
        if client_kind == "claude":
            self.claude_requests += 1
        elif client_kind == "codex":
            self.codex_requests += 1
        else:
            self.chat_requests += 1

        self.input_tokens += max(0, input_tokens)
        self.output_tokens += max(0, output_tokens)
        self.reasoning_tokens += max(0, reasoning_tokens)
        self.cached_tokens += max(0, cached_tokens)
        self.last_client = client_kind
        self.last_requested_model = requested_model
        self.last_vertex_model = vertex_model
        self.last_latency_ms = max(0, latency_ms)
        self.last_request_at = int(time.time())

    def to_dict(self) -> dict[str, Any]:
        return {
            "total_requests": self.total_requests,
            "claude_requests": self.claude_requests,
            "codex_requests": self.codex_requests,
            "chat_requests": self.chat_requests,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "cached_tokens": self.cached_tokens,
            "total_tokens": self.input_tokens + self.output_tokens,
            "fallback_count": self.fallback_count,
            "error_count": self.error_count,
            "last_client": self.last_client,
            "last_requested_model": self.last_requested_model,
            "last_vertex_model": self.last_vertex_model,
            "last_latency_ms": self.last_latency_ms,
            "last_request_at": self.last_request_at,
        }


ROUTER_METRICS = RouterMetrics()


def _vertex_endpoint_url(
    model_id: str,
    method: str = "generateContent",
    project_id: str | None = None,
    location: str | None = None,
) -> str:
    """Build the Vertex AI (or local keyless GenAI proxy) URL for `model_id`."""
    custom_upstream = os.environ.get("GEMINI_UPSTREAM_URL", "").rstrip("/")
    if custom_upstream:
        # e.g. http://localhost:7242/genai -> /v1beta/models/{model_id}:{method}
        return f"{custom_upstream}/v1beta/models/{model_id}:{method}"

    proj = project_id or auth_manager.project_id
    loc = location or auth_manager.location
    host = (
        "aiplatform.googleapis.com"
        if loc in ("global", "us", "eu")
        else f"{loc}-aiplatform.googleapis.com"
    )
    return (
        f"https://{host}/v1beta1/projects/{proj}/locations/{loc}"
        f"/publishers/google/models/{model_id}:{method}"
    )


def _build_upstream_headers() -> dict[str, str]:
    """Build upstream HTTP headers using ADC Bearer token (zero API keys)."""
    headers = {"Content-Type": "application/json"}
    if not os.environ.get("GEMINI_UPSTREAM_URL"):
        token = auth_manager.get_access_token()
        headers["Authorization"] = f"Bearer {token}"
    return headers


async def _call_gemini_unary(
    model_id: str,
    gemini_body: dict[str, Any],
) -> tuple[str, dict[str, Any]]:
    """Call Vertex AI `:generateContent` with automatic fallback from 4.0 -> 3.8 on 403/404/429."""
    fallback_enabled = os.environ.get("GEMINI_4_FALLBACK_TO_3_8", "true").lower() in (
        "1",
        "true",
        "yes",
    )
    url = _vertex_endpoint_url(model_id, "generateContent")
    headers = _build_upstream_headers()

    async with httpx.AsyncClient(timeout=300.0) as client:
        resp = await client.post(url, json=gemini_body, headers=headers)
        if (
            resp.status_code in (403, 404, 429)
            and model_id == MODEL_GEMINI_4_ARGON
            and fallback_enabled
        ):
            ROUTER_METRICS.fallback_count += 1
            logger.warning(
                "Vertex AI returned HTTP %s for %s on project %s; falling back to %s",
                resp.status_code,
                model_id,
                auth_manager.project_id,
                MODEL_GEMINI_3_8_FLASH,
            )
            fallback_url = _vertex_endpoint_url(MODEL_GEMINI_3_8_FLASH, "generateContent")
            resp = await client.post(fallback_url, json=gemini_body, headers=headers)
            model_id = MODEL_GEMINI_3_8_FLASH

        if resp.status_code >= 400:
            ROUTER_METRICS.error_count += 1
            logger.error(
                "Upstream Vertex AI error (%s) for %s: %s",
                resp.status_code,
                model_id,
                resp.text[:500],
            )
            raise HTTPException(
                status_code=resp.status_code,
                detail={
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": f"Vertex AI ({model_id}) HTTP {resp.status_code}: {resp.text}",
                    },
                },
            )
        return model_id, resp.json()


async def _iter_gemini_upstream_chunks(
    model_id: str,
    gemini_body: dict[str, Any],
    *,
    client_kind: str = "claude",
    requested_model: str = "",
) -> AsyncIterator[dict[str, Any]]:
    """Stream `GenerateContentResponse` JSON chunks from Vertex AI `:streamGenerateContent?alt=sse`.

    Automatically falls back from `gemini-4-argon` to `gemini-3.8-flash` on 403/404/429,
    and falls back to unary `:generateContent` if upstream SSE streaming is unavailable.
    """
    t0 = time.monotonic()
    effective_model = model_id
    last_usage_chunk: dict[str, Any] = {}
    yielded_any = False

    def _record_stream_metrics() -> None:
        if not yielded_any:
            return
        latency_ms = int((time.monotonic() - t0) * 1000)
        in_tok, out_tok, thought_tok, cached_tok = _extract_usage_counts(last_usage_chunk)
        ROUTER_METRICS.record_request(
            client_kind=client_kind,
            requested_model=requested_model or model_id,
            vertex_model=effective_model,
            latency_ms=latency_ms,
            input_tokens=in_tok,
            output_tokens=out_tok,
            reasoning_tokens=thought_tok,
            cached_tokens=cached_tok,
        )

    use_upstream_stream = os.environ.get("GEMINI_UPSTREAM_STREAMING", "1").lower() not in (
        "0",
        "false",
        "no",
    )
    if not use_upstream_stream:
        effective_model, unary_resp = await _call_gemini_unary(model_id, gemini_body)
        if unary_resp.get("usageMetadata"):
            last_usage_chunk = unary_resp
        yielded_any = True
        try:
            yield unary_resp
        finally:
            _record_stream_metrics()
        return

    fallback_enabled = os.environ.get("GEMINI_4_FALLBACK_TO_3_8", "true").lower() in (
        "1",
        "true",
        "yes",
    )
    stream_url = f"{_vertex_endpoint_url(model_id, 'streamGenerateContent')}?alt=sse"
    headers = _build_upstream_headers()

    try:
        async with httpx.AsyncClient(timeout=300.0) as client:
            async with client.stream(
                "POST", stream_url, json=gemini_body, headers=headers
            ) as resp:
                if (
                    resp.status_code in (403, 404, 429)
                    and model_id == MODEL_GEMINI_4_ARGON
                    and fallback_enabled
                ):
                    ROUTER_METRICS.fallback_count += 1
                    effective_model = MODEL_GEMINI_3_8_FLASH
                    logger.warning(
                        "Vertex AI stream returned HTTP %s for %s; falling back to %s",
                        resp.status_code,
                        model_id,
                        MODEL_GEMINI_3_8_FLASH,
                    )
                    fb_url = f"{_vertex_endpoint_url(MODEL_GEMINI_3_8_FLASH, 'streamGenerateContent')}?alt=sse"
                    async with client.stream(
                        "POST", fb_url, json=gemini_body, headers=headers
                    ) as fb_resp:
                        if fb_resp.status_code == 200:
                            try:
                                async for line in fb_resp.aiter_lines():
                                    stripped = line.strip()
                                    if not stripped.startswith("data:"):
                                        continue
                                    raw_json = stripped[5:].strip()
                                    if not raw_json or raw_json == "[DONE]":
                                        continue
                                    chunk_obj = json.loads(raw_json)
                                    if chunk_obj.get("usageMetadata"):
                                        last_usage_chunk = chunk_obj
                                    yielded_any = True
                                    yield chunk_obj
                            finally:
                                if yielded_any:
                                    _record_stream_metrics()
                            if yielded_any:
                                return
                elif resp.status_code == 200:
                    try:
                        async for line in resp.aiter_lines():
                            stripped = line.strip()
                            if not stripped.startswith("data:"):
                                continue
                            raw_json = stripped[5:].strip()
                            if not raw_json or raw_json == "[DONE]":
                                continue
                            chunk_obj = json.loads(raw_json)
                            if chunk_obj.get("usageMetadata"):
                                last_usage_chunk = chunk_obj
                            yielded_any = True
                            yield chunk_obj
                    finally:
                        if yielded_any:
                            _record_stream_metrics()
                    if yielded_any:
                        return
    except Exception as exc:
        if yielded_any:
            logger.error("Error mid-stream from Vertex AI: %s", exc)
            return
        logger.debug("Upstream SSE stream unavailable (%s); falling back to unary", exc)

    # Fallback to unary `:generateContent` if streaming endpoint was unsupported or returned 0 chunks
    effective_model, unary_resp = await _call_gemini_unary(model_id, gemini_body)
    if unary_resp.get("usageMetadata"):
        last_usage_chunk = unary_resp
    yielded_any = True
    try:
        yield unary_resp
    finally:
        _record_stream_metrics()


def _sse_response_from_iterator(iterator: AsyncIterator[str]) -> StreamingResponse:
    """Wrap an async SSE event generator in a FastAPI StreamingResponse."""
    return StreamingResponse(
        iterator,
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


def _extract_usage_counts(gemini_resp: dict[str, Any]) -> tuple[int, int, int, int]:
    usage = gemini_resp.get("usageMetadata") or {}
    in_tok = int(usage.get("promptTokenCount", 0))
    cached_tok = int(usage.get("cachedContentTokenCount", 0))
    thought_tok = int(usage.get("thoughtsTokenCount", 0))
    cand_tok = int(usage.get("candidatesTokenCount", 0))
    out_tok = cand_tok + thought_tok
    return in_tok, out_tok, thought_tok, cached_tok


@app.get("/health")
@app.get("/v1/health")
async def health() -> dict[str, Any]:
    """Return ADC status, active model routing map, session telemetry, and supported protocols."""
    raw_primary = os.environ.get("GEMINI_PRIMARY_MODEL", MODEL_GEMINI_3_8_FLASH).strip().lower()
    primary_model = (
        MODEL_GEMINI_4_ARGON
        if ("4" in raw_primary or "argon" in raw_primary)
        else MODEL_GEMINI_3_8_FLASH
    )
    reasoning_effort = (
        os.environ.get("GEMINI_REASONING_EFFORT", "medium").strip().lower() or "medium"
    )
    hot_swap_active = os.environ.get("GEMINI_HOT_SWAP_ACTIVE", "").strip() == "1"
    return {
        "status": "ok",
        "adc": auth_manager.status_dict(),
        "primary_model": primary_model,
        "reasoning_effort": reasoning_effort,
        "hot_swap_active": hot_swap_active,
        "stats": ROUTER_METRICS.to_dict(),
        "protocols": [
            "anthropic-messages",
            "openai-responses",
            "openai-chat-completions",
        ],
        "models": {
            "gemini-3.8": MODEL_GEMINI_3_8_FLASH,
            "gemini-3.8-flash": MODEL_GEMINI_3_8_FLASH,
            "gemini-4.0": MODEL_GEMINI_4_ARGON,
            "gemini-4.0-pro": MODEL_GEMINI_4_ARGON,
            "gemini-4-argon": MODEL_GEMINI_4_ARGON,
            "claude-haiku-4-5": MODEL_GEMINI_3_8_FLASH,
            "claude-sonnet-4-6": primary_model,
            "claude-opus-4-6": primary_model,
            "gpt-5-codex": primary_model,
            "gpt-5-pro": primary_model,
        },
    }


@app.post("/config")
@app.post("/v1/config")
async def update_config(request: Request) -> dict[str, Any]:
    """Update runtime model, reasoning effort, and hot-swap state without restarting the server."""
    payload = await request.json()
    if payload.get("reset_stats") is True:
        ROUTER_METRICS.reset()

    if "primary_model" in payload:
        raw_model = str(payload["primary_model"] or "").strip()
        if raw_model:
            resolved = (
                MODEL_GEMINI_4_ARGON
                if "4" in raw_model or "argon" in raw_model.lower()
                else MODEL_GEMINI_3_8_FLASH
            )
            os.environ["GEMINI_PRIMARY_MODEL"] = resolved
            if payload.get("hot_swap", True):
                os.environ["GEMINI_HOT_SWAP_ACTIVE"] = "1"
        else:
            os.environ.pop("GEMINI_PRIMARY_MODEL", None)
            os.environ.pop("GEMINI_HOT_SWAP_ACTIVE", None)

    if "reasoning_effort" in payload:
        effort = str(payload["reasoning_effort"] or "").strip().lower()
        if effort in ("low", "medium", "high", "xhigh", "max", "minimal"):
            os.environ["GEMINI_REASONING_EFFORT"] = effort
            if payload.get("hot_swap", True):
                os.environ["GEMINI_HOT_SWAP_ACTIVE"] = "1"
        elif effort == "":
            os.environ.pop("GEMINI_REASONING_EFFORT", None)

    if "hot_swap" in payload and not payload["hot_swap"]:
        os.environ.pop("GEMINI_HOT_SWAP_ACTIVE", None)

    return await health()


@app.get("/v1/models")
@app.get("/models")
async def list_models() -> dict[str, Any]:
    """Return Anthropic/OpenAI-style model catalog for Claude Code and OpenAI Codex."""
    return {
        "object": "list",
        "data": [
            {
                "id": "gemini-3.8-flash",
                "object": "model",
                "type": "model",
                "created": 1767225600,
                "owned_by": "google-vertex-adc",
                "display_name": "Gemini 3.8 Flash (Vertex AI ADC)",
            },
            {
                "id": "gemini-4.0",
                "object": "model",
                "type": "model",
                "created": 1767225600,
                "owned_by": "google-vertex-adc",
                "display_name": "Gemini 4.0 / gemini-4-argon (Vertex AI ADC)",
            },
            {
                "id": "gemini-4-argon",
                "object": "model",
                "type": "model",
                "created": 1767225600,
                "owned_by": "google-vertex-adc",
                "display_name": "Gemini 4 Argon (Vertex AI ADC)",
            },
            {
                "id": "gpt-5-codex",
                "object": "model",
                "type": "model",
                "created": 1767225600,
                "owned_by": "google-vertex-adc",
                "display_name": "Codex -> Gemini ADC Alias",
            },
        ],
    }


@app.post("/v1/messages/count_tokens")
@app.post("/messages/count_tokens")
async def count_tokens(request: Request) -> dict[str, Any]:
    """Handle Claude Code's `/v1/messages/count_tokens` pre-flight check via Vertex AI."""
    payload = await request.json()
    gemini_model, gemini_body = anthropic_to_gemini_request(payload)

    count_payload: dict[str, Any] = {"contents": gemini_body.get("contents", [])}
    if "systemInstruction" in gemini_body:
        count_payload["systemInstruction"] = gemini_body["systemInstruction"]
    if "tools" in gemini_body:
        count_payload["tools"] = gemini_body["tools"]

    try:
        url = _vertex_endpoint_url(gemini_model, "countTokens")
        headers = _build_upstream_headers()
        async with httpx.AsyncClient(timeout=30.0) as client:
            resp = await client.post(url, json=count_payload, headers=headers)
            if resp.status_code == 200:
                data = resp.json()
                return {"input_tokens": int(data.get("totalTokens", 0))}
    except Exception as exc:
        logger.debug("countTokens fallback estimation due to: %s", exc)

    # Fallback character-based heuristic (~4 chars/token) so Claude Code never blocks
    serialized = json.dumps(count_payload)
    return {"input_tokens": max(1, len(serialized) // 4)}


@app.post("/v1/messages")
@app.post("/messages")
async def create_message(request: Request) -> Any:
    """Translate Anthropic `/v1/messages` request to Vertex AI Gemini and return JSON or live SSE."""
    payload = await request.json()
    requested_model = str(payload.get("model") or MODEL_GEMINI_3_8_FLASH)
    stream = bool(payload.get("stream", False))

    gemini_model, gemini_body = anthropic_to_gemini_request(payload, SIGNATURE_STORE)
    logger.info(
        "Routing Claude Code (/v1/messages): requested=%s -> vertex=%s (stream=%s, tools=%d)",
        requested_model,
        gemini_model,
        stream,
        len(gemini_body.get("tools", [{}])[0].get("functionDeclarations", []))
        if "tools" in gemini_body
        else 0,
    )

    if stream:
        chunk_iter = _iter_gemini_upstream_chunks(
            gemini_model,
            gemini_body,
            client_kind="claude",
            requested_model=requested_model,
        )
        return _sse_response_from_iterator(
            iter_anthropic_sse_from_gemini_chunks(
                chunk_iter,
                requested_model=requested_model,
                signature_store=SIGNATURE_STORE,
            )
        )

    t0 = time.monotonic()
    effective_model, gemini_resp = await _call_gemini_unary(gemini_model, gemini_body)
    latency_ms = int((time.monotonic() - t0) * 1000)
    in_tok, out_tok, thought_tok, cached_tok = _extract_usage_counts(gemini_resp)
    ROUTER_METRICS.record_request(
        client_kind="claude",
        requested_model=requested_model,
        vertex_model=effective_model,
        latency_ms=latency_ms,
        input_tokens=in_tok,
        output_tokens=out_tok,
        reasoning_tokens=thought_tok,
        cached_tokens=cached_tok,
    )
    anthropic_msg = gemini_to_anthropic_response(
        gemini_resp,
        requested_model=requested_model,
        signature_store=SIGNATURE_STORE,
    )
    return JSONResponse(content=anthropic_msg)


@app.post("/v1/chat/completions")
@app.post("/chat/completions")
async def create_chat_completion(request: Request) -> Any:
    """Translate OpenAI `/v1/chat/completions` request to Vertex AI Gemini and return JSON or live SSE."""
    payload = await request.json()
    requested_model = str(payload.get("model") or MODEL_GEMINI_3_8_FLASH)
    stream = bool(payload.get("stream", False))

    gemini_model, gemini_body = openai_chat_to_gemini_request(payload, SIGNATURE_STORE)
    logger.info(
        "Routing OpenAI Chat (/v1/chat/completions): requested=%s -> vertex=%s (stream=%s, tools=%d)",
        requested_model,
        gemini_model,
        stream,
        len(gemini_body.get("tools", [{}])[0].get("functionDeclarations", []))
        if "tools" in gemini_body
        else 0,
    )

    if stream:
        chunk_iter = _iter_gemini_upstream_chunks(
            gemini_model,
            gemini_body,
            client_kind="chat",
            requested_model=requested_model,
        )
        return _sse_response_from_iterator(
            iter_openai_chat_sse_from_gemini_chunks(
                chunk_iter,
                requested_model=requested_model,
                signature_store=SIGNATURE_STORE,
            )
        )

    t0 = time.monotonic()
    effective_model, gemini_resp = await _call_gemini_unary(gemini_model, gemini_body)
    latency_ms = int((time.monotonic() - t0) * 1000)
    in_tok, out_tok, thought_tok, cached_tok = _extract_usage_counts(gemini_resp)
    ROUTER_METRICS.record_request(
        client_kind="chat",
        requested_model=requested_model,
        vertex_model=effective_model,
        latency_ms=latency_ms,
        input_tokens=in_tok,
        output_tokens=out_tok,
        reasoning_tokens=thought_tok,
        cached_tokens=cached_tok,
    )
    chat_resp = gemini_to_openai_chat_response(
        gemini_resp,
        requested_model=requested_model,
        signature_store=SIGNATURE_STORE,
    )
    return JSONResponse(content=chat_resp)


@app.post("/v1/responses")
@app.post("/responses")
async def create_response(request: Request) -> Any:
    """Translate OpenAI Codex `/v1/responses` request to Vertex AI Gemini and return JSON or live SSE."""
    payload = await request.json()
    requested_model = str(payload.get("model") or MODEL_GEMINI_3_8_FLASH)
    stream = bool(payload.get("stream", False))

    gemini_model, gemini_body, custom_tool_names = openai_responses_to_gemini_request(
        payload, SIGNATURE_STORE
    )
    logger.info(
        "Routing OpenAI Codex (/v1/responses): requested=%s -> vertex=%s (stream=%s, tools=%d, custom_tools=%d)",
        requested_model,
        gemini_model,
        stream,
        len(gemini_body.get("tools", [{}])[0].get("functionDeclarations", []))
        if "tools" in gemini_body
        else 0,
        len(custom_tool_names),
    )

    if stream:
        chunk_iter = _iter_gemini_upstream_chunks(
            gemini_model,
            gemini_body,
            client_kind="codex",
            requested_model=requested_model,
        )
        return _sse_response_from_iterator(
            iter_openai_responses_sse_from_gemini_chunks(
                chunk_iter,
                requested_model=requested_model,
                signature_store=SIGNATURE_STORE,
                request_contents=gemini_body.get("contents"),
                custom_tool_names=custom_tool_names,
            )
        )

    t0 = time.monotonic()
    effective_model, gemini_resp = await _call_gemini_unary(gemini_model, gemini_body)
    latency_ms = int((time.monotonic() - t0) * 1000)
    in_tok, out_tok, thought_tok, cached_tok = _extract_usage_counts(gemini_resp)
    ROUTER_METRICS.record_request(
        client_kind="codex",
        requested_model=requested_model,
        vertex_model=effective_model,
        latency_ms=latency_ms,
        input_tokens=in_tok,
        output_tokens=out_tok,
        reasoning_tokens=thought_tok,
        cached_tokens=cached_tok,
    )
    response_obj = gemini_to_openai_responses_object(
        gemini_resp,
        requested_model=requested_model,
        signature_store=SIGNATURE_STORE,
        request_contents=gemini_body.get("contents"),
        custom_tool_names=custom_tool_names,
    )
    return JSONResponse(content=response_obj)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run the ADC-only Claude Code & OpenAI Codex -> Gemini 3.8 / 4.0 Router"
    )
    parser.add_argument("--host", default=os.environ.get("ROUTER_HOST", "127.0.0.1"))
    parser.add_argument(
        "--port", type=int, default=int(os.environ.get("ROUTER_PORT", "4000"))
    )
    parser.add_argument("--project", default=None, help="Override GCP Project ID")
    parser.add_argument("--location", default=None, help="Override Vertex AI location")
    args = parser.parse_args()

    if args.project:
        auth_manager.project_id = args.project
    if args.location:
        auth_manager.location = args.location

    status = auth_manager.status_dict()
    logger.info(
        "Starting ADC Router on http://%s:%d | project=%s | location=%s | api_key_used=%s",
        args.host,
        args.port,
        status["project_id"],
        status["location"],
        status["api_key_used"],
    )
    uvicorn.run(app, host=args.host, port=args.port, log_level="info")


if __name__ == "__main__":
    main()
