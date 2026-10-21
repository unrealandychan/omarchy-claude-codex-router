use omarchy_claude_codex_router::adc_auth::ADCAuthManager;
use omarchy_claude_codex_router::server::{RouterResponse, RouterServer};
use omarchy_claude_codex_router::translator::{
    anthropic_to_gemini_request, gemini_to_anthropic_response, gemini_to_openai_chat_response,
    gemini_to_openai_responses, openai_chat_to_gemini_request, openai_responses_to_gemini_request,
    resolve_gemini_model, sanitize_json_schema, AnthropicStreamState,
    SKIP_THOUGHT_SIGNATURE_VALIDATOR, SIGNATURE_STORE,
};
use serde_json::{json, Value};
use std::sync::Mutex;

static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn test_adc_strips_api_key_env_vars() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("GEMINI_API_KEY", "AIzaFakeKey123");
    std::env::set_var("GOOGLE_API_KEY", "AIzaFakeKey456");

    let auth = ADCAuthManager::new(Some("test-project".to_string()), Some("global".to_string()));
    assert!(std::env::var("GEMINI_API_KEY").is_err());
    assert!(std::env::var("GOOGLE_API_KEY").is_err());
    assert!(auth.stripped_vars.contains(&"GEMINI_API_KEY".to_string()));
    assert!(auth.stripped_vars.contains(&"GOOGLE_API_KEY".to_string()));
    assert_eq!(
        std::env::var("GOOGLE_API_USE_CLIENT_CERTIFICATE").unwrap_or_default(),
        "false"
    );
}

#[test]
fn test_model_resolution() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(resolve_gemini_model(None), "gemini-3.1-pro-preview");
    assert_eq!(
        resolve_gemini_model(Some("claude-sonnet-4-6")),
        "gemini-3.1-pro-preview"
    );
    assert_eq!(
        resolve_gemini_model(Some("claude-haiku-4-5")),
        "gemini-3-flash-preview"
    );
    assert_eq!(
        resolve_gemini_model(Some("gpt-5-codex")),
        "gemini-3.1-pro-preview"
    );
    assert_eq!(
        resolve_gemini_model(Some("gemini-2.5-pro")),
        "gemini-2.5-pro"
    );
    assert_eq!(
        resolve_gemini_model(Some("gemini-4-argon")),
        "gemini-4-argon"
    );
    assert_eq!(
        resolve_gemini_model(Some("gemini-3.8-flash")),
        "gemini-3.8-flash"
    );
}

#[test]
fn test_sanitize_json_schema_complex_claude_and_skill_schemas() {
    let raw_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "SkillInput": {
                "type": "object",
                "properties": {
                    "skill": {"type": "string", "description": "Skill name"},
                    "args": {"type": ["string", "null"], "format": "uri"},
                },
                "required": ["skill", "non_existent_prop"],
            },
            "EmptyConfig": {
                "type": "object",
            },
        },
        "allOf": [{"$ref": "#/$defs/SkillInput"}],
        "properties": {
            "config": {"$ref": "#/$defs/EmptyConfig"},
            "mode": {"const": "execute"},
            "target": {
                "anyOf": [
                    {"type": "string"},
                    {"type": "null"},
                ]
            },
        },
        "additionalProperties": false,
    });

    let cleaned = sanitize_json_schema(&raw_schema);
    assert_eq!(cleaned["type"], "object");
    assert_eq!(cleaned["properties"]["skill"]["type"], "string");
    assert_eq!(cleaned["properties"]["args"]["type"], "string");
    assert_eq!(cleaned["properties"]["args"]["nullable"], true);
    assert!(cleaned["properties"]["args"].get("format").is_none());
    assert_eq!(cleaned["required"], json!(["skill"]));
    assert_eq!(cleaned["properties"]["config"]["type"], "object");
    assert_eq!(cleaned["properties"]["config"]["properties"], json!({}));
    assert_eq!(cleaned["properties"]["mode"]["enum"], json!(["execute"]));
    assert_eq!(cleaned["properties"]["mode"]["type"], "string");
    assert_eq!(cleaned["properties"]["target"]["type"], "string");
    assert_eq!(cleaned["properties"]["target"]["nullable"], true);
}

#[test]
fn test_anthropic_to_gemini_and_back() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let anthropic_req = json!({
        "model": "claude-sonnet-4-6",
        "system": "You are a helpful coding assistant.",
        "max_tokens": 2048,
        "temperature": 0.2,
        "tools": [
            {
                "name": "bash",
                "description": "Execute a bash command",
                "input_schema": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false,
                },
            }
        ],
        "messages": [{"role": "user", "content": "List files in current directory"}],
    });

    let (model, gemini_payload) = anthropic_to_gemini_request(&anthropic_req);
    assert_eq!(model, "gemini-3.1-pro-preview");
    assert_eq!(
        gemini_payload["systemInstruction"]["parts"][0]["text"],
        "You are a helpful coding assistant."
    );
    assert_eq!(
        gemini_payload["contents"][0]["parts"][0]["text"],
        "List files in current directory"
    );
    assert!(gemini_payload["tools"][0]["functionDeclarations"][0]["parameters"]
        .get("additionalProperties")
        .is_none());

    let fake_gemini_resp = json!({
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "functionCall": {
                                "name": "bash",
                                "args": {"command": "ls -la"},
                            },
                            "thoughtSignature": "sig_test_123",
                        }
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 25, "candidatesTokenCount": 12},
    });

    let anthropic_resp = gemini_to_anthropic_response(&fake_gemini_resp, "claude-sonnet-4-6");
    assert_eq!(anthropic_resp["stop_reason"], "tool_use");
    assert_eq!(anthropic_resp["content"][0]["type"], "tool_use");
    assert_eq!(anthropic_resp["content"][0]["name"], "bash");
    assert_eq!(anthropic_resp["content"][0]["input"], json!({"command": "ls -la"}));
    assert_eq!(anthropic_resp["usage"]["input_tokens"], 25);
    assert_eq!(anthropic_resp["usage"]["output_tokens"], 12);
}

#[test]
fn test_anthropic_multi_turn_function_and_skill_calling() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SIGNATURE_STORE.clear();

    let gemini_turn1 = json!({
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "text": "Let me load the skill first.",
                            "thought": true,
                        },
                        {
                            "functionCall": {
                                "name": "Skill",
                                "args": {"skill": "omarchy"},
                            },
                            "thoughtSignature": "sig_skill_abc",
                        },
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 8},
    });
    let anthropic_turn1 = gemini_to_anthropic_response(&gemini_turn1, "claude-sonnet-4-6");
    let tool_use_block = anthropic_turn1["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap()
        .clone();
    let tool_id = tool_use_block["id"].as_str().unwrap().to_string();

    let anthropic_turn2 = json!({
        "model": "claude-sonnet-4-6",
        "tools": [
            {
                "name": "Skill",
                "description": "Invoke a skill",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "skill": {"type": "string"},
                        "args": {"type": "string"},
                    },
                    "required": ["skill"],
                },
            }
        ],
        "messages": [
            {"role": "user", "content": "Configure waybar using the omarchy skill"},
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": "Let me load the skill first.",
                        "signature": "sig_from_thinking_block",
                    },
                    {
                        "type": "tool_use",
                        "id": tool_id,
                        "name": "Skill",
                        "input": {"skill": "omarchy"},
                    },
                ],
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": tool_id,
                        "content": "Launching skill: omarchy",
                    }
                ],
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "<command-message>omarchy</command-message>\n# Omarchy Skill Instructions...",
                    }
                ],
            },
        ],
    });

    let (_model, gemini_req) = anthropic_to_gemini_request(&anthropic_turn2);
    let contents = gemini_req["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(contents[2]["role"], "user");

    let model_parts = contents[1]["parts"].as_array().unwrap();
    let fc_parts: Vec<&Value> = model_parts
        .iter()
        .filter(|p| p.get("functionCall").is_some())
        .collect();
    assert_eq!(fc_parts.len(), 1);
    assert_eq!(fc_parts[0]["functionCall"]["name"], "Skill");
    assert_eq!(fc_parts[0]["thoughtSignature"], "sig_from_thinking_block");

    let user_parts = contents[2]["parts"].as_array().unwrap();
    assert_eq!(user_parts.len(), 2);
    assert_eq!(user_parts[0]["functionResponse"]["name"], "Skill");
    assert!(user_parts[1]["text"]
        .as_str()
        .unwrap()
        .contains("Omarchy Skill Instructions"));
}

#[test]
fn test_anthropic_tool_search_tool_reference_and_missing_signature_fallback() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SIGNATURE_STORE.clear();

    let anthropic_req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Find tools for Slack"},
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_unseen_id",
                        "name": "ToolSearch",
                        "input": {"query": "slack"},
                    }
                ],
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_unseen_id",
                        "content": [
                            {"type": "tool_reference", "tool_name": "mcp__slack__send_message"},
                            {"type": "text", "text": "Found 1 tool."},
                        ],
                    }
                ],
            },
        ],
    });

    let (_model, gemini_req) = anthropic_to_gemini_request(&anthropic_req);
    let contents = gemini_req["contents"].as_array().unwrap();
    let model_fc_part = &contents[1]["parts"][0];
    assert_eq!(
        model_fc_part["thoughtSignature"],
        SKIP_THOUGHT_SIGNATURE_VALIDATOR
    );

    let user_fr_part = &contents[2]["parts"][0];
    assert_eq!(user_fr_part["functionResponse"]["name"], "ToolSearch");
    let res_str = user_fr_part["functionResponse"]["response"]["result"]
        .as_str()
        .unwrap();
    assert!(res_str.contains("mcp__slack__send_message"));
    assert!(res_str.contains("Found 1 tool."));

    let declared: Vec<&str> = gemini_req["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(declared.contains(&"ToolSearch"));
}

#[test]
fn test_anthropic_stream_function_call_preserves_signature() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SIGNATURE_STORE.clear();
    let mut state = AnthropicStreamState::new("claude-sonnet-4-6");
    let chunk = json!({
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "functionCall": {
                                "name": "Skill",
                                "args": {"skill": "omarchy"},
                            },
                            "thoughtSignature": "stream_sig_999",
                        }
                    ],
                },
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 15, "candidatesTokenCount": 5},
    });
    let events = state.process_gemini_chunk(&chunk);
    let end_events = state.finish();
    let all_sse = format!("{}{}", events.join(""), end_events.join(""));
    assert!(all_sse.contains("\"type\":\"tool_use\""));
    assert!(all_sse.contains("\"stop_reason\":\"tool_use\""));
    assert_eq!(
        SIGNATURE_STORE.get(None).as_deref(),
        Some("stream_sig_999")
    );
}

#[test]
fn test_openai_chat_to_gemini_and_back_with_tool_calls() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SIGNATURE_STORE.clear();
    let chat_req = json!({
        "model": "gpt-5-codex",
        "messages": [
            {"role": "system", "content": "You are Codex."},
            {"role": "user", "content": "Run cargo test"},
            {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"cargo test\"}"},
                    }
                ],
            },
            {
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "test result: ok",
            },
        ],
        "max_completion_tokens": 1024,
    });

    let (model, gemini_payload) = openai_chat_to_gemini_request(&chat_req);
    assert_eq!(model, "gemini-3.1-pro-preview");
    assert_eq!(
        gemini_payload["contents"][1]["parts"][0]["functionCall"]["name"],
        "shell"
    );
    assert_eq!(
        gemini_payload["contents"][1]["parts"][0]["thoughtSignature"],
        SKIP_THOUGHT_SIGNATURE_VALIDATOR
    );
    assert_eq!(
        gemini_payload["contents"][2]["parts"][0]["functionResponse"]["name"],
        "shell"
    );

    let fake_gemini_resp = json!({
        "candidates": [
            {
                "content": {"role": "model", "parts": [{"text": "All tests passed!"}]},
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
    });
    let chat_resp = gemini_to_openai_chat_response(&fake_gemini_resp, "gpt-5-codex");
    assert_eq!(
        chat_resp["choices"][0]["message"]["content"],
        "All tests passed!"
    );
    assert_eq!(chat_resp["usage"]["total_tokens"], 15);
}

#[test]
fn test_openai_responses_to_gemini_and_back_with_function_call() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SIGNATURE_STORE.clear();
    let resp_req = json!({
        "model": "gpt-5-codex",
        "instructions": "You are OpenAI Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Check git status"}]},
            {
                "type": "function_call",
                "call_id": "call_git_1",
                "name": "shell",
                "arguments": "{\"command\":[\"git\",\"status\"]}",
            },
            {
                "type": "function_call_output",
                "call_id": "call_git_1",
                "output": "On branch main\nnothing to commit",
            },
        ],
    });
    let (model, gemini_payload) = openai_responses_to_gemini_request(&resp_req);
    assert_eq!(model, "gemini-3.1-pro-preview");
    assert_eq!(
        gemini_payload["systemInstruction"]["parts"][0]["text"],
        "You are OpenAI Codex."
    );
    assert_eq!(
        gemini_payload["contents"][1]["parts"][0]["functionCall"]["name"],
        "shell"
    );
    assert_eq!(
        gemini_payload["contents"][1]["parts"][0]["thoughtSignature"],
        SKIP_THOUGHT_SIGNATURE_VALIDATOR
    );
    assert_eq!(
        gemini_payload["contents"][2]["parts"][0]["functionResponse"]["name"],
        "shell"
    );

    let fake_gemini = json!({
        "candidates": [
            {
                "content": {"role": "model", "parts": [{"text": "Working tree is clean."}]},
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {"promptTokenCount": 14, "candidatesTokenCount": 7},
    });
    let codex_resp = gemini_to_openai_responses(&fake_gemini, "gpt-5-codex");
    assert_eq!(codex_resp["object"], "response");
    assert_eq!(
        codex_resp["output"][0]["content"][0]["text"],
        "Working tree is clean."
    );
}

#[tokio::test]
async fn test_server_endpoints_health_models_config_count_tokens() {
    let server = RouterServer::new(Some("test-proj".to_string()), Some("global".to_string()));

    // 1. /health
    match server.handle_request("GET", "/health", b"").await {
        RouterResponse::Json(code, data) => {
            assert_eq!(code, 200);
            assert_eq!(data["status"], "ok");
            assert_eq!(data["auth_mode"], "adc_only");
            assert!(data.get("metrics").is_some());
        }
        _ => panic!("Expected JSON response"),
    }

    // 2. /v1/models
    match server.handle_request("GET", "/v1/models", b"").await {
        RouterResponse::Json(code, data) => {
            assert_eq!(code, 200);
            let ids: Vec<&str> = data["data"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m["id"].as_str())
                .collect();
            assert!(ids.contains(&"gemini-4-argon"));
            assert!(ids.contains(&"gemini-3.8-flash"));
            assert!(ids.contains(&"gemini-3.1-pro-preview"));
            assert!(ids.contains(&"gemini-3-flash-preview"));
        }
        _ => panic!("Expected JSON response"),
    }

    // 3. /config
    let cfg_body = serde_json::to_vec(&json!({
        "default_model": "gemini-3-flash-preview",
        "project_id": "custom-proj-123"
    }))
    .unwrap();
    match server.handle_request("POST", "/config", &cfg_body).await {
        RouterResponse::Json(code, data) => {
            assert_eq!(code, 200);
            assert_eq!(data["status"], "updated");
            assert_eq!(data["default_model"], "gemini-3-flash-preview");
            assert_eq!(data["project_id"], "custom-proj-123");
        }
        _ => panic!("Expected JSON response"),
    }

    // 4. /v1/messages/count_tokens
    let ct_body = serde_json::to_vec(&json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "Hello world!"}]
    }))
    .unwrap();
    match server
        .handle_request("POST", "/v1/messages/count_tokens", &ct_body)
        .await
    {
        RouterResponse::Json(code, data) => {
            assert_eq!(code, 200);
            assert!(data["input_tokens"].as_u64().unwrap() >= 1);
        }
        _ => panic!("Expected JSON response"),
    }
}

#[tokio::test]
async fn test_e2e_mock_vertex_upstream_function_and_skill_calling() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();

    // Spawn a mock Vertex AI `:generateContent` server that inspects incoming Gemini requests
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&buf[..n]);
            assert!(req_str.contains(":generateContent"));
            assert!(req_str.contains("\"functionCall\""));
            assert!(req_str.contains("\"functionResponse\""));
            assert!(req_str.contains("\"thoughtSignature\""));

            let mock_gemini_body = json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            {
                                "functionCall": {
                                    "name": "bash",
                                    "args": {"command": "echo skill-executed"}
                                },
                                "thoughtSignature": "e2e_sig_verified"
                            }
                        ]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 42,
                    "candidatesTokenCount": 18
                }
            })
            .to_string();

            let http_resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                mock_gemini_body.len(),
                mock_gemini_body
            );
            let _ = sock.write_all(http_resp.as_bytes()).await;
        }
    });

    std::env::set_var("GEMINI_UPSTREAM_URL", format!("http://{}", mock_addr));
    let server = RouterServer::new(Some("test-proj".to_string()), Some("global".to_string()));

    let req_body = serde_json::to_vec(&json!({
        "model": "claude-sonnet-4-6",
        "tools": [
            {
                "name": "Skill",
                "description": "Execute a skill",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"skill": {"type": "string"}},
                    "required": ["skill"],
                    "additionalProperties": false
                }
            },
            {
                "name": "bash",
                "description": "Run bash",
                "input_schema": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }
            }
        ],
        "messages": [
            {"role": "user", "content": "Run the omarchy skill"},
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_skill_1", "name": "Skill", "input": {"skill": "omarchy"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_skill_1", "content": "Launching skill: omarchy"}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "<command-message>omarchy</command-message>\nNow run echo skill-executed"}
                ]
            }
        ]
    }))
    .unwrap();

    match server.handle_request("POST", "/v1/messages", &req_body).await {
        RouterResponse::Json(code, data) => {
            std::env::remove_var("GEMINI_UPSTREAM_URL");
            assert_eq!(code, 200);
            assert_eq!(data["stop_reason"], "tool_use");
            assert_eq!(data["content"][0]["type"], "tool_use");
            assert_eq!(data["content"][0]["name"], "bash");
            assert_eq!(
                data["content"][0]["input"],
                json!({"command": "echo skill-executed"})
            );
        }
        _ => {
            std::env::remove_var("GEMINI_UPSTREAM_URL");
            panic!("Expected JSON response");
        }
    }
}
