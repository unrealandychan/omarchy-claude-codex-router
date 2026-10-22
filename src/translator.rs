use java_like_uuid::new_short_id;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.1-pro-preview";
pub const DEFAULT_FAST_GEMINI_MODEL: &str = "gemini-3-flash-preview";
pub const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

mod java_like_uuid {
    use uuid::Uuid;
    pub fn new_short_id(len: usize) -> String {
        let simple = Uuid::new_v4().simple().to_string();
        simple.chars().take(len).collect()
    }
}

pub struct ThoughtSignatureStore {
    max_entries: usize,
    order: Mutex<VecDeque<String>>,
    store: Mutex<HashMap<String, String>>,
    last_signature: Mutex<Option<String>>,
}

impl ThoughtSignatureStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            order: Mutex::new(VecDeque::new()),
            store: Mutex::new(HashMap::new()),
            last_signature: Mutex::new(None),
        }
    }

    pub fn save(&self, call_id: Option<&str>, signature: Option<&str>) {
        let sig = match signature {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return,
        };
        *self.last_signature.lock().unwrap() = Some(sig.clone());
        if let Some(id) = call_id {
            if !id.is_empty() {
                let mut store = self.store.lock().unwrap();
                let mut order = self.order.lock().unwrap();
                if store.contains_key(id) {
                    order.retain(|k| k != id);
                } else if order.len() >= self.max_entries {
                    if let Some(oldest) = order.pop_front() {
                        store.remove(&oldest);
                    }
                }
                order.push_back(id.to_string());
                store.insert(id.to_string(), sig);
            }
        }
    }

    pub fn get(&self, call_id: Option<&str>) -> Option<String> {
        if let Some(id) = call_id {
            let found = self.store.lock().unwrap().get(id).cloned();
            if let Some(sig) = found {
                let mut order = self.order.lock().unwrap();
                order.retain(|k| k != id);
                order.push_back(id.to_string());
                return Some(sig);
            }
        }
        self.last_signature.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.order.lock().unwrap().clear();
        self.store.lock().unwrap().clear();
        *self.last_signature.lock().unwrap() = None;
    }
}

pub static SIGNATURE_STORE: LazyLock<ThoughtSignatureStore> =
    LazyLock::new(|| ThoughtSignatureStore::new(4096));

pub struct ToolSchemaStore {
    max_entries: usize,
    order: Mutex<VecDeque<String>>,
    store: Mutex<HashMap<String, Value>>,
}

impl ToolSchemaStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            order: Mutex::new(VecDeque::new()),
            store: Mutex::new(HashMap::new()),
        }
    }

    pub fn save(&self, name: &str, declaration: &Value) {
        if name.is_empty() {
            return;
        }
        let mut store = self.store.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        if store.contains_key(name) {
            order.retain(|k| k != name);
        } else if order.len() >= self.max_entries {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
        order.push_back(name.to_string());
        store.insert(name.to_string(), declaration.clone());
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let found = self.store.lock().unwrap().get(name).cloned();
        if found.is_some() {
            let mut order = self.order.lock().unwrap();
            order.retain(|k| k != name);
            order.push_back(name.to_string());
        }
        found
    }

    pub fn clear(&self) {
        self.order.lock().unwrap().clear();
        self.store.lock().unwrap().clear();
    }
}

pub static TOOL_SCHEMA_STORE: LazyLock<ToolSchemaStore> =
    LazyLock::new(|| ToolSchemaStore::new(2048));

#[derive(Clone, Debug)]
pub struct CachedResponseState {
    pub system_instruction: Option<Value>,
    pub contents: Vec<Value>,
    pub tools: Option<Value>,
    pub call_id_to_name: HashMap<String, String>,
}

pub struct ResponseCacheStore {
    max_entries: usize,
    order: Mutex<VecDeque<String>>,
    store: Mutex<HashMap<String, CachedResponseState>>,
}

impl ResponseCacheStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            order: Mutex::new(VecDeque::new()),
            store: Mutex::new(HashMap::new()),
        }
    }

    pub fn save(&self, key: &str, state: CachedResponseState) {
        if key.is_empty() {
            return;
        }
        let mut store = self.store.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        if store.contains_key(key) {
            order.retain(|k| k != key);
        } else if order.len() >= self.max_entries {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
        order.push_back(key.to_string());
        store.insert(key.to_string(), state);
    }

    pub fn get(&self, key: &str) -> Option<CachedResponseState> {
        let found = self.store.lock().unwrap().get(key).cloned();
        if found.is_some() {
            let mut order = self.order.lock().unwrap();
            order.retain(|k| k != key);
            order.push_back(key.to_string());
        }
        found
    }

    pub fn clear(&self) {
        self.order.lock().unwrap().clear();
        self.store.lock().unwrap().clear();
    }
}

pub static RESPONSE_CACHE_STORE: LazyLock<ResponseCacheStore> =
    LazyLock::new(|| ResponseCacheStore::new(1024));

#[derive(Clone, Debug, Default)]
pub struct AnthropicCacheHint {
    pub session_key: String,
    pub has_cache_control: bool,
    pub is_multi_turn: bool,
    pub estimated_prefix_tokens: u64,
    pub estimated_new_turn_tokens: u64,
}

pub struct PromptCacheStore {
    max_entries: usize,
    order: Mutex<VecDeque<String>>,
    store: Mutex<HashMap<String, u64>>,
}

impl PromptCacheStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            order: Mutex::new(VecDeque::new()),
            store: Mutex::new(HashMap::new()),
        }
    }

    pub fn record(&self, key: &str, cached_tokens: u64) {
        if key.is_empty() || cached_tokens == 0 {
            return;
        }
        let mut order = self.order.lock().unwrap();
        let mut store = self.store.lock().unwrap();
        order.retain(|k| k != key);
        if order.len() >= self.max_entries {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
        order.push_back(key.to_string());
        store.insert(key.to_string(), cached_tokens);
    }

    pub fn get(&self, key: &str) -> Option<u64> {
        let found = self.store.lock().unwrap().get(key).copied();
        if found.is_some() {
            let mut order = self.order.lock().unwrap();
            order.retain(|k| k != key);
            order.push_back(key.to_string());
        }
        found
    }

    pub fn clear(&self) {
        self.order.lock().unwrap().clear();
        self.store.lock().unwrap().clear();
    }
}

pub static PROMPT_CACHE_STORE: LazyLock<PromptCacheStore> =
    LazyLock::new(|| PromptCacheStore::new(512));

pub fn extract_anthropic_cache_hint(body: &Value, gemini_payload: &Value) -> AnthropicCacheHint {
    use std::hash::{Hash, Hasher};

    let raw_body_str = serde_json::to_string(body).unwrap_or_default();
    let has_cache_control = raw_body_str.contains("\"cache_control\"");
    let messages_len = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let is_multi_turn = messages_len > 1;

    let sys_str = gemini_payload
        .get("systemInstruction")
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default();
    let tools_str = gemini_payload
        .get("tools")
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default();
    let contents = gemini_payload
        .get("contents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let first_turn_str = contents
        .first()
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sys_str.hash(&mut hasher);
    tools_str.hash(&mut hasher);
    if sys_str.is_empty() && tools_str.is_empty() {
        first_turn_str.hash(&mut hasher);
    }
    let session_key = format!("claude_cache_{:016x}", hasher.finish());

    let mut prefix_bytes = sys_str.len() + tools_str.len();
    if contents.len() > 1 {
        for item in &contents[..contents.len() - 1] {
            if let Ok(s) = serde_json::to_string(item) {
                prefix_bytes += s.len();
            }
        }
    }
    let last_turn_bytes = contents
        .last()
        .and_then(|v| serde_json::to_string(v).ok())
        .map(|s| s.len())
        .unwrap_or(4);

    let estimated_prefix_tokens = (prefix_bytes as u64) / 4;
    let estimated_new_turn_tokens = std::cmp::max(1, (last_turn_bytes as u64) / 4);

    AnthropicCacheHint {
        session_key,
        has_cache_control,
        is_multi_turn,
        estimated_prefix_tokens,
        estimated_new_turn_tokens,
    }
}

pub fn resolve_effective_anthropic_cache_usage(
    prompt_tokens: u64,
    reported_cached_tokens: u64,
    hint: Option<&AnthropicCacheHint>,
) -> (u64, u64) {
    let Some(h) = hint else {
        let effective_cached = if prompt_tokens > 0 {
            reported_cached_tokens.min(prompt_tokens)
        } else {
            reported_cached_tokens
        };
        return (
            prompt_tokens.saturating_sub(effective_cached),
            effective_cached,
        );
    };

    let prev_cached = PROMPT_CACHE_STORE.get(&h.session_key).unwrap_or(0);
    let should_preserve = prev_cached > 0
        || reported_cached_tokens > 0
        || (h.has_cache_control && h.estimated_prefix_tokens > 0)
        || (h.is_multi_turn && h.estimated_prefix_tokens >= 64);

    if !should_preserve {
        return (prompt_tokens, 0);
    }

    if prompt_tokens > 0 {
        let candidate = if prompt_tokens <= prev_cached {
            // Conversation was compacted or truncated; scale down to current prompt size
            reported_cached_tokens
                .max(h.estimated_prefix_tokens)
                .min(prompt_tokens.saturating_sub(1))
        } else {
            reported_cached_tokens
                .max(prev_cached)
                .max(h.estimated_prefix_tokens)
                .min(prompt_tokens.saturating_sub(1))
        };
        if candidate > 0 {
            PROMPT_CACHE_STORE.record(&h.session_key, candidate);
        }
        (prompt_tokens.saturating_sub(candidate), candidate)
    } else {
        // First SSE chunk (`message_start`) before Vertex AI reports `promptTokenCount`
        let candidate = reported_cached_tokens
            .max(prev_cached)
            .max(h.estimated_prefix_tokens);
        let uncached_in = if candidate > 0 {
            h.estimated_new_turn_tokens.max(1)
        } else {
            0
        };
        (uncached_in, candidate)
    }
}

pub fn extract_usage_counts(usage: &Value) -> (u64, u64, u64, u64) {
    let prompt_tokens = usage
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_tokens = usage
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    (
        prompt_tokens,
        output_tokens,
        cached_tokens,
        reasoning_tokens,
    )
}

pub fn record_response_cache_state(
    response_id: &str,
    prompt_cache_key: Option<&str>,
    gemini_payload: &Value,
    output_or_parts: &[Value],
) {
    let sys_inst = gemini_payload.get("systemInstruction").cloned();
    let tools = gemini_payload.get("tools").cloned();
    let mut contents = gemini_payload
        .get("contents")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut converted_parts: Vec<Value> = Vec::new();
    let mut call_id_to_name: HashMap<String, String> = HashMap::new();
    for item in output_or_parts {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if obj.get("thought").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                if let Some(carr) = obj.get("content").and_then(|c| c.as_array()) {
                    for c in carr {
                        if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                converted_parts.push(json!({"text": t}));
                            }
                        }
                    }
                } else if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                    if !s.is_empty() {
                        converted_parts.push(json!({"text": s}));
                    }
                }
            }
            Some("function_call") => {
                let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str());
                if let Some(cid) = call_id {
                    call_id_to_name.insert(cid.to_string(), name.to_string());
                }
                let args = match obj.get("arguments") {
                    Some(Value::String(s)) => {
                        serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({"raw": s}))
                    }
                    Some(Value::Object(m)) => Value::Object(m.clone()),
                    _ => json!({}),
                };
                let sig = obj
                    .get("thought_signature")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| SIGNATURE_STORE.get(call_id))
                    .unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
                converted_parts.push(json!({
                    "functionCall": {
                        "name": name,
                        "args": args,
                    },
                    "thoughtSignature": sig,
                }));
            }
            _ => {
                if obj.contains_key("text") || obj.contains_key("functionCall") {
                    converted_parts.push(item.clone());
                }
            }
        }
    }

    if !converted_parts.is_empty() {
        contents.push(json!({
            "role": "model",
            "parts": converted_parts,
        }));
    }

    let state = CachedResponseState {
        system_instruction: sys_inst,
        contents,
        tools,
        call_id_to_name,
    };
    RESPONSE_CACHE_STORE.save(response_id, state.clone());
    if let Some(cache_key) = prompt_cache_key.filter(|k| !k.trim().is_empty()) {
        RESPONSE_CACHE_STORE.save(cache_key.trim(), state);
    }
}

pub fn default_gemini_model() -> String {
    std::env::var("GEMINI_DEFAULT_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string())
}

pub fn default_fast_gemini_model() -> String {
    std::env::var("GEMINI_FAST_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_FAST_GEMINI_MODEL.to_string())
}

pub fn resolve_gemini_model(requested_model: Option<&str>) -> String {
    let raw = match requested_model {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return default_gemini_model(),
    };
    // Strip optional Claude Code context window suffix like `[1m]`
    let model = raw.split('[').next().unwrap_or(raw).trim();
    if model.is_empty() {
        return default_gemini_model();
    }
    let lower = model.to_lowercase();

    // 1. User-defined dynamic aliases via GEMINI_MODEL_ALIASES="alias1=target1,alias2=target2"
    if let Ok(custom_aliases) = std::env::var("GEMINI_MODEL_ALIASES") {
        for pair in custom_aliases.split(',') {
            if let Some((alias, target)) = pair.split_once('=') {
                let alias_clean = alias.trim().to_lowercase();
                let target_clean = target.trim();
                if !alias_clean.is_empty() && !target_clean.is_empty() && lower == alias_clean {
                    return target_clean.to_string();
                }
            }
        }
    }

    // 2. Built-in shorthand aliases
    for (alias, target) in [
        ("gemini-4-argon", "gemini-4-argon"),
        ("gemini-4.0", "gemini-4-argon"),
        ("gemini-4.0-pro", "gemini-4-argon"),
        ("4.0", "gemini-4-argon"),
        ("argon", "gemini-4-argon"),
        ("gemini-3.8-flash", "gemini-3.8-flash"),
        ("gemini-3.8", "gemini-3.8-flash"),
        ("3.8-flash", "gemini-3.8-flash"),
        ("3.8", "gemini-3.8-flash"),
        ("flash", "gemini-3.8-flash"),
        ("gemini-3.1-pro-preview", "gemini-3.1-pro-preview"),
        ("gemini-3.1-pro", "gemini-3.1-pro-preview"),
        ("3.1-pro", "gemini-3.1-pro-preview"),
        ("3.1", "gemini-3.1-pro-preview"),
        ("gemini-3-flash-preview", "gemini-3-flash-preview"),
        ("gemini-3-flash", "gemini-3-flash-preview"),
        ("3-flash", "gemini-3-flash-preview"),
        ("gemini-2.5-pro", "gemini-2.5-pro"),
        ("2.5-pro", "gemini-2.5-pro"),
        ("gemini-2.5-flash-lite", "gemini-2.5-flash-lite"),
        ("2.5-flash-lite", "gemini-2.5-flash-lite"),
        ("gemini-2.5-flash", "gemini-2.5-flash"),
        ("2.5-flash", "gemini-2.5-flash"),
    ] {
        if lower == alias {
            return target.to_string();
        }
    }

    // 3. Any explicit Gemini model ID (past, present, or future) passes through directly
    if lower.starts_with("gemini-") {
        return model.to_string();
    }

    // 4. Client tier mappings (Haiku/fast vs Sonnet/Opus/Codex/GPT)
    if lower.contains("haiku")
        || lower.contains("mini")
        || lower.contains("fast")
        || lower.contains("nano")
    {
        return default_fast_gemini_model();
    }
    if lower.contains("sonnet")
        || lower.contains("opus")
        || lower.contains("fable")
        || lower.contains("claude")
    {
        return default_gemini_model();
    }
    if lower.contains("codex")
        || lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return default_gemini_model();
    }

    // 5. If a custom non-Anthropic/non-OpenAI model ID was explicitly passed, preserve it
    if lower.contains("gemini") {
        return model.to_string();
    }
    default_gemini_model()
}

pub fn format_model_display_name(model: &str) -> String {
    let clean = model.trim().to_lowercase();
    match clean.as_str() {
        "gemini-4-argon" => "Gemini 4.0 Argon".to_string(),
        "gemini-3.8-flash" => "Gemini 3.8 Flash".to_string(),
        _ => {
            let mut parts: Vec<&str> = model.trim().split('-').collect();
            if parts.len() > 2
                && parts.last().map(|s| s.eq_ignore_ascii_case("preview")) == Some(true)
            {
                parts.pop();
            }
            parts
                .into_iter()
                .map(|part| {
                    let mut chars = part.chars();
                    match chars.next() {
                        None => String::new(),
                        Some(first) => {
                            first.to_uppercase().collect::<String>()
                                + &chars.as_str().to_lowercase()
                        }
                    }
                })
                .collect::<Vec<String>>()
                .join(" ")
        }
    }
}

pub fn format_model_short_label(model: &str) -> String {
    let clean = model.trim().to_lowercase();
    match clean.as_str() {
        "gemini-4-argon" => "Gemini 4.0".to_string(),
        "gemini-3.8-flash" => "Gemini 3.8".to_string(),
        _ => format_model_display_name(model),
    }
}

pub fn format_model_short_badge(model: &str) -> String {
    let clean = model.trim().to_lowercase();
    match clean.as_str() {
        "gemini-4-argon" => "G4".to_string(),
        "gemini-3.8-flash" => "G3.8".to_string(),
        _ => {
            let stripped = clean.strip_prefix("gemini-").unwrap_or(&clean);
            let mut segs = stripped.split('-');
            let first = segs.next().unwrap_or("?");
            let second = segs.next().unwrap_or("");
            let third = segs.next().unwrap_or("");
            let suffix = if third == "lite" {
                "L"
            } else if second == "pro" {
                "P"
            } else if second == "flash" {
                "F"
            } else if second == "ultra" {
                "U"
            } else {
                ""
            };
            format!("G{}{}", first.to_uppercase(), suffix)
        }
    }
}

fn effort_to_thinking_level(effort: &str) -> &'static str {
    match effort.trim().to_lowercase().as_str() {
        "low" => "LOW",
        "high" | "xhigh" | "max" => "HIGH",
        _ => "MEDIUM",
    }
}

fn resolve_json_ref(
    ref_path: &str,
    root_schema: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    if !ref_path.starts_with("#/") {
        return None;
    }
    let parts: Vec<&str> = ref_path[2..].split('/').collect();
    let mut node_map = root_schema;
    for (i, part) in parts.iter().enumerate() {
        let decoded = part.replace("~1", "/").replace("~0", "~");
        let next = node_map.get(&decoded)?;
        if i + 1 == parts.len() {
            return next.as_object().cloned();
        }
        node_map = next.as_object()?;
    }
    None
}

pub fn sanitize_json_schema(schema: &Value) -> Value {
    let root = match schema.as_object() {
        Some(obj) => obj,
        None => return json!({"type": "object", "properties": {}}),
    };
    let mut seen = HashSet::new();
    let mut cleaned = sanitize_schema_inner(root, root, &mut seen);
    if !cleaned.contains_key("type") {
        cleaned.insert("type".to_string(), Value::String("object".to_string()));
    }
    Value::Object(cleaned)
}

fn sanitize_schema_inner(
    node: &Map<String, Value>,
    root_schema: &Map<String, Value>,
    seen_refs: &mut HashSet<String>,
) -> Map<String, Value> {
    let mut work: Map<String, Value> = node.clone();

    if let Some(Value::String(ref_str)) = work.get("$ref").cloned() {
        if !seen_refs.contains(&ref_str) {
            if let Some(resolved) = resolve_json_ref(&ref_str, root_schema) {
                seen_refs.insert(ref_str.clone());
                let mut merged = resolved;
                for (k, v) in work.iter() {
                    if k != "$ref" {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                let res = sanitize_schema_inner(&merged, root_schema, seen_refs);
                seen_refs.remove(&ref_str);
                return res;
            }
        }
    }

    if let Some(Value::Array(all_of)) = work.get("allOf").cloned() {
        let mut merged_props = Map::new();
        let mut merged_req: Vec<String> = Vec::new();
        let mut base_desc = work
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        for sub in all_of {
            if let Some(sub_obj) = sub.as_object() {
                let cleaned_sub = sanitize_schema_inner(sub_obj, root_schema, seen_refs);
                if let Some(Value::Object(props)) = cleaned_sub.get("properties") {
                    for (pk, pv) in props {
                        merged_props.insert(pk.clone(), pv.clone());
                    }
                }
                if let Some(Value::Array(reqs)) = cleaned_sub.get("required") {
                    for r in reqs {
                        if let Some(rs) = r.as_str() {
                            if !merged_req.contains(&rs.to_string()) {
                                merged_req.push(rs.to_string());
                            }
                        }
                    }
                }
                if base_desc.is_empty() {
                    if let Some(d) = cleaned_sub.get("description").and_then(|v| v.as_str()) {
                        base_desc = d.to_string();
                    }
                }
            }
        }
        if !merged_props.is_empty() {
            let existing = work
                .entry("properties".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(ref mut ex_map) = existing {
                for (k, v) in merged_props {
                    ex_map.entry(k).or_insert(v);
                }
            }
            work.entry("type".to_string())
                .or_insert_with(|| Value::String("object".to_string()));
        }
        if !merged_req.is_empty() {
            let mut cur_reqs: Vec<String> = work
                .get("required")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            for r in merged_req {
                if !cur_reqs.contains(&r) {
                    cur_reqs.push(r);
                }
            }
            work.insert(
                "required".to_string(),
                Value::Array(cur_reqs.into_iter().map(Value::String).collect()),
            );
        }
        if !base_desc.is_empty() && !work.contains_key("description") {
            work.insert("description".to_string(), Value::String(base_desc));
        }
    }

    let allowed_keys: HashSet<&str> = [
        "type",
        "description",
        "properties",
        "required",
        "items",
        "enum",
        "nullable",
        "anyOf",
        "format",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
    ]
    .into_iter()
    .collect();

    let supported_formats: HashSet<&str> = [
        "enum",
        "date-time",
        "date",
        "time",
        "int32",
        "int64",
        "float",
        "double",
    ]
    .into_iter()
    .collect();

    let mut cleaned = Map::new();

    if let Some(const_val) = work.get("const") {
        if !work.contains_key("enum") {
            cleaned.insert("enum".to_string(), Value::Array(vec![const_val.clone()]));
        }
    }

    for (k, v) in work.iter() {
        let mut key = k.as_str();
        if key == "oneOf" {
            key = "anyOf";
        }
        if !allowed_keys.contains(key) {
            continue;
        }
        if key == "format" {
            if let Some(fmt_str) = v.as_str() {
                if !supported_formats.contains(fmt_str) {
                    continue;
                }
            } else {
                continue;
            }
        }

        if key == "type" {
            if let Some(arr) = v.as_array() {
                let non_null: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str())
                    .filter(|&t| t != "null")
                    .map(|s| s.to_string())
                    .collect();
                let has_null = arr.iter().any(|x| x.as_str() == Some("null"));
                cleaned.insert(
                    "type".to_string(),
                    Value::String(
                        non_null
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "string".to_string()),
                    ),
                );
                if has_null {
                    cleaned.insert("nullable".to_string(), Value::Bool(true));
                }
            } else if v.is_string() {
                cleaned.insert("type".to_string(), v.clone());
            }
        } else if key == "properties" {
            if let Some(props_obj) = v.as_object() {
                let mut new_props = Map::new();
                for (pk, pv) in props_obj {
                    if let Some(pv_obj) = pv.as_object() {
                        new_props.insert(
                            pk.clone(),
                            Value::Object(sanitize_schema_inner(pv_obj, root_schema, seen_refs)),
                        );
                    } else if let Some(b) = pv.as_bool() {
                        new_props.insert(
                            pk.clone(),
                            if b {
                                json!({"type": "string"})
                            } else {
                                json!({"type": "string", "nullable": true})
                            },
                        );
                    }
                }
                cleaned.insert("properties".to_string(), Value::Object(new_props));
            }
        } else if key == "items" {
            if let Some(items_obj) = v.as_object() {
                cleaned.insert(
                    "items".to_string(),
                    Value::Object(sanitize_schema_inner(items_obj, root_schema, seen_refs)),
                );
            } else if let Some(items_arr) = v.as_array() {
                if let Some(first_obj) = items_arr.first().and_then(|x| x.as_object()) {
                    cleaned.insert(
                        "items".to_string(),
                        Value::Object(sanitize_schema_inner(first_obj, root_schema, seen_refs)),
                    );
                } else {
                    cleaned.insert("items".to_string(), json!({"type": "string"}));
                }
            }
        } else if key == "anyOf" {
            if let Some(any_arr) = v.as_array() {
                let non_null_branches: Vec<&Map<String, Value>> = any_arr
                    .iter()
                    .filter_map(|b| b.as_object())
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
                    .collect();
                let has_null = any_arr
                    .iter()
                    .filter_map(|b| b.as_object())
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("null"));
                if has_null {
                    cleaned.insert("nullable".to_string(), Value::Bool(true));
                }
                if non_null_branches.len() == 1 {
                    let merged =
                        sanitize_schema_inner(non_null_branches[0], root_schema, seen_refs);
                    for (mk, mv) in merged {
                        cleaned.entry(mk).or_insert(mv);
                    }
                } else if !non_null_branches.is_empty() {
                    let sanitized_branches: Vec<Value> = non_null_branches
                        .into_iter()
                        .map(|b| Value::Object(sanitize_schema_inner(b, root_schema, seen_refs)))
                        .collect();
                    cleaned.insert("anyOf".to_string(), Value::Array(sanitized_branches));
                }
            }
        } else if key == "enum" {
            if let Some(enum_arr) = v.as_array() {
                let mut str_vals = Vec::new();
                for item in enum_arr {
                    if item.is_null() {
                        cleaned.insert("nullable".to_string(), Value::Bool(true));
                    } else if let Some(s) = item.as_str() {
                        str_vals.push(Value::String(s.to_string()));
                    } else {
                        str_vals.push(Value::String(item.to_string()));
                    }
                }
                if !str_vals.is_empty() {
                    cleaned.insert("enum".to_string(), Value::Array(str_vals));
                    cleaned
                        .entry("type".to_string())
                        .or_insert_with(|| Value::String("string".to_string()));
                }
            }
        } else {
            cleaned.insert(key.to_string(), v.clone());
        }
    }

    if cleaned.contains_key("properties") && !cleaned.contains_key("type") {
        cleaned.insert("type".to_string(), Value::String("object".to_string()));
    }
    if cleaned.get("type").and_then(|v| v.as_str()) == Some("object") {
        cleaned
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if cleaned.get("type").and_then(|v| v.as_str()) == Some("array")
        && !cleaned.contains_key("items")
    {
        cleaned.insert("items".to_string(), json!({"type": "string"}));
    }

    if let Some(Value::Array(reqs)) = cleaned.get("required").cloned() {
        if let Some(Value::Object(props)) = cleaned.get("properties") {
            let valid_reqs: Vec<Value> = reqs
                .into_iter()
                .filter(|r| r.as_str().map(|s| props.contains_key(s)).unwrap_or(false))
                .collect();
            if valid_reqs.is_empty() {
                cleaned.remove("required");
            } else {
                cleaned.insert("required".to_string(), Value::Array(valid_reqs));
            }
        } else {
            cleaned.remove("required");
        }
    }

    if !cleaned.contains_key("type") && !cleaned.contains_key("anyOf") {
        cleaned.insert("type".to_string(), Value::String("string".to_string()));
    }

    cleaned
}

fn normalize_gemini_contents(contents: Vec<Value>) -> Vec<Value> {
    let mut normalized: Vec<Value> = Vec::new();
    for msg in contents {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("user")
            .to_string();
        let parts = match msg.get("parts").and_then(|p| p.as_array()) {
            Some(arr) if !arr.is_empty() => arr.clone(),
            _ => continue,
        };
        if let Some(last) = normalized.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(&role) {
                if let Some(last_parts) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                    last_parts.extend(parts);
                }
                continue;
            }
        }
        normalized.push(json!({
            "role": role,
            "parts": parts,
        }));
    }

    for msg in normalized.iter_mut() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        if role == "model" {
            let has_fc = msg
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().any(|pt| pt.get("functionCall").is_some()))
                .unwrap_or(false);
            if has_fc {
                let mut found_sig: Option<String> = None;
                let mut first_fc_idx: Option<usize> = None;
                if let Some(parts) = msg.get("parts").and_then(|p| p.as_array()) {
                    for (idx, pt) in parts.iter().enumerate() {
                        if let Some(sig) = pt.get("thoughtSignature").and_then(|s| s.as_str()) {
                            if !sig.is_empty() && found_sig.is_none() {
                                found_sig = Some(sig.to_string());
                            }
                        }
                        if pt.get("functionCall").is_some() && first_fc_idx.is_none() {
                            first_fc_idx = Some(idx);
                        }
                    }
                }
                if let Some(fc_idx) = first_fc_idx {
                    if let Some(parts) = msg.get_mut("parts").and_then(|p| p.as_array_mut()) {
                        let existing_sig = parts[fc_idx]
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string());
                        if existing_sig.is_none() {
                            let sig_to_set = found_sig
                                .or_else(|| SIGNATURE_STORE.get(None))
                                .unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
                            if let Some(obj) = parts[fc_idx].as_object_mut() {
                                obj.insert(
                                    "thoughtSignature".to_string(),
                                    Value::String(sig_to_set),
                                );
                            }
                        }
                    }
                }
            }
        } else if role == "user" {
            if let Some(parts) = msg.get_mut("parts").and_then(|p| p.as_array_mut()) {
                let mut fr_parts = Vec::new();
                let mut extra_texts = Vec::new();
                let mut non_text_other_parts = Vec::new();
                for pt in parts.drain(..) {
                    if pt.get("functionResponse").is_some() {
                        fr_parts.push(pt);
                    } else if let Some(t) = pt.get("text").and_then(|v| v.as_str()) {
                        if !t.is_empty() {
                            extra_texts.push(t.to_string());
                        }
                    } else {
                        non_text_other_parts.push(pt);
                    }
                }
                if !fr_parts.is_empty() {
                    // Vertex AI `gemini-4-argon` rejects a `"user"` turn that mixes
                    // `functionResponse` parts with `text` parts (`400 INVALID_ARGUMENT:
                    // "Requests ending with a model turn are not supported."`).
                    // Fold any accompanying text blocks (such as Claude Code Skill
                    // prompts or `<system-reminder>` blocks) into the last
                    // `functionResponse.response` payload so the turn consists purely
                    // of `functionResponse` parts.
                    if !extra_texts.is_empty() {
                        let combined_extra = extra_texts.join("\n\n");
                        if let Some(last_fr) = fr_parts.last_mut() {
                            if let Some(resp_obj) = last_fr
                                .get_mut("functionResponse")
                                .and_then(|fr| fr.get_mut("response"))
                                .and_then(|r| r.as_object_mut())
                            {
                                let key = if resp_obj.contains_key("error") {
                                    "error"
                                } else {
                                    "result"
                                };
                                let existing = resp_obj
                                    .get(key)
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let merged = if existing.is_empty() {
                                    combined_extra
                                } else {
                                    format!("{}\n\n{}", existing, combined_extra)
                                };
                                resp_obj.insert(key.to_string(), Value::String(merged));
                            }
                        }
                    }
                    fr_parts.extend(non_text_other_parts);
                    *parts = fr_parts;
                } else {
                    let mut rebuilt = Vec::new();
                    for t in extra_texts {
                        rebuilt.push(json!({"text": t}));
                    }
                    rebuilt.extend(non_text_other_parts);
                    *parts = rebuilt;
                }
            }
        }
    }

    // Ensure requests never end with a `model` turn (e.g. assistant prefill or
    // interrupted turn), which Vertex AI `gemini-4-argon` rejects with 400.
    if let Some(last_msg) = normalized.last() {
        if last_msg.get("role").and_then(|r| r.as_str()) == Some("model") {
            let mut trailing_fr_parts = Vec::new();
            if let Some(parts) = last_msg.get("parts").and_then(|p| p.as_array()) {
                for pt in parts {
                    if let Some(fc) = pt.get("functionCall").and_then(|v| v.as_object()) {
                        let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        trailing_fr_parts.push(json!({
                            "functionResponse": {
                                "name": name,
                                "response": {"result": "OK"},
                            }
                        }));
                    }
                }
            }
            if !trailing_fr_parts.is_empty() {
                normalized.push(json!({
                    "role": "user",
                    "parts": trailing_fr_parts,
                }));
            } else {
                normalized.push(json!({
                    "role": "user",
                    "parts": [{"text": "Continue."}],
                }));
            }
        }
    }

    normalized
}

fn ensure_historical_tools_declared(
    contents: &[Value],
    mut fn_declarations: Vec<Value>,
) -> Vec<Value> {
    let mut declared_names: HashSet<String> = HashSet::new();
    for d in &fn_declarations {
        if let Some(name) = d.get("name").and_then(|n| n.as_str()) {
            if !name.is_empty() {
                declared_names.insert(name.to_string());
                TOOL_SCHEMA_STORE.save(name, d);
            }
        }
    }

    for msg in contents {
        if let Some(parts) = msg.get("parts").and_then(|p| p.as_array()) {
            for part in parts {
                for key in ["functionCall", "functionResponse"] {
                    if let Some(fc) = part.get(key).and_then(|v| v.as_object()) {
                        if let Some(name) = fc.get("name").and_then(|n| n.as_str()) {
                            if !name.is_empty() && !declared_names.contains(name) {
                                declared_names.insert(name.to_string());
                                let decl = TOOL_SCHEMA_STORE.get(name).unwrap_or_else(|| {
                                    json!({
                                        "name": name,
                                        "description": format!("Tool {}", name),
                                        "parameters": {"type": "object", "properties": {}},
                                    })
                                });
                                fn_declarations.push(decl);
                            }
                        }
                    }
                }
            }
        }
    }
    fn_declarations
}

fn coerce_tool_result_content(content: Option<&Value>) -> String {
    let c = match content {
        Some(v) => v,
        None => return String::new(),
    };
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        let mut texts = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                texts.push(s.to_string());
            } else if let Some(obj) = item.as_object() {
                let btype = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if btype == "text" {
                    if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                        texts.push(t.to_string());
                    }
                } else if btype == "tool_reference" {
                    let tool_name = obj
                        .get("tool_name")
                        .or_else(|| obj.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    texts.push(format!("[tool_reference: {}]", tool_name));
                } else if let Ok(dumped) = serde_json::to_string(obj) {
                    texts.push(dumped);
                }
            }
        }
        return texts.join("\n");
    }
    serde_json::to_string(c).unwrap_or_default()
}

pub fn anthropic_to_gemini_request(body: &Value) -> (String, Value) {
    let model = resolve_gemini_model(body.get("model").and_then(|v| v.as_str()));
    let mut payload = Map::new();

    if let Some(system) = body.get("system") {
        let mut sys_parts = Vec::new();
        if let Some(s) = system.as_str() {
            if !s.trim().is_empty() {
                sys_parts.push(json!({"text": s}));
            }
        } else if let Some(arr) = system.as_array() {
            for block in arr {
                if let Some(s) = block.as_str() {
                    sys_parts.push(json!({"text": s}));
                } else if let Some(obj) = block.as_object() {
                    if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                sys_parts.push(json!({"text": t}));
                            }
                        }
                    }
                }
            }
        }
        if !sys_parts.is_empty() {
            payload.insert("systemInstruction".to_string(), json!({"parts": sys_parts}));
        }
    }

    let mut tool_id_to_name: HashMap<String, String> = HashMap::new();
    let empty_msgs = Vec::new();
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty_msgs);

    for msg in messages {
        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            for block in arr {
                if let Some(obj) = block.as_object() {
                    if obj.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let (Some(tid), Some(tname)) = (
                            obj.get("id").and_then(|v| v.as_str()),
                            obj.get("name").and_then(|v| v.as_str()),
                        ) {
                            tool_id_to_name.insert(tid.to_string(), tname.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut raw_contents: Vec<Value> = Vec::new();
    for msg in messages {
        let role_str = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let role = if role_str == "assistant" {
            "model"
        } else {
            "user"
        };
        let content = msg.get("content");
        let mut parts: Vec<Value> = Vec::new();
        let mut pending_signature: Option<String> = None;

        if let Some(s) = content.and_then(|c| c.as_str()) {
            if !s.is_empty() {
                parts.push(json!({"text": s}));
            }
        } else if let Some(arr) = content.and_then(|c| c.as_array()) {
            for block in arr {
                if let Some(s) = block.as_str() {
                    if !s.is_empty() {
                        parts.push(json!({"text": s}));
                    }
                    continue;
                }
                let obj = match block.as_object() {
                    Some(o) => o,
                    None => continue,
                };
                let btype = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        let text = obj.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            let mut part = json!({"text": text});
                            if let Some(sig) = pending_signature.take() {
                                part["thoughtSignature"] = Value::String(sig);
                            }
                            parts.push(part);
                        }
                    }
                    "thinking" => {
                        let thinking_text =
                            obj.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        let sig = obj
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string());
                        if !thinking_text.is_empty() {
                            parts.push(json!({
                                "text": thinking_text,
                                "thought": true,
                            }));
                        }
                        if let Some(s) = sig {
                            if !s.is_empty() {
                                pending_signature = Some(s);
                            }
                        }
                    }
                    "image" => {
                        if let Some(source) = obj.get("source").and_then(|s| s.as_object()) {
                            let stype = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if stype == "base64" {
                                parts.push(json!({
                                    "inlineData": {
                                        "mimeType": source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png"),
                                        "data": source.get("data").and_then(|d| d.as_str()).unwrap_or(""),
                                    }
                                }));
                            } else if stype == "url" {
                                parts.push(json!({
                                    "fileData": {
                                        "mimeType": source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/jpeg"),
                                        "fileUri": source.get("url").and_then(|u| u.as_str()).unwrap_or(""),
                                    }
                                }));
                            }
                        }
                    }
                    "tool_use" => {
                        let call_id = obj.get("id").and_then(|v| v.as_str());
                        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args = obj
                            .get("input")
                            .filter(|v| v.is_object())
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let mut fc_part = json!({
                            "functionCall": {
                                "name": name,
                                "args": args,
                            }
                        });
                        let sig = pending_signature
                            .take()
                            .or_else(|| {
                                obj.get("signature")
                                    .or_else(|| obj.get("thought_signature"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            })
                            .or_else(|| SIGNATURE_STORE.get(call_id))
                            .unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
                        fc_part["thoughtSignature"] = Value::String(sig);
                        parts.push(fc_part);
                    }
                    "tool_result" => {
                        let tool_use_id = obj
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tool_name = obj
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| tool_id_to_name.get(tool_use_id).cloned())
                            .unwrap_or_else(|| "tool".to_string());
                        let result_text = coerce_tool_result_content(obj.get("content"));
                        let is_error = obj
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let resp_payload = if is_error {
                            json!({"error": result_text})
                        } else {
                            json!({"result": result_text})
                        };
                        parts.push(json!({
                            "functionResponse": {
                                "name": tool_name,
                                "response": resp_payload,
                            }
                        }));
                    }
                    _ => {}
                }
            }
        }

        if let Some(sig) = pending_signature.take() {
            if let Some(last_part) = parts.last_mut().and_then(|p| p.as_object_mut()) {
                last_part
                    .entry("thoughtSignature".to_string())
                    .or_insert(Value::String(sig));
            }
        }

        if !parts.is_empty() {
            raw_contents.push(json!({
                "role": role,
                "parts": parts,
            }));
        }
    }

    let normalized_contents = normalize_gemini_contents(raw_contents);
    payload.insert(
        "contents".to_string(),
        Value::Array(normalized_contents.clone()),
    );

    let mut fn_declarations: Vec<Value> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            if let Some(tobj) = tool.as_object() {
                if let Some(name) = tobj.get("name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        let schema = tobj
                            .get("input_schema")
                            .or_else(|| tobj.get("parameters"))
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        fn_declarations.push(json!({
                            "name": name,
                            "description": tobj.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                            "parameters": sanitize_json_schema(&schema),
                        }));
                    }
                }
            }
        }
    }

    fn_declarations = ensure_historical_tools_declared(&normalized_contents, fn_declarations);
    if !fn_declarations.is_empty() {
        payload.insert(
            "tools".to_string(),
            json!([{"functionDeclarations": fn_declarations}]),
        );
    }

    if let Some(tc) = body.get("tool_choice").and_then(|t| t.as_object()) {
        let tc_type = tc.get("type").and_then(|t| t.as_str()).unwrap_or("auto");
        if tc_type == "any" {
            payload.insert(
                "toolConfig".to_string(),
                json!({"functionCallingConfig": {"mode": "ANY"}}),
            );
        } else if tc_type == "tool" {
            if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
                payload.insert(
                    "toolConfig".to_string(),
                    json!({
                        "functionCallingConfig": {
                            "mode": "ANY",
                            "allowedFunctionNames": [name],
                        }
                    }),
                );
            }
        } else if tc_type == "none" {
            payload.insert(
                "toolConfig".to_string(),
                json!({"functionCallingConfig": {"mode": "NONE"}}),
            );
        }
    }

    let mut gen_cfg = Map::new();
    if let Some(max_tok) = body.get("max_tokens").and_then(|v| v.as_u64()) {
        gen_cfg.insert("maxOutputTokens".to_string(), json!(max_tok));
    }
    if let Some(temp) = body.get("temperature") {
        if !temp.is_null() {
            gen_cfg.insert("temperature".to_string(), temp.clone());
        }
    }
    if let Some(top_p) = body.get("top_p") {
        if !top_p.is_null() {
            gen_cfg.insert("topP".to_string(), top_p.clone());
        }
    }
    if let Some(top_k) = body.get("top_k") {
        if !top_k.is_null() {
            gen_cfg.insert("topK".to_string(), top_k.clone());
        }
    }
    if let Some(stop_seqs) = body.get("stop_sequences").and_then(|v| v.as_array()) {
        if !stop_seqs.is_empty() {
            gen_cfg.insert("stopSequences".to_string(), Value::Array(stop_seqs.clone()));
        }
    }
    let hot_swap = body
        .get("_router_hot_swap")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let router_effort = body
        .get("_router_reasoning_effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());

    if let Some(thinking) = body.get("thinking").and_then(|v| v.as_object()) {
        if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled") {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|b| b.as_u64())
                .unwrap_or(4096);
            if model.starts_with("gemini-2.5") {
                gen_cfg.insert(
                    "thinkingConfig".to_string(),
                    json!({
                        "includeThoughts": true,
                        "thinkingBudget": budget,
                    }),
                );
            } else {
                let level = if hot_swap {
                    if let Some(eff) = router_effort {
                        effort_to_thinking_level(eff)
                    } else if budget >= 8192 {
                        "HIGH"
                    } else {
                        "MEDIUM"
                    }
                } else if budget >= 8192 {
                    "HIGH"
                } else if budget <= 1024 {
                    "LOW"
                } else {
                    "MEDIUM"
                };
                gen_cfg.insert(
                    "thinkingConfig".to_string(),
                    json!({
                        "includeThoughts": true,
                        "thinkingLevel": level,
                    }),
                );
            }
        }
    } else if hot_swap && !model.starts_with("gemini-2.5") {
        if let Some(eff) = router_effort {
            gen_cfg.insert(
                "thinkingConfig".to_string(),
                json!({
                    "includeThoughts": true,
                    "thinkingLevel": effort_to_thinking_level(eff),
                }),
            );
        }
    }
    if !gen_cfg.is_empty() {
        payload.insert("generationConfig".to_string(), Value::Object(gen_cfg));
    }

    (model, Value::Object(payload))
}

pub fn extract_candidate_parts(gemini_resp: &Value) -> (Vec<Value>, String, Value) {
    let candidates = gemini_resp
        .get("candidates")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let usage = gemini_resp
        .get("usageMetadata")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if candidates.is_empty() {
        return (Vec::new(), "STOP".to_string(), usage);
    }
    let cand = &candidates[0];
    let parts = cand
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let finish_reason = cand
        .get("finishReason")
        .and_then(|f| f.as_str())
        .unwrap_or("STOP")
        .to_string();
    (parts, finish_reason, usage)
}

pub fn build_safeguard_results(tool_use_ids: &[String]) -> Value {
    let mut tool_uses = Map::new();
    for id in tool_use_ids {
        tool_uses.insert(
            id.clone(),
            json!({
                "type": "evaluated",
                "outcome": "not_flagged",
            }),
        );
    }
    json!([
        {
            "type": "dangerous_tool_use",
            "status": {
                "type": "available",
                "tool_uses": Value::Object(tool_uses),
            }
        }
    ])
}

pub fn gemini_to_anthropic_response(gemini_resp: &Value, requested_model: &str) -> Value {
    gemini_to_anthropic_response_with_hint(gemini_resp, requested_model, None)
}

pub fn gemini_to_anthropic_response_with_hint(
    gemini_resp: &Value,
    requested_model: &str,
    cache_hint: Option<&AnthropicCacheHint>,
) -> Value {
    let (parts, finish_reason, usage) = extract_candidate_parts(gemini_resp);
    let mut content_blocks: Vec<Value> = Vec::new();
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut has_tool_use = false;

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

        if pobj.get("thought").and_then(|v| v.as_bool()) == Some(true) {
            let mut block = json!({
                "type": "thinking",
                "thinking": pobj.get("text").and_then(|t| t.as_str()).unwrap_or(""),
            });
            if let Some(s) = sig {
                block["signature"] = Value::String(s);
            }
            content_blocks.push(block);
        } else if let Some(fc) = pobj.get("functionCall").and_then(|v| v.as_object()) {
            has_tool_use = true;
            let call_id = format!("toolu_{}", new_short_id(24));
            if let Some(ref s) = sig {
                SIGNATURE_STORE.save(Some(&call_id), Some(s));
            }
            tool_use_ids.push(call_id.clone());
            content_blocks.push(json!({
                "type": "tool_use",
                "id": call_id,
                "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                "input": fc.get("args").cloned().unwrap_or_else(|| json!({})),
            }));
        } else if let Some(text) = pobj.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                content_blocks.push(json!({
                    "type": "text",
                    "text": text,
                }));
            }
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(json!({"type": "text", "text": ""}));
    }

    let stop_reason = if has_tool_use {
        "tool_use"
    } else {
        match finish_reason.as_str() {
            "MAX_TOKENS" => "max_tokens",
            "STOP" => "end_turn",
            _ => "end_turn",
        }
    };

    let (prompt_tokens, output_tokens, cached_tokens, _reasoning_tokens) =
        extract_usage_counts(&usage);
    let (uncached_input_tokens, effective_cached_tokens) =
        resolve_effective_anthropic_cache_usage(prompt_tokens, cached_tokens, cache_hint);

    json!({
        "id": format!("msg_{}", new_short_id(24)),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "safeguard_results": build_safeguard_results(&tool_use_ids),
        "usage": {
            "input_tokens": uncached_input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": effective_cached_tokens,
        }
    })
}

pub fn openai_chat_to_gemini_request(body: &Value) -> (String, Value) {
    let model = resolve_gemini_model(body.get("model").and_then(|v| v.as_str()));
    let mut payload = Map::new();
    let mut sys_parts: Vec<Value> = Vec::new();
    let mut raw_contents: Vec<Value> = Vec::new();

    let empty_msgs = Vec::new();
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty_msgs);

    let mut tool_id_to_name: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool");
                    tool_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        if role == "system" || role == "developer" {
            let mut extracted = Vec::new();
            if let Some(s) = content.and_then(|c| c.as_str()) {
                if !s.is_empty() {
                    extracted.push(json!({"text": s}));
                }
            } else if let Some(arr) = content.and_then(|c| c.as_array()) {
                for item in arr {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        if !t.is_empty() {
                            extracted.push(json!({"text": t}));
                        }
                    }
                }
            }
            if !extracted.is_empty() {
                if raw_contents.is_empty() {
                    // Leading system/developer messages form the static systemInstruction prefix
                    sys_parts.extend(extracted);
                } else {
                    // Mid-conversation system/developer messages stay in-place as user turns
                    // so `systemInstruction` at the start of the prompt never mutates and
                    // invalidates Vertex AI's context cache prefix.
                    raw_contents.push(json!({
                        "role": "user",
                        "parts": extracted,
                    }));
                }
            }
            continue;
        }

        if role == "tool" {
            let call_id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let fn_name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| tool_id_to_name.get(call_id).cloned())
                .unwrap_or_else(|| "tool".to_string());
            let result_str = match content {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                None => String::new(),
            };
            raw_contents.push(json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": fn_name,
                        "response": {"result": result_str},
                    }
                }]
            }));
            continue;
        }

        let gemini_role = if role == "assistant" { "model" } else { "user" };
        let mut parts: Vec<Value> = Vec::new();

        if let Some(s) = content.and_then(|c| c.as_str()) {
            if !s.is_empty() {
                parts.push(json!({"text": s}));
            }
        } else if let Some(arr) = content.and_then(|c| c.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        parts.push(json!({"text": s}));
                    }
                } else if let Some(obj) = item.as_object() {
                    let itype = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if itype == "text" || itype == "input_text" || itype == "output_text" {
                        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                parts.push(json!({"text": t}));
                            }
                        }
                    }
                }
            }
        }

        if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let fn_obj = tc.get("function").and_then(|f| f.as_object());
                let name = fn_obj
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool");
                let raw_args = fn_obj.and_then(|f| f.get("arguments"));
                let args = match raw_args {
                    Some(Value::String(s)) => {
                        serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({"raw": s}))
                    }
                    Some(Value::Object(m)) => Value::Object(m.clone()),
                    _ => json!({}),
                };
                let call_id = tc.get("id").and_then(|v| v.as_str());
                let sig = tc
                    .get("thought_signature")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| SIGNATURE_STORE.get(call_id))
                    .unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
                parts.push(json!({
                    "functionCall": {
                        "name": name,
                        "args": args,
                    },
                    "thoughtSignature": sig,
                }));
            }
        }

        if !parts.is_empty() {
            raw_contents.push(json!({
                "role": gemini_role,
                "parts": parts,
            }));
        }
    }

    if !sys_parts.is_empty() {
        payload.insert("systemInstruction".to_string(), json!({"parts": sys_parts}));
    }

    let normalized_contents = normalize_gemini_contents(raw_contents);
    payload.insert(
        "contents".to_string(),
        Value::Array(normalized_contents.clone()),
    );

    let mut fn_declarations: Vec<Value> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            if let Some(tobj) = tool.as_object() {
                let fn_def = if tobj.get("type").and_then(|t| t.as_str()) == Some("function") {
                    tobj.get("function")
                        .and_then(|f| f.as_object())
                        .unwrap_or(tobj)
                } else {
                    tobj
                };
                if let Some(name) = fn_def.get("name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        let schema = fn_def
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        fn_declarations.push(json!({
                            "name": name,
                            "description": fn_def.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                            "parameters": sanitize_json_schema(&schema),
                        }));
                    }
                }
            }
        }
    }

    fn_declarations = ensure_historical_tools_declared(&normalized_contents, fn_declarations);
    if !fn_declarations.is_empty() {
        payload.insert(
            "tools".to_string(),
            json!([{"functionDeclarations": fn_declarations}]),
        );
    }

    let mut gen_cfg = Map::new();
    let max_tok = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(|v| v.as_u64());
    if let Some(mt) = max_tok {
        gen_cfg.insert("maxOutputTokens".to_string(), json!(mt));
    }
    if let Some(temp) = body.get("temperature") {
        if !temp.is_null() {
            gen_cfg.insert("temperature".to_string(), temp.clone());
        }
    }
    if let Some(top_p) = body.get("top_p") {
        if !top_p.is_null() {
            gen_cfg.insert("topP".to_string(), top_p.clone());
        }
    }
    let hot_swap = body
        .get("_router_hot_swap")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let router_effort = body
        .get("_router_reasoning_effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let req_effort = body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or_else(|| {
            body.get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.trim().is_empty());
    let effective_effort = if hot_swap {
        router_effort.or(req_effort)
    } else {
        req_effort.or(router_effort)
    };
    if let Some(effort) = effective_effort {
        if !model.starts_with("gemini-2.5") {
            let level = effort_to_thinking_level(effort);
            gen_cfg.insert(
                "thinkingConfig".to_string(),
                json!({"thinkingLevel": level}),
            );
        }
    }
    if !gen_cfg.is_empty() {
        payload.insert("generationConfig".to_string(), Value::Object(gen_cfg));
    }

    (model, Value::Object(payload))
}

pub fn gemini_to_openai_chat_response(gemini_resp: &Value, requested_model: &str) -> Value {
    let (parts, finish_reason, usage) = extract_candidate_parts(gemini_resp);
    let mut text_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

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

        if pobj.get("thought").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(fc) = pobj.get("functionCall").and_then(|v| v.as_object()) {
            let call_id = format!("call_{}", new_short_id(24));
            if let Some(ref s) = sig {
                SIGNATURE_STORE.save(Some(&call_id), Some(s));
            }
            let args_str =
                serde_json::to_string(&fc.get("args").cloned().unwrap_or_else(|| json!({})))
                    .unwrap_or_else(|_| "{}".to_string());
            tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                    "arguments": args_str,
                }
            }));
        } else if let Some(t) = pobj.get("text").and_then(|v| v.as_str()) {
            text_chunks.push(t.to_string());
        }
    }

    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    if !text_chunks.is_empty() || tool_calls.is_empty() {
        message.insert("content".to_string(), Value::String(text_chunks.join("")));
    } else {
        message.insert("content".to_string(), Value::Null);
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls.clone()));
    }

    let openai_finish = if !tool_calls.is_empty() {
        "tool_calls"
    } else if finish_reason == "MAX_TOKENS" {
        "length"
    } else {
        "stop"
    };

    let (prompt_tokens, completion_tokens, cached_tokens, reasoning_tokens) =
        extract_usage_counts(&usage);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    json!({
        "id": format!("chatcmpl-{}", new_short_id(24)),
        "object": "chat.completion",
        "created": now_secs,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": openai_finish,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "prompt_tokens_details": {
                "cached_tokens": cached_tokens,
            },
            "completion_tokens": completion_tokens,
            "completion_tokens_details": {
                "reasoning_tokens": reasoning_tokens,
            },
            "total_tokens": prompt_tokens + completion_tokens,
        }
    })
}

pub fn openai_responses_to_gemini_request(body: &Value) -> (String, Value) {
    let model = resolve_gemini_model(body.get("model").and_then(|v| v.as_str()));
    let mut payload = Map::new();

    let prev_state = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .and_then(|id| RESPONSE_CACHE_STORE.get(id.trim()))
        .or_else(|| {
            body.get("prompt_cache_key")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .and_then(|k| RESPONSE_CACHE_STORE.get(k.trim()))
        });

    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.trim().is_empty() {
            payload.insert(
                "systemInstruction".to_string(),
                json!({"parts": [{"text": instructions}]}),
            );
        }
    }

    let input_data = body.get("input");
    let mut raw_contents: Vec<Value> = Vec::new();
    let mut call_id_to_name: HashMap<String, String> = HashMap::new();

    if let Some(ref ps) = prev_state {
        call_id_to_name.extend(ps.call_id_to_name.clone());
    }

    if let Some(s) = input_data.and_then(|v| v.as_str()) {
        raw_contents.push(json!({
            "role": "user",
            "parts": [{"text": s}],
        }));
    } else if let Some(arr) = input_data.and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(obj) = item.as_object() {
                if obj.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    if let (Some(cid), Some(name)) = (
                        obj.get("call_id")
                            .or_else(|| obj.get("id"))
                            .and_then(|v| v.as_str()),
                        obj.get("name").and_then(|v| v.as_str()),
                    ) {
                        call_id_to_name.insert(cid.to_string(), name.to_string());
                    }
                }
            }
        }

        for item in arr {
            if let Some(s) = item.as_str() {
                raw_contents.push(json!({
                    "role": "user",
                    "parts": [{"text": s}],
                }));
                continue;
            }
            let obj = match item.as_object() {
                Some(o) => o,
                None => continue,
            };
            let item_type = obj
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("message");
            if item_type == "function_call" {
                let raw_args = obj.get("arguments");
                let args = match raw_args {
                    Some(Value::String(s)) => {
                        serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({"raw": s}))
                    }
                    Some(Value::Object(m)) => Value::Object(m.clone()),
                    _ => json!({}),
                };
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str());
                let sig = obj
                    .get("thought_signature")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| SIGNATURE_STORE.get(call_id))
                    .unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
                raw_contents.push(json!({
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": obj.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                            "args": args,
                        },
                        "thoughtSignature": sig,
                    }]
                }));
            } else if item_type == "function_call_output" {
                let cid = obj.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let fn_name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| call_id_to_name.get(cid).cloned())
                    .unwrap_or_else(|| "tool".to_string());
                let output = match obj.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                raw_contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": fn_name,
                            "response": {"result": output},
                        }
                    }]
                }));
            } else {
                let role = obj.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                if role == "system" || role == "developer" {
                    let mut sys_texts: Vec<String> = Vec::new();
                    if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                        if !s.is_empty() {
                            sys_texts.push(s.to_string());
                        }
                    } else if let Some(carr) = obj.get("content").and_then(|c| c.as_array()) {
                        for c in carr {
                            if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    sys_texts.push(t.to_string());
                                }
                            }
                        }
                    }
                    if !sys_texts.is_empty() {
                        let joined = sys_texts.join("\n");
                        if raw_contents.is_empty() {
                            // Leading developer/system items belong in the static systemInstruction prefix
                            let existing = payload
                                .entry("systemInstruction".to_string())
                                .or_insert_with(|| json!({"parts": []}));
                            if let Some(parts_arr) =
                                existing.get_mut("parts").and_then(|p| p.as_array_mut())
                            {
                                parts_arr.push(json!({"text": joined}));
                            }
                        } else {
                            // Mid-conversation developer/system items stay in-place as user turns
                            // so `systemInstruction` at the root of the request never mutates and
                            // invalidates Vertex AI's context cache prefix.
                            raw_contents.push(json!({
                                "role": "user",
                                "parts": [{"text": joined}],
                            }));
                        }
                    }
                    continue;
                }
                let gemini_role = if role == "assistant" { "model" } else { "user" };
                let mut parts: Vec<Value> = Vec::new();
                if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                    if !s.is_empty() {
                        parts.push(json!({"text": s}));
                    }
                } else if let Some(carr) = obj.get("content").and_then(|c| c.as_array()) {
                    for c in carr {
                        if let Some(s) = c.as_str() {
                            if !s.is_empty() {
                                parts.push(json!({"text": s}));
                            }
                        } else if let Some(cobj) = c.as_object() {
                            let ctype = cobj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if ctype == "input_text" || ctype == "output_text" || ctype == "text" {
                                if let Some(t) = cobj.get("text").and_then(|v| v.as_str()) {
                                    if !t.is_empty() {
                                        parts.push(json!({"text": t}));
                                    }
                                }
                            }
                        }
                    }
                }
                if !parts.is_empty() {
                    raw_contents.push(json!({
                        "role": gemini_role,
                        "parts": parts,
                    }));
                }
            }
        }
    }

    if let Some(ref ps) = prev_state {
        if !payload.contains_key("systemInstruction") {
            if let Some(ref sys) = ps.system_instruction {
                payload.insert("systemInstruction".to_string(), sys.clone());
            }
        }
        // If `previous_response_id` was explicitly passed and `raw_contents` only contains
        // incremental delta items (does not already start with the cached first turn),
        // prepend the cached conversation prefix so Vertex AI hits its prefix cache.
        if body
            .get("previous_response_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
            && !ps.contents.is_empty()
        {
            let already_has_prefix = raw_contents.first() == ps.contents.first()
                && raw_contents.len() > ps.contents.len();
            if !already_has_prefix {
                let mut combined = ps.contents.clone();
                combined.extend(raw_contents);
                raw_contents = combined;
            }
        }
    }

    let normalized_contents = normalize_gemini_contents(raw_contents);
    payload.insert(
        "contents".to_string(),
        Value::Array(normalized_contents.clone()),
    );

    let mut fn_declarations: Vec<Value> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            if let Some(tobj) = tool.as_object() {
                let fn_def = if tobj.get("type").and_then(|t| t.as_str()) == Some("function")
                    && tobj.contains_key("function")
                {
                    tobj.get("function")
                        .and_then(|f| f.as_object())
                        .unwrap_or(tobj)
                } else {
                    tobj
                };
                if let Some(name) = fn_def.get("name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        let schema = fn_def
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        fn_declarations.push(json!({
                            "name": name,
                            "description": fn_def.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                            "parameters": sanitize_json_schema(&schema),
                        }));
                    }
                }
            }
        }
    } else if let Some(ref ps) = prev_state {
        if let Some(cached_decls) = ps
            .tools
            .as_ref()
            .and_then(|t| t.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("functionDeclarations"))
            .and_then(|fd| fd.as_array())
        {
            fn_declarations = cached_decls.clone();
        }
    }

    fn_declarations = ensure_historical_tools_declared(&normalized_contents, fn_declarations);
    if !fn_declarations.is_empty() {
        payload.insert(
            "tools".to_string(),
            json!([{"functionDeclarations": fn_declarations}]),
        );
    }

    let mut gen_cfg = Map::new();
    if let Some(mt) = body.get("max_output_tokens").and_then(|v| v.as_u64()) {
        gen_cfg.insert("maxOutputTokens".to_string(), json!(mt));
    }
    if let Some(temp) = body.get("temperature") {
        if !temp.is_null() {
            gen_cfg.insert("temperature".to_string(), temp.clone());
        }
    }
    let hot_swap = body
        .get("_router_hot_swap")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let router_effort = body
        .get("_router_reasoning_effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let req_effort = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|v| v.as_str())
        .or_else(|| body.get("reasoning_effort").and_then(|v| v.as_str()))
        .filter(|s| !s.trim().is_empty());
    let effective_effort = if hot_swap {
        router_effort.or(req_effort)
    } else {
        req_effort.or(router_effort)
    };
    if let Some(effort) = effective_effort {
        if !model.starts_with("gemini-2.5") {
            let level = effort_to_thinking_level(effort);
            gen_cfg.insert(
                "thinkingConfig".to_string(),
                json!({"thinkingLevel": level}),
            );
        }
    }
    if !gen_cfg.is_empty() {
        payload.insert("generationConfig".to_string(), Value::Object(gen_cfg));
    }

    (model, Value::Object(payload))
}

pub fn gemini_to_openai_responses(gemini_resp: &Value, requested_model: &str) -> Value {
    let (parts, _finish_reason, usage) = extract_candidate_parts(gemini_resp);
    let mut output_items: Vec<Value> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

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

        if pobj.get("thought").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(fc) = pobj.get("functionCall").and_then(|v| v.as_object()) {
            let call_id = format!("call_{}", new_short_id(24));
            if let Some(ref s) = sig {
                SIGNATURE_STORE.save(Some(&call_id), Some(s));
            }
            let args_str =
                serde_json::to_string(&fc.get("args").cloned().unwrap_or_else(|| json!({})))
                    .unwrap_or_else(|_| "{}".to_string());
            output_items.push(json!({
                "id": format!("fc_{}", new_short_id(24)),
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                "arguments": args_str,
            }));
        } else if let Some(t) = pobj.get("text").and_then(|v| v.as_str()) {
            text_parts.push(t.to_string());
        }
    }

    if !text_parts.is_empty() || output_items.is_empty() {
        output_items.insert(
            0,
            json!({
                "id": format!("msg_{}", new_short_id(24)),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text_parts.join(""),
                    "annotations": [],
                }],
            }),
        );
    }

    let (input_tokens, output_tokens, cached_tokens, reasoning_tokens) =
        extract_usage_counts(&usage);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    json!({
        "id": format!("resp_{}", new_short_id(24)),
        "object": "response",
        "created_at": now_secs,
        "status": "completed",
        "model": requested_model,
        "output": output_items,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {
                "cached_tokens": cached_tokens,
                "cache_write_tokens": 0,
            },
            "output_tokens": output_tokens,
            "output_tokens_details": {
                "reasoning_tokens": reasoning_tokens,
            },
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

pub fn format_sse(event: Option<&str>, data: &Value) -> String {
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    if let Some(ev) = event {
        format!("event: {}\ndata: {}\n\n", ev, payload)
    } else {
        format!("data: {}\n\n", payload)
    }
}

pub struct AnthropicStreamState {
    pub requested_model: String,
    pub msg_id: String,
    pub started: bool,
    pub block_index: usize,
    pub current_block_type: Option<String>,
    pub has_tool_use: bool,
    pub tool_use_ids: Vec<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hint: Option<AnthropicCacheHint>,
}

impl AnthropicStreamState {
    pub fn new(requested_model: &str) -> Self {
        Self::with_cache_hint(requested_model, None)
    }

    pub fn with_cache_hint(requested_model: &str, cache_hint: Option<AnthropicCacheHint>) -> Self {
        Self {
            requested_model: requested_model.to_string(),
            msg_id: format!("msg_{}", new_short_id(24)),
            started: false,
            block_index: 0,
            current_block_type: None,
            has_tool_use: false,
            tool_use_ids: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cache_hint,
        }
    }

    pub fn process_gemini_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let (parts, _finish_reason, usage) = extract_candidate_parts(chunk);
        if let Some(pt) = usage.get("promptTokenCount").and_then(|v| v.as_u64()) {
            self.input_tokens = pt;
        }
        if let Some(ct) = usage.get("candidatesTokenCount").and_then(|v| v.as_u64()) {
            self.output_tokens = ct;
        }
        if let Some(cached) = usage
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64())
        {
            self.cached_tokens = cached;
        }
        if let Some(thoughts) = usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()) {
            self.reasoning_tokens = thoughts;
        }

        if !self.started {
            self.started = true;
            let (uncached_in, effective_cached) = resolve_effective_anthropic_cache_usage(
                self.input_tokens,
                self.cached_tokens,
                self.cache_hint.as_ref(),
            );
            out.push(format_sse(
                Some("message_start"),
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.msg_id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.requested_model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {
                            "input_tokens": uncached_in,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": effective_cached,
                        },
                    }
                }),
            ));
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

            if pobj.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                let text = pobj.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if self.current_block_type.as_deref() != Some("thinking") {
                    if self.current_block_type.is_some() {
                        out.push(format_sse(
                            Some("content_block_stop"),
                            &json!({"type": "content_block_stop", "index": self.block_index}),
                        ));
                        self.block_index += 1;
                    }
                    self.current_block_type = Some("thinking".to_string());
                    out.push(format_sse(
                        Some("content_block_start"),
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "thinking", "thinking": ""},
                        }),
                    ));
                }
                if !text.is_empty() {
                    out.push(format_sse(
                        Some("content_block_delta"),
                        &json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": {"type": "thinking_delta", "thinking": text},
                        }),
                    ));
                }
                if let Some(s) = sig {
                    out.push(format_sse(
                        Some("content_block_delta"),
                        &json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": {"type": "signature_delta", "signature": s},
                        }),
                    ));
                }
            } else if let Some(fc) = pobj.get("functionCall").and_then(|v| v.as_object()) {
                self.has_tool_use = true;
                if self.current_block_type.is_some() {
                    out.push(format_sse(
                        Some("content_block_stop"),
                        &json!({"type": "content_block_stop", "index": self.block_index}),
                    ));
                    self.block_index += 1;
                    self.current_block_type = None;
                }
                let call_id = format!("toolu_{}", new_short_id(24));
                if let Some(ref s) = sig {
                    SIGNATURE_STORE.save(Some(&call_id), Some(s));
                }
                self.tool_use_ids.push(call_id.clone());
                let args_json =
                    serde_json::to_string(&fc.get("args").cloned().unwrap_or_else(|| json!({})))
                        .unwrap_or_else(|_| "{}".to_string());

                out.push(format_sse(
                    Some("content_block_start"),
                    &json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool"),
                            "input": {},
                        }
                    }),
                ));
                out.push(format_sse(
                    Some("content_block_delta"),
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": args_json,
                        }
                    }),
                ));
                out.push(format_sse(
                    Some("content_block_stop"),
                    &json!({"type": "content_block_stop", "index": self.block_index}),
                ));
                self.block_index += 1;
            } else if let Some(text) = pobj.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    if self.current_block_type.as_deref() != Some("text") {
                        if self.current_block_type.is_some() {
                            out.push(format_sse(
                                Some("content_block_stop"),
                                &json!({"type": "content_block_stop", "index": self.block_index}),
                            ));
                            self.block_index += 1;
                        }
                        self.current_block_type = Some("text".to_string());
                        out.push(format_sse(
                            Some("content_block_start"),
                            &json!({
                                "type": "content_block_start",
                                "index": self.block_index,
                                "content_block": {"type": "text", "text": ""},
                            }),
                        ));
                    }
                    out.push(format_sse(
                        Some("content_block_delta"),
                        &json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    ));
                }
            }
        }

        out
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.process_gemini_chunk(&json!({})));
        }
        if self.current_block_type.is_some() {
            out.push(format_sse(
                Some("content_block_stop"),
                &json!({"type": "content_block_stop", "index": self.block_index}),
            ));
            self.current_block_type = None;
        }
        let stop_reason = if self.has_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };
        let (uncached_in, effective_cached) = resolve_effective_anthropic_cache_usage(
            self.input_tokens,
            self.cached_tokens,
            self.cache_hint.as_ref(),
        );
        out.push(format_sse(
            Some("message_delta"),
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": Value::Null,
                    "safeguard_results": build_safeguard_results(&self.tool_use_ids),
                },
                "usage": {
                    "input_tokens": uncached_in,
                    "output_tokens": self.output_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": effective_cached,
                },
            }),
        ));
        out.push(format_sse(
            Some("message_stop"),
            &json!({"type": "message_stop"}),
        ));
        out
    }
}
