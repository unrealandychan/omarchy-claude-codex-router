use crate::adc_auth::ADCAuthManager;
use crate::translator::{
    anthropic_to_gemini_request, extract_candidate_parts, format_sse, gemini_to_anthropic_response,
    gemini_to_openai_chat_response, gemini_to_openai_responses, openai_chat_to_gemini_request,
    openai_responses_to_gemini_request, resolve_gemini_model, AnthropicStreamState,
    DEFAULT_GEMINI_MODEL, SIGNATURE_STORE,
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use uuid::Uuid;

fn short_id(len: usize) -> String {
    Uuid::new_v4().simple().to_string().chars().take(len).collect()
}

pub fn fallback_model_for(model: &str) -> Option<&'static str> {
    match model {
        "gemini-4-argon" => Some("gemini-3.8-flash"),
        _ => None,
    }
}

pub struct RouterMetrics {
    inner: Mutex<MetricsInner>,
    started_at: Instant,
}

struct MetricsInner {
    requests_total: u64,
    requests_ok: u64,
    requests_err: u64,
    claude_requests: u64,
    codex_requests: u64,
    chat_requests: u64,
    tokens_in: u64,
    tokens_out: u64,
    reasoning_tokens: u64,
    cached_tokens: u64,
    fallback_count: u64,
    active_model: String,
    default_model_override: Option<String>,
    reasoning_effort: String,
    hot_swap_active: bool,
    last_client: Option<String>,
    last_requested_model: Option<String>,
    last_vertex_model: Option<String>,
    last_latency_ms: Option<u64>,
    last_request_at: Option<u64>,
}

impl RouterMetrics {
    pub fn new() -> Self {
        let env_model = std::env::var("ROUTER_DEFAULT_MODEL")
            .or_else(|_| std::env::var("GEMINI_PRIMARY_MODEL"))
            .ok()
            .filter(|s| !s.trim().is_empty());
        let active = env_model
            .clone()
            .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
        let effort = std::env::var("GEMINI_REASONING_EFFORT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "medium".to_string());
        let hot_swap = std::env::var("GEMINI_HOT_SWAP_ACTIVE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        Self {
            inner: Mutex::new(MetricsInner {
                requests_total: 0,
                requests_ok: 0,
                requests_err: 0,
                claude_requests: 0,
                codex_requests: 0,
                chat_requests: 0,
                tokens_in: 0,
                tokens_out: 0,
                reasoning_tokens: 0,
                cached_tokens: 0,
                fallback_count: 0,
                active_model: active,
                default_model_override: env_model,
                reasoning_effort: effort,
                hot_swap_active: hot_swap,
                last_client: None,
                last_requested_model: None,
                last_vertex_model: None,
                last_latency_ms: None,
                last_request_at: None,
            }),
            started_at: Instant::now(),
        }
    }

    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.requests_total = 0;
        g.requests_ok = 0;
        g.requests_err = 0;
        g.claude_requests = 0;
        g.codex_requests = 0;
        g.chat_requests = 0;
        g.tokens_in = 0;
        g.tokens_out = 0;
        g.reasoning_tokens = 0;
        g.cached_tokens = 0;
        g.fallback_count = 0;
        g.last_client = None;
        g.last_requested_model = None;
        g.last_vertex_model = None;
        g.last_latency_ms = None;
        g.last_request_at = None;
    }

    pub fn record_request(&self, model: &str) {
        self.record_client_request("claude", model, model);
    }

    pub fn record_client_request(&self, client_kind: &str, requested_model: &str, vertex_model: &str) {
        let mut g = self.inner.lock().unwrap();
        g.requests_total += 1;
        match client_kind {
            "claude" => g.claude_requests += 1,
            "codex" => g.codex_requests += 1,
            _ => g.chat_requests += 1,
        }
        g.active_model = vertex_model.to_string();
        g.last_client = Some(client_kind.to_string());
        g.last_requested_model = Some(requested_model.to_string());
        g.last_vertex_model = Some(vertex_model.to_string());
        g.last_request_at = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }

    pub fn record_success(&self, tokens_in: u64, tokens_out: u64) {
        let mut g = self.inner.lock().unwrap();
        g.requests_ok += 1;
        g.tokens_in += tokens_in;
        g.tokens_out += tokens_out;
    }

    pub fn record_error(&self) {
        let mut g = self.inner.lock().unwrap();
        g.requests_err += 1;
    }

    pub fn set_default_model(&self, model: &str) -> String {
        let resolved = resolve_gemini_model(Some(model));
        let mut g = self.inner.lock().unwrap();
        g.default_model_override = Some(resolved.clone());
        g.active_model = resolved.clone();
        resolved
    }

    pub fn set_reasoning_effort(&self, effort: &str) {
        self.inner.lock().unwrap().reasoning_effort = effort.to_string();
    }

    pub fn set_hot_swap_active(&self, active: bool) {
        self.inner.lock().unwrap().hot_swap_active = active;
    }

    pub fn get_default_model_override(&self) -> Option<String> {
        self.inner.lock().unwrap().default_model_override.clone()
    }

    pub fn get_active_model(&self) -> String {
        self.inner.lock().unwrap().active_model.clone()
    }

    pub fn get_reasoning_effort(&self) -> String {
        self.inner.lock().unwrap().reasoning_effort.clone()
    }

    pub fn get_hot_swap_active(&self) -> bool {
        self.inner.lock().unwrap().hot_swap_active
    }

    pub fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "requests_total": g.requests_total,
            "requests_ok": g.requests_ok,
            "requests_err": g.requests_err,
            "tokens_in": g.tokens_in,
            "tokens_out": g.tokens_out,
            "active_model": g.active_model,
            "default_model_override": g.default_model_override,
            "uptime_seconds": self.started_at.elapsed().as_secs(),
        })
    }

    pub fn stats_snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "total_requests": g.requests_total,
            "claude_requests": g.claude_requests,
            "codex_requests": g.codex_requests,
            "chat_requests": g.chat_requests,
            "input_tokens": g.tokens_in,
            "output_tokens": g.tokens_out,
            "reasoning_tokens": g.reasoning_tokens,
            "cached_tokens": g.cached_tokens,
            "total_tokens": g.tokens_in + g.tokens_out,
            "fallback_count": g.fallback_count,
            "error_count": g.requests_err,
            "last_client": g.last_client,
            "last_requested_model": g.last_requested_model,
            "last_vertex_model": g.last_vertex_model,
            "last_latency_ms": g.last_latency_ms,
            "last_request_at": g.last_request_at,
        })
    }
}

impl Default for RouterMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct UpstreamError {
    pub status_code: u16,
    pub message: String,
    pub details: Option<Value>,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vertex AI HTTP {}: {}", self.status_code, self.message)
    }
}

impl std::error::Error for UpstreamError {}

pub struct VertexGeminiUpstream {
    pub auth: Arc<ADCAuthManager>,
    pub client: reqwest::Client,
    pub base_url_override: Option<String>,
}

impl VertexGeminiUpstream {
    pub fn new(auth: Arc<ADCAuthManager>) -> Self {
        for key in ["NO_PROXY", "no_proxy"] {
            let current = std::env::var(key).unwrap_or_default();
            if !current.contains("127.0.0.1") {
                let updated = if current.trim().is_empty() {
                    "127.0.0.1,localhost,::1".to_string()
                } else {
                    format!("{},127.0.0.1,localhost,::1", current)
                };
                std::env::set_var(key, updated);
            }
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            auth,
            client,
            base_url_override: None,
        }
    }

    pub fn build_endpoint_url(&self, model: &str, stream: bool) -> Result<String> {
        let method = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        if let Ok(custom) = std::env::var("GEMINI_UPSTREAM_URL") {
            let trimmed = custom.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                return Ok(format!("{}/v1beta/models/{}:{}", trimmed, model, method));
            }
        }
        let project = self.auth.resolve_project_id().ok_or_else(|| {
            anyhow!(
                "No Google Cloud project ID found. Set GOOGLE_CLOUD_PROJECT or run: gcloud config set project <PROJECT_ID>"
            )
        })?;
        let location = &self.auth.location;
        let host = if let Some(ref base) = self.base_url_override {
            base.trim_end_matches('/').to_string()
        } else if location == "global" || location == "us" || location == "eu" {
            "https://aiplatform.googleapis.com".to_string()
        } else {
            format!("https://{}-aiplatform.googleapis.com", location)
        };
        Ok(format!(
            "{}/v1beta1/projects/{}/locations/{}/publishers/google/models/{}:{}",
            host, project, location, model, method
        ))
    }

    pub async fn generate_content(
        &self,
        model: &str,
        payload: &Value,
    ) -> std::result::Result<Value, UpstreamError> {
        let url = self.build_endpoint_url(model, false).map_err(|e| UpstreamError {
            status_code: 500,
            message: e.to_string(),
            details: None,
        })?;
        let headers = self.auth.get_auth_headers().await.map_err(|e| UpstreamError {
            status_code: 401,
            message: e.to_string(),
            details: None,
        })?;

        let mut req = self.client.post(&url).json(payload);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let resp = req.send().await.map_err(|e| UpstreamError {
            status_code: 502,
            message: e.to_string(),
            details: None,
        })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status >= 400 {
            let details = serde_json::from_str::<Value>(&text).ok();
            return Err(UpstreamError {
                status_code: status,
                message: format!("Vertex AI error ({}): {}", status, text),
                details,
            });
        }
        serde_json::from_str::<Value>(&text).map_err(|e| UpstreamError {
            status_code: 502,
            message: format!("Invalid JSON from Vertex AI: {}", e),
            details: None,
        })
    }

    pub async fn stream_generate_content(
        &self,
        model: &str,
        payload: &Value,
    ) -> std::result::Result<mpsc::Receiver<std::result::Result<Value, UpstreamError>>, UpstreamError>
    {
        let url = self.build_endpoint_url(model, true).map_err(|e| UpstreamError {
            status_code: 500,
            message: e.to_string(),
            details: None,
        })?;
        let headers = self.auth.get_auth_headers().await.map_err(|e| UpstreamError {
            status_code: 401,
            message: e.to_string(),
            details: None,
        })?;

        let mut req = self.client.post(&url).json(payload);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let mut resp = req.send().await.map_err(|e| UpstreamError {
            status_code: 502,
            message: e.to_string(),
            details: None,
        })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            let details = serde_json::from_str::<Value>(&text).ok();
            return Err(UpstreamError {
                status_code: status,
                message: format!("Vertex AI stream error ({}): {}", status, text),
                details,
            });
        }

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut buffer = String::new();
            while let Ok(Some(chunk)) = resp.chunk().await {
                let s = String::from_utf8_lossy(&chunk);
                buffer.push_str(&s);
                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer.drain(..=pos);
                    if let Some(data_str) = line.strip_prefix("data:") {
                        let data_str = data_str.trim();
                        if !data_str.is_empty() && data_str != "[DONE]" {
                            if let Ok(parsed) = serde_json::from_str::<Value>(data_str) {
                                if tx.send(Ok(parsed)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let rem = buffer.trim();
            if let Some(data_str) = rem.strip_prefix("data:") {
                let data_str = data_str.trim();
                if !data_str.is_empty() && data_str != "[DONE]" {
                    if let Ok(parsed) = serde_json::from_str::<Value>(data_str) {
                        let _ = tx.send(Ok(parsed)).await;
                    }
                }
            }
        });

        Ok(rx)
    }
}

pub enum RouterResponse {
    Json(u16, Value),
    Sse(mpsc::Receiver<String>),
}

pub struct RouterServer {
    pub auth: Arc<ADCAuthManager>,
    pub upstream: Arc<VertexGeminiUpstream>,
    pub metrics: Arc<RouterMetrics>,
}

impl RouterServer {
    pub fn new(project_id: Option<String>, location: Option<String>) -> Self {
        let auth = Arc::new(ADCAuthManager::new(project_id, location));
        let upstream = Arc::new(VertexGeminiUpstream::new(auth.clone()));
        let metrics = Arc::new(RouterMetrics::new());
        Self {
            auth,
            upstream,
            metrics,
        }
    }

    pub fn apply_model_override(&self, mut body: Value) -> Value {
        if let Some(override_model) = self.metrics.get_default_model_override() {
            if let Some(obj) = body.as_object_mut() {
                let req_model = obj
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if req_model.is_empty() || !req_model.starts_with("gemini-") {
                    obj.insert("model".to_string(), Value::String(override_model));
                }
            }
        }
        body
    }

    pub async fn generate_with_fallback(
        &self,
        model: &str,
        payload: &Value,
    ) -> std::result::Result<(String, Value), UpstreamError> {
        match self.upstream.generate_content(model, payload).await {
            Ok(resp) => Ok((model.to_string(), resp)),
            Err(err) => {
                if let Some(fallback) = fallback_model_for(model) {
                    if matches!(err.status_code, 400 | 404 | 429 | 500 | 503) {
                        let resp = self.upstream.generate_content(fallback, payload).await?;
                        return Ok((fallback.to_string(), resp));
                    }
                }
                Err(err)
            }
        }
    }

    pub async fn handle_request(
        &self,
        method: &str,
        path: &str,
        body_bytes: &[u8],
    ) -> RouterResponse {
        let clean_path = path.split('?').next().unwrap_or(path);
        match (method, clean_path) {
            ("GET", "/health") | ("GET", "/v1/health") => {
                let adc_status = self.auth.status().await;
                let mut status = adc_status.clone();
                let default_model = self
                    .metrics
                    .get_default_model_override()
                    .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
                let primary_model = if default_model.contains("4") || default_model.contains("argon") {
                    "gemini-4-argon".to_string()
                } else {
                    default_model.clone()
                };
                if let Some(obj) = status.as_object_mut() {
                    obj.insert("status".to_string(), Value::String("ok".to_string()));
                    obj.insert("adc".to_string(), adc_status);
                    obj.insert(
                        "default_model".to_string(),
                        Value::String(default_model),
                    );
                    obj.insert(
                        "primary_model".to_string(),
                        Value::String(primary_model),
                    );
                    obj.insert(
                        "reasoning_effort".to_string(),
                        Value::String(self.metrics.get_reasoning_effort()),
                    );
                    obj.insert(
                        "hot_swap_active".to_string(),
                        Value::Bool(self.metrics.get_hot_swap_active()),
                    );
                    obj.insert("metrics".to_string(), self.metrics.snapshot());
                    obj.insert("stats".to_string(), self.metrics.stats_snapshot());
                }
                RouterResponse::Json(200, status)
            }
            ("POST", "/config") | ("POST", "/v1/config") => {
                let body: Value = match serde_json::from_slice(body_bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        return RouterResponse::Json(
                            400,
                            json!({"error": {"message": format!("Invalid JSON: {}", e)}}),
                        )
                    }
                };
                if body.get("reset_stats").and_then(|v| v.as_bool()) == Some(true) {
                    self.metrics.reset();
                }
                if let Some(m) = body
                    .get("default_model")
                    .or_else(|| body.get("primary_model"))
                    .and_then(|v| v.as_str())
                {
                    if !m.trim().is_empty() {
                        self.metrics.set_default_model(m.trim());
                        if body.get("hot_swap").and_then(|v| v.as_bool()).unwrap_or(true) {
                            self.metrics.set_hot_swap_active(true);
                        }
                    }
                }
                if let Some(e) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
                    if !e.trim().is_empty() {
                        self.metrics.set_reasoning_effort(e.trim());
                        if body.get("hot_swap").and_then(|v| v.as_bool()).unwrap_or(true) {
                            self.metrics.set_hot_swap_active(true);
                        }
                    }
                }
                if body.get("hot_swap").and_then(|v| v.as_bool()) == Some(false) {
                    self.metrics.set_hot_swap_active(false);
                }
                if let Some(p) = body.get("project_id").and_then(|v| v.as_str()) {
                    if !p.trim().is_empty() {
                        *self.auth.project_id.lock().unwrap() = Some(p.trim().to_string());
                    }
                }
                let def_model = self
                    .metrics
                    .get_default_model_override()
                    .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
                RouterResponse::Json(
                    200,
                    json!({
                        "status": "updated",
                        "default_model": def_model,
                        "primary_model": def_model,
                        "reasoning_effort": self.metrics.get_reasoning_effort(),
                        "hot_swap_active": self.metrics.get_hot_swap_active(),
                        "active_model": self.metrics.get_active_model(),
                        "project_id": self.auth.resolve_project_id(),
                        "stats": self.metrics.stats_snapshot(),
                    }),
                )
            }
            ("GET", "/models") | ("GET", "/v1/models") => {
                let models = [
                    "gemini-4-argon",
                    "gemini-3.8-flash",
                    "gemini-3.1-pro-preview",
                    "gemini-3-flash-preview",
                    "gemini-2.5-pro",
                    "gemini-2.5-flash",
                    "gemini-2.5-flash-lite",
                ];
                let data: Vec<Value> = models
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m,
                            "object": "model",
                            "created": 1740000000,
                            "owned_by": "google-vertex-adc",
                        })
                    })
                    .collect();
                RouterResponse::Json(200, json!({"object": "list", "data": data}))
            }
            ("POST", "/messages/count_tokens") | ("POST", "/v1/messages/count_tokens") => {
                let body: Value = serde_json::from_slice(body_bytes).unwrap_or_else(|_| json!({}));
                let serialized = serde_json::to_string(&body).unwrap_or_default();
                let est = std::cmp::max(1, serialized.len() / 4);
                RouterResponse::Json(200, json!({"input_tokens": est}))
            }
            ("POST", "/messages") | ("POST", "/v1/messages") => {
                let raw_body: Value = match serde_json::from_slice(body_bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        return RouterResponse::Json(
                            400,
                            json!({"type": "error", "error": {"type": "invalid_request_error", "message": e.to_string()}}),
                        )
                    }
                };
                let body = self.apply_model_override(raw_body);
                let requested_model = body
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_GEMINI_MODEL)
                    .to_string();
                let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
                let (gemini_model, gemini_payload) = anthropic_to_gemini_request(&body);
                self.metrics.record_request(&gemini_model);

                if !stream {
                    match self
                        .generate_with_fallback(&gemini_model, &gemini_payload)
                        .await
                    {
                        Ok((used_model, gemini_resp)) => {
                            let mut g = self.metrics.inner.lock().unwrap();
                            g.active_model = used_model;
                            drop(g);
                            let usage = gemini_resp
                                .get("usageMetadata")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            self.metrics.record_success(
                                usage.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
                                usage
                                    .get("candidatesTokenCount")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            );
                            let resp =
                                gemini_to_anthropic_response(&gemini_resp, &requested_model);
                            RouterResponse::Json(200, resp)
                        }
                        Err(err) => {
                            self.metrics.record_error();
                            RouterResponse::Json(
                                err.status_code,
                                json!({
                                    "type": "error",
                                    "error": {
                                        "type": "api_error",
                                        "message": err.message,
                                    }
                                }),
                            )
                        }
                    }
                } else {
                    let (tx, rx) = mpsc::channel(64);
                    let upstream = self.upstream.clone();
                    let metrics = self.metrics.clone();
                    tokio::spawn(async move {
                        let mut state = AnthropicStreamState::new(&requested_model);
                        let mut active_model = gemini_model.clone();
                        let stream_res = match upstream
                            .stream_generate_content(&active_model, &gemini_payload)
                            .await
                        {
                            Ok(r) => Ok(r),
                            Err(err) => {
                                if let Some(fb) = fallback_model_for(&active_model) {
                                    if matches!(err.status_code, 400 | 404 | 429 | 500 | 503) {
                                        active_model = fb.to_string();
                                        metrics.inner.lock().unwrap().active_model =
                                            active_model.clone();
                                        upstream
                                            .stream_generate_content(&active_model, &gemini_payload)
                                            .await
                                    } else {
                                        Err(err)
                                    }
                                } else {
                                    Err(err)
                                }
                            }
                        };

                        match stream_res {
                            Ok(mut chunk_rx) => {
                                while let Some(item) = chunk_rx.recv().await {
                                    match item {
                                        Ok(chunk) => {
                                            for sse in state.process_gemini_chunk(&chunk) {
                                                if tx.send(sse).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            metrics.record_error();
                                            let _ = tx
                                                .send(format_sse(
                                                    Some("error"),
                                                    &json!({
                                                        "type": "error",
                                                        "error": {"type": "api_error", "message": err.message},
                                                    }),
                                                ))
                                                .await;
                                            return;
                                        }
                                    }
                                }
                                for sse in state.finish() {
                                    if tx.send(sse).await.is_err() {
                                        return;
                                    }
                                }
                                metrics.record_success(state.input_tokens, state.output_tokens);
                            }
                            Err(err) => {
                                metrics.record_error();
                                let _ = tx
                                    .send(format_sse(
                                        Some("error"),
                                        &json!({
                                            "type": "error",
                                            "error": {"type": "api_error", "message": err.message},
                                        }),
                                    ))
                                    .await;
                            }
                        }
                    });
                    RouterResponse::Sse(rx)
                }
            }
            ("POST", "/chat/completions") | ("POST", "/v1/chat/completions") => {
                let raw_body: Value = match serde_json::from_slice(body_bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        return RouterResponse::Json(
                            400,
                            json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}}),
                        )
                    }
                };
                let body = self.apply_model_override(raw_body);
                let requested_model = body
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_GEMINI_MODEL)
                    .to_string();
                let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
                let (gemini_model, gemini_payload) = openai_chat_to_gemini_request(&body);
                self.metrics.record_request(&gemini_model);

                if !stream {
                    match self
                        .generate_with_fallback(&gemini_model, &gemini_payload)
                        .await
                    {
                        Ok((used_model, gemini_resp)) => {
                            self.metrics.inner.lock().unwrap().active_model = used_model;
                            let usage = gemini_resp
                                .get("usageMetadata")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            self.metrics.record_success(
                                usage.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
                                usage
                                    .get("candidatesTokenCount")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            );
                            let resp =
                                gemini_to_openai_chat_response(&gemini_resp, &requested_model);
                            RouterResponse::Json(200, resp)
                        }
                        Err(err) => {
                            self.metrics.record_error();
                            RouterResponse::Json(
                                err.status_code,
                                json!({
                                    "error": {
                                        "message": err.message,
                                        "type": "upstream_error",
                                        "code": err.status_code,
                                    }
                                }),
                            )
                        }
                    }
                } else {
                    let (tx, rx) = mpsc::channel(64);
                    let upstream = self.upstream.clone();
                    let metrics = self.metrics.clone();
                    tokio::spawn(async move {
                        let chat_id = format!("chatcmpl-{}", short_id(24));
                        let created = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let mut sent_role = false;
                        let mut has_tool_calls = false;
                        let mut tool_idx = 0usize;
                        let mut tok_in = 0u64;
                        let mut tok_out = 0u64;

                        match upstream
                            .stream_generate_content(&gemini_model, &gemini_payload)
                            .await
                        {
                            Ok(mut chunk_rx) => {
                                while let Some(Ok(chunk)) = chunk_rx.recv().await {
                                    let (parts, _finish, usage) = extract_candidate_parts(&chunk);
                                    if let Some(pt) =
                                        usage.get("promptTokenCount").and_then(|v| v.as_u64())
                                    {
                                        tok_in = pt;
                                    }
                                    if let Some(ct) =
                                        usage.get("candidatesTokenCount").and_then(|v| v.as_u64())
                                    {
                                        tok_out = ct;
                                    }
                                    if !sent_role {
                                        sent_role = true;
                                        let _ = tx
                                            .send(format_sse(
                                                None,
                                                &json!({
                                                    "id": chat_id,
                                                    "object": "chat.completion.chunk",
                                                    "created": created,
                                                    "model": requested_model,
                                                    "choices": [{
                                                        "index": 0,
                                                        "delta": {"role": "assistant"},
                                                        "finish_reason": Value::Null,
                                                    }]
                                                }),
                                            ))
                                            .await;
                                    }
                                    for part in parts {
                                        let pobj = match part.as_object() {
                                            Some(o) => o,
                                            None => continue,
                                        };
                                        let sig = pobj
                                            .get("thoughtSignature")
                                            .and_then(|s| s.as_str())
                                            .map(|s| s.to_string());
                                        if let Some(ref s) = sig {
                                            SIGNATURE_STORE.save(None, Some(s));
                                        }
                                        if pobj.get("thought").and_then(|v| v.as_bool())
                                            == Some(true)
                                        {
                                            continue;
                                        }
                                        if let Some(fc) =
                                            pobj.get("functionCall").and_then(|v| v.as_object())
                                        {
                                            has_tool_calls = true;
                                            let call_id = format!("call_{}", short_id(24));
                                            if let Some(ref s) = sig {
                                                SIGNATURE_STORE.save(Some(&call_id), Some(s));
                                            }
                                            let args_str = serde_json::to_string(
                                                &fc.get("args")
                                                    .cloned()
                                                    .unwrap_or_else(|| json!({})),
                                            )
                                            .unwrap_or_else(|_| "{}".to_string());
                                            let _ = tx
                                                .send(format_sse(
                                                    None,
                                                    &json!({
                                                        "id": chat_id,
                                                        "object": "chat.completion.chunk",
                                                        "created": created,
                                                        "model": requested_model,
                                                        "choices": [{
                                                            "index": 0,
                                                            "delta": {
                                                                "tool_calls": [{
                                                                    "index": tool_idx,
                                                                    "id": call_id,
                                                                    "type": "function",
                                                                    "function": {
                                                                        "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                                                                        "arguments": args_str,
                                                                    }
                                                                }]
                                                            },
                                                            "finish_reason": Value::Null,
                                                        }]
                                                    }),
                                                ))
                                                .await;
                                            tool_idx += 1;
                                        } else if let Some(text) =
                                            pobj.get("text").and_then(|t| t.as_str())
                                        {
                                            if !text.is_empty() {
                                                let _ = tx
                                                    .send(format_sse(
                                                        None,
                                                        &json!({
                                                            "id": chat_id,
                                                            "object": "chat.completion.chunk",
                                                            "created": created,
                                                            "model": requested_model,
                                                            "choices": [{
                                                                "index": 0,
                                                                "delta": {"content": text},
                                                                "finish_reason": Value::Null,
                                                            }]
                                                        }),
                                                    ))
                                                    .await;
                                            }
                                        }
                                    }
                                }
                                let finish_reason = if has_tool_calls { "tool_calls" } else { "stop" };
                                let _ = tx
                                    .send(format_sse(
                                        None,
                                        &json!({
                                            "id": chat_id,
                                            "object": "chat.completion.chunk",
                                            "created": created,
                                            "model": requested_model,
                                            "choices": [{
                                                "index": 0,
                                                "delta": {},
                                                "finish_reason": finish_reason,
                                            }]
                                        }),
                                    ))
                                    .await;
                                let _ = tx.send("data: [DONE]\n\n".to_string()).await;
                                metrics.record_success(tok_in, tok_out);
                            }
                            Err(err) => {
                                metrics.record_error();
                                let _ = tx
                                    .send(format_sse(
                                        None,
                                        &json!({"error": {"message": err.message}}),
                                    ))
                                    .await;
                            }
                        }
                    });
                    RouterResponse::Sse(rx)
                }
            }
            ("POST", "/responses") | ("POST", "/v1/responses") => {
                let raw_body: Value = match serde_json::from_slice(body_bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        return RouterResponse::Json(
                            400,
                            json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}}),
                        )
                    }
                };
                let body = self.apply_model_override(raw_body);
                let requested_model = body
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_GEMINI_MODEL)
                    .to_string();
                let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
                let (gemini_model, gemini_payload) = openai_responses_to_gemini_request(&body);
                self.metrics.record_request(&gemini_model);

                if !stream {
                    match self
                        .generate_with_fallback(&gemini_model, &gemini_payload)
                        .await
                    {
                        Ok((used_model, gemini_resp)) => {
                            self.metrics.inner.lock().unwrap().active_model = used_model;
                            let usage = gemini_resp
                                .get("usageMetadata")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            self.metrics.record_success(
                                usage.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
                                usage
                                    .get("candidatesTokenCount")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            );
                            let resp = gemini_to_openai_responses(&gemini_resp, &requested_model);
                            RouterResponse::Json(200, resp)
                        }
                        Err(err) => {
                            self.metrics.record_error();
                            RouterResponse::Json(
                                err.status_code,
                                json!({
                                    "error": {
                                        "message": err.message,
                                        "type": "upstream_error",
                                        "code": err.status_code,
                                    }
                                }),
                            )
                        }
                    }
                } else {
                    let (tx, rx) = mpsc::channel(64);
                    let upstream = self.upstream.clone();
                    let metrics = self.metrics.clone();
                    tokio::spawn(async move {
                        let resp_id = format!("resp_{}", short_id(24));
                        let msg_id = format!("msg_{}", short_id(24));
                        let created = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let mut full_text = String::new();
                        let mut output_items: Vec<Value> = Vec::new();
                        let mut tok_in = 0u64;
                        let mut tok_out = 0u64;

                        let _ = tx
                            .send(format_sse(
                                Some("response.created"),
                                &json!({
                                    "type": "response.created",
                                    "response": {
                                        "id": resp_id,
                                        "object": "response",
                                        "created_at": created,
                                        "status": "in_progress",
                                        "model": requested_model,
                                        "output": [],
                                    }
                                }),
                            ))
                            .await;

                        match upstream
                            .stream_generate_content(&gemini_model, &gemini_payload)
                            .await
                        {
                            Ok(mut chunk_rx) => {
                                while let Some(Ok(chunk)) = chunk_rx.recv().await {
                                    let (parts, _finish, usage) = extract_candidate_parts(&chunk);
                                    if let Some(pt) =
                                        usage.get("promptTokenCount").and_then(|v| v.as_u64())
                                    {
                                        tok_in = pt;
                                    }
                                    if let Some(ct) =
                                        usage.get("candidatesTokenCount").and_then(|v| v.as_u64())
                                    {
                                        tok_out = ct;
                                    }
                                    for part in parts {
                                        let pobj = match part.as_object() {
                                            Some(o) => o,
                                            None => continue,
                                        };
                                        let sig = pobj
                                            .get("thoughtSignature")
                                            .and_then(|s| s.as_str())
                                            .map(|s| s.to_string());
                                        if let Some(ref s) = sig {
                                            SIGNATURE_STORE.save(None, Some(s));
                                        }
                                        if pobj.get("thought").and_then(|v| v.as_bool())
                                            == Some(true)
                                        {
                                            continue;
                                        }
                                        if let Some(fc) =
                                            pobj.get("functionCall").and_then(|v| v.as_object())
                                        {
                                            let call_id = format!("call_{}", short_id(24));
                                            if let Some(ref s) = sig {
                                                SIGNATURE_STORE.save(Some(&call_id), Some(s));
                                            }
                                            let args_str = serde_json::to_string(
                                                &fc.get("args")
                                                    .cloned()
                                                    .unwrap_or_else(|| json!({})),
                                            )
                                            .unwrap_or_else(|_| "{}".to_string());
                                            let fc_item = json!({
                                                "id": format!("fc_{}", short_id(24)),
                                                "type": "function_call",
                                                "status": "completed",
                                                "call_id": call_id,
                                                "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                                                "arguments": args_str,
                                            });
                                            output_items.push(fc_item.clone());
                                            let _ = tx
                                                .send(format_sse(
                                                    Some("response.output_item.done"),
                                                    &json!({
                                                        "type": "response.output_item.done",
                                                        "output_index": output_items.len() - 1,
                                                        "item": fc_item,
                                                    }),
                                                ))
                                                .await;
                                        } else if let Some(text) =
                                            pobj.get("text").and_then(|t| t.as_str())
                                        {
                                            if !text.is_empty() {
                                                full_text.push_str(text);
                                                let _ = tx
                                                    .send(format_sse(
                                                        Some("response.output_text.delta"),
                                                        &json!({
                                                            "type": "response.output_text.delta",
                                                            "item_id": msg_id,
                                                            "output_index": 0,
                                                            "content_index": 0,
                                                            "delta": text,
                                                        }),
                                                    ))
                                                    .await;
                                            }
                                        }
                                    }
                                }

                                if !full_text.is_empty() || output_items.is_empty() {
                                    let msg_item = json!({
                                        "id": msg_id,
                                        "type": "message",
                                        "status": "completed",
                                        "role": "assistant",
                                        "content": [{
                                            "type": "output_text",
                                            "text": full_text,
                                            "annotations": [],
                                        }],
                                    });
                                    output_items.insert(0, msg_item.clone());
                                    let _ = tx
                                        .send(format_sse(
                                            Some("response.output_item.done"),
                                            &json!({
                                                "type": "response.output_item.done",
                                                "output_index": 0,
                                                "item": msg_item,
                                            }),
                                        ))
                                        .await;
                                }

                                let _ = tx
                                    .send(format_sse(
                                        Some("response.completed"),
                                        &json!({
                                            "type": "response.completed",
                                            "response": {
                                                "id": resp_id,
                                                "object": "response",
                                                "created_at": created,
                                                "status": "completed",
                                                "model": requested_model,
                                                "output": output_items,
                                                "usage": {
                                                    "input_tokens": tok_in,
                                                    "output_tokens": tok_out,
                                                    "total_tokens": tok_in + tok_out,
                                                }
                                            }
                                        }),
                                    ))
                                    .await;
                                metrics.record_success(tok_in, tok_out);
                            }
                            Err(err) => {
                                metrics.record_error();
                                let _ = tx
                                    .send(format_sse(
                                        Some("error"),
                                        &json!({"type": "error", "error": {"message": err.message}}),
                                    ))
                                    .await;
                            }
                        }
                    });
                    RouterResponse::Sse(rx)
                }
            }
            _ => RouterResponse::Json(
                404,
                json!({"error": {"message": format!("Not Found: {} {}", method, clean_path)}}),
            ),
        }
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn read_http_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut temp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            return Err(anyhow!("Connection closed before headers completed"));
        }
        buf.extend_from_slice(&temp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 2 * 1024 * 1024 {
            return Err(anyhow!("Headers too large"));
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let _ = req.parse(&buf[..header_end])?;
    let method = req.method.unwrap_or("GET").to_string();
    let path = req.path.unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let mut is_chunked = false;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("content-length") {
            if let Ok(s) = std::str::from_utf8(h.value) {
                content_length = s.trim().parse::<usize>().unwrap_or(0);
            }
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            if let Ok(s) = std::str::from_utf8(h.value) {
                if s.to_ascii_lowercase().contains("chunked") {
                    is_chunked = true;
                }
            }
        }
    }

    let mut body = buf[header_end..].to_vec();
    if is_chunked {
        while !body.windows(5).any(|w| w == b"0\r\n\r\n") {
            let n = stream.read(&mut temp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&temp[..n]);
        }
        let mut decoded = Vec::new();
        let mut idx = 0usize;
        while idx < body.len() {
            if let Some(pos) = body[idx..].windows(2).position(|w| w == b"\r\n") {
                let len_str = std::str::from_utf8(&body[idx..idx + pos]).unwrap_or("0");
                let chunk_len = usize::from_str_radix(len_str.trim(), 16).unwrap_or(0);
                idx += pos + 2;
                if chunk_len == 0 {
                    break;
                }
                if idx + chunk_len <= body.len() {
                    decoded.extend_from_slice(&body[idx..idx + chunk_len]);
                    idx += chunk_len + 2;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        body = decoded;
    } else if content_length > body.len() {
        let remaining = content_length - body.len();
        let mut rest = vec![0u8; remaining];
        stream.read_exact(&mut rest).await?;
        body.extend_from_slice(&rest);
    } else if content_length > 0 {
        body.truncate(content_length);
    }

    Ok((method, path, body))
}

async fn handle_connection(mut stream: TcpStream, server: Arc<RouterServer>) {
    let (method, path, body) = match read_http_request(&mut stream).await {
        Ok(r) => r,
        Err(_) => return,
    };

    match server.handle_request(&method, &path, &body).await {
        RouterResponse::Json(status, val) => {
            let payload = serde_json::to_vec(&val).unwrap_or_else(|_| b"{}".to_vec());
            let header = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                status_reason(status),
                payload.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&payload).await;
            let _ = stream.flush().await;
        }
        RouterResponse::Sse(mut rx) => {
            let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\n";
            if stream.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            while let Some(chunk) = rx.recv().await {
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        }
    }
}

pub async fn serve(
    host: &str,
    port: u16,
    project: Option<String>,
    location: Option<String>,
) -> Result<()> {
    let server = Arc::new(RouterServer::new(project, location));
    let addr = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "Claude/Codex -> Vertex AI Gemini Router (Rust) listening on http://{}",
        addr
    );
    loop {
        let (stream, _) = listener.accept().await?;
        let srv = server.clone();
        tokio::spawn(async move {
            handle_connection(stream, srv).await;
        });
    }
}

pub fn parse_query_params(path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some((_, q)) = path.split_once('?') {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}
