use regex::Regex;
use serde_json::{json, Map, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: u64 = 1_048_576; // 1 MiB
const VALID_TARGETS: &[&str] = &["claude", "codex", "both"];
const VALID_MODELS: &[&str] = &["gemini-3.8-flash", "gemini-4-argon"];
const VALID_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

const CODEX_BLOCK_BEGIN: &str = "# BEGIN OMARCHY_GEMINI_ADC_ROUTER";
const CODEX_BLOCK_END: &str = "# END OMARCHY_GEMINI_ADC_ROUTER";

const CLAUDE_MANAGED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "DISABLE_PROMPT_CACHING",
];

struct Paths {
    config_dir: PathBuf,
    state_file: PathBuf,
    env_file: PathBuf,
    claude_settings: PathBuf,
    codex_config: PathBuf,
    project_dir: PathBuf,
}

fn home_dir() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn get_paths() -> Paths {
    let home = home_dir();
    let config_dir = env::var("OMARCHY_GEMINI_ROUTER_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".config/omarchy/gemini-router"));
    let claude_settings = env::var("CLAUDE_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".claude/settings.json"));
    let codex_config = env::var("CODEX_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".codex/config.toml"));

    let mut default_project_dir = home.join("Projects/omarchy-claude-codex-router");
    if !default_project_dir.join("Cargo.toml").is_file() {
        let legacy_dir = home.join("Projects/claude-gemini-adc-router");
        if legacy_dir.join("Cargo.toml").is_file() {
            default_project_dir = legacy_dir;
        }
    }
    if let Ok(exe_path) = env::current_exe() {
        if let Some(p3) = exe_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            if p3.join("Cargo.toml").is_file() {
                default_project_dir = p3.to_path_buf();
            }
        }
    }
    let project_dir = env::var("ROUTER_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or(default_project_dir);

    Paths {
        state_file: config_dir.join("state.json"),
        env_file: config_dir.join("router.env"),
        config_dir,
        claude_settings,
        codex_config,
        project_dir,
    }
}

fn router_port() -> u16 {
    env::var("ROUTER_PORT")
        .ok()
        .and_then(|raw| raw.parse::<u16>().ok())
        .filter(|&p| p >= 1)
        .unwrap_or(4000)
}

fn router_url() -> String {
    env::var("ROUTER_URL")
        .unwrap_or_else(|_| format!("http://127.0.0.1:{}", router_port()))
        .trim_end_matches('/')
        .to_string()
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn read_file_safely(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!("Refusing to read symlink: {}", path.display()));
            }
            if meta.uid() != current_uid() {
                return Err(format!(
                    "Refusing to read file not owned by current user: {}",
                    path.display()
                ));
            }
            if meta.len() > MAX_OUTPUT_BYTES {
                return Err(format!(
                    "File exceeds size limit ({} bytes): {}",
                    meta.len(),
                    path.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Failed to stat {}: {}", path.display(), e)),
    }

    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            if e.raw_os_error() == Some(libc::ELOOP) {
                return Err(format!("Refusing to read symlink: {}", path.display()));
            }
            return Err(format!("Failed to open {}: {}", path.display(), e));
        }
    };

    let meta = file
        .metadata()
        .map_err(|e| format!("fstat failed on {}: {}", path.display(), e))?;
    if meta.uid() != current_uid() {
        return Err(format!(
            "Refusing to read file not owned by current user: {}",
            path.display()
        ));
    }
    if meta.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "File exceeds size limit ({} bytes): {}",
            meta.len(),
            path.display()
        ));
    }

    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn write_file_safely(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Invalid path without parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;

    let parent_meta = fs::symlink_metadata(parent)
        .map_err(|e| format!("Failed to lstat parent {}: {}", parent.display(), e))?;
    if parent_meta.file_type().is_symlink() {
        return Err(format!(
            "Parent directory {} must not be a symlink",
            parent.display()
        ));
    }
    if parent_meta.uid() != current_uid() {
        return Err(format!(
            "Parent directory {} is not owned by current user",
            parent.display()
        ));
    }

    if let Ok(target_meta) = fs::symlink_metadata(path) {
        if target_meta.file_type().is_symlink() {
            return Err(format!("Refusing to write to symlink: {}", path.display()));
        }
        if target_meta.uid() != current_uid() {
            return Err(format!(
                "Target file {} is not owned by current user",
                path.display()
            ));
        }
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp_name = format!(
        ".{}.tmp.{}.{}",
        file_name,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let tmp_path = parent.join(tmp_name);

    let res = (|| -> Result<(), String> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to create temp file {}: {}", tmp_path.display(), e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write temp file {}: {}", tmp_path.display(), e))?;
        f.sync_all()
            .map_err(|e| format!("Failed to fsync temp file {}: {}", tmp_path.display(), e))?;
        drop(f);
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod failed on {}: {}", tmp_path.display(), e))?;
        fs::rename(&tmp_path, path)
            .map_err(|e| format!("Atomic rename to {} failed: {}", path.display(), e))?;
        Ok(())
    })();

    if res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    res
}

fn run_cmd(args: &[&str], timeout: Duration) -> (i32, String, String) {
    if args.is_empty() {
        return (1, String::new(), "Empty command".to_string());
    }
    let home = home_dir();
    let current_path = env::var("PATH").unwrap_or_default();
    let new_path = format!(
        "{}/.cargo/bin:{}/.local/share/mise/shims:{}/.local/bin:{}/google-cloud-sdk/bin:/usr/local/bin:/usr/bin:/bin:{}",
        home.display(),
        home.display(),
        home.display(),
        home.display(),
        current_path
    );

    let mut child = match Command::new(args[0])
        .args(&args[1..])
        .env("PATH", new_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (127, String::new(), format!("Executable not found: {}", args[0]));
        }
        Err(e) => return (1, String::new(), e.to_string()),
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return match child.wait_with_output() {
                    Ok(out) => {
                        let code = out.status.code().unwrap_or(1);
                        let stdout_s = String::from_utf8_lossy(
                            &out.stdout[..out.stdout.len().min(MAX_OUTPUT_BYTES as usize)],
                        )
                        .into_owned();
                        let stderr_s = String::from_utf8_lossy(
                            &out.stderr[..out.stderr.len().min(MAX_OUTPUT_BYTES as usize)],
                        )
                        .into_owned();
                        (code, stdout_s, stderr_s)
                    }
                    Err(e) => (1, String::new(), e.to_string()),
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (
                        124,
                        String::new(),
                        format!("Command timed out after {:.1}s", timeout.as_secs_f64()),
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return (1, String::new(), e.to_string()),
        }
    }
}

fn load_state() -> Result<Map<String, Value>, String> {
    let paths = get_paths();
    let raw = read_file_safely(&paths.state_file)?;
    let mut default_state = Map::new();
    default_state.insert(
        "primary_model".to_string(),
        Value::String("gemini-3.8-flash".to_string()),
    );
    default_state.insert(
        "reasoning_effort".to_string(),
        Value::String("medium".to_string()),
    );
    default_state.insert("claude_backup_env".to_string(), Value::Null);
    default_state.insert("codex_backup".to_string(), Value::Null);

    if let Some(text) = raw {
        if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&text) {
            for (k, v) in parsed {
                default_state.insert(k, v);
            }
        }
    }
    Ok(default_state)
}

fn save_state(state: &Map<String, Value>) -> Result<(), String> {
    let paths = get_paths();
    let json_text = serde_json::to_string_pretty(&Value::Object(state.clone()))
        .map_err(|e| e.to_string())?
        + "\n";
    write_file_safely(&paths.state_file, &json_text, 0o600)?;

    let primary_model = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash");
    let effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium");
    let env_content = format!(
        "GEMINI_PRIMARY_MODEL={}\nGEMINI_REASONING_EFFORT={}\nROUTER_PORT={}\n",
        primary_model,
        effort,
        router_port()
    );
    write_file_safely(&paths.env_file, &env_content, 0o600)
}

fn normalize_model(model: Option<&str>) -> Result<String, String> {
    let Some(m) = model else {
        return Ok("gemini-3.8-flash".to_string());
    };
    let lowered = m.trim().to_lowercase();
    if lowered.is_empty() {
        return Ok("gemini-3.8-flash".to_string());
    }
    // Strict validation against path traversal or injection characters
    if lowered.contains('/') || lowered.contains('\\') || lowered.contains("..") || lowered.contains(';') {
        return Err(format!(
            "Invalid model '{}'. Allowed: {}",
            m,
            VALID_MODELS.join(", ")
        ));
    }
    if lowered == "gemini-4.0"
        || lowered == "gemini-4.0-pro"
        || lowered == "gemini-4-argon"
        || lowered == "4.0"
        || lowered == "argon"
        || lowered == "pro"
    {
        return Ok("gemini-4-argon".to_string());
    }
    if lowered == "gemini-3.8"
        || lowered == "gemini-3.8-flash"
        || lowered == "3.8"
        || lowered == "flash"
    {
        return Ok("gemini-3.8-flash".to_string());
    }
    if VALID_MODELS.contains(&lowered.as_str()) {
        return Ok(lowered);
    }
    Err(format!(
        "Invalid model '{}'. Allowed: {}",
        m,
        VALID_MODELS.join(", ")
    ))
}

fn normalize_effort(effort: Option<&str>) -> Result<String, String> {
    let Some(e) = effort else {
        return Ok("medium".to_string());
    };
    let lowered = e.trim().to_lowercase();
    if lowered.is_empty() {
        return Ok("medium".to_string());
    }
    if VALID_EFFORTS.contains(&lowered.as_str()) {
        return Ok(lowered);
    }
    Err(format!(
        "Invalid reasoning effort '{}'. Allowed: {}",
        e,
        VALID_EFFORTS.join(", ")
    ))
}

fn is_claude_routed() -> Result<bool, String> {
    let paths = get_paths();
    let Some(raw) = read_file_safely(&paths.claude_settings)? else {
        return Ok(false);
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&raw) else {
        return Ok(false);
    };
    let Some(Value::Object(env_obj)) = data.get("env") else {
        return Ok(false);
    };
    let base_url = env_obj
        .get("ANTHROPIC_BASE_URL")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches('/');
    let auth_token = env_obj
        .get("ANTHROPIC_AUTH_TOKEN")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(base_url == router_url() && auth_token == "adc-keyless")
}

fn configure_claude_routing(
    enable: bool,
    primary_model: &str,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let paths = get_paths();
    let raw = read_file_safely(&paths.claude_settings)?;
    let mut settings: Map<String, Value> = Map::new();
    if let Some(text) = raw {
        if !text.trim().is_empty() {
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(m)) => settings = m,
                Ok(_) => {}
                Err(e) => {
                    return Err(format!(
                        "Malformed JSON in {}: {}",
                        paths.claude_settings.display(),
                        e
                    ))
                }
            }
        }
    }

    let current_env = settings
        .get("env")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let already_routed = current_env
        .get("ANTHROPIC_BASE_URL")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches('/')
        == router_url()
        && current_env
            .get("ANTHROPIC_AUTH_TOKEN")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            == "adc-keyless";

    if enable {
        if !already_routed
            && state
                .get("claude_backup_env")
                .map(|v| v.is_null())
                .unwrap_or(true)
        {
            state.insert(
                "claude_backup_env".to_string(),
                Value::Object(current_env.clone()),
            );
        }

        let mut base_env = state
            .get("claude_backup_env")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or(current_env);

        for conflict_key in [
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
        ] {
            base_env.remove(conflict_key);
        }

        base_env.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            Value::String(router_url()),
        );
        base_env.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            Value::String("adc-keyless".to_string()),
        );
        base_env.insert(
            "ANTHROPIC_MODEL".to_string(),
            Value::String(primary_model.to_string()),
        );
        base_env.insert(
            "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
            Value::String("claude-haiku-4-5".to_string()),
        );
        base_env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            Value::String("claude-haiku-4-5".to_string()),
        );
        base_env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
            Value::String(primary_model.to_string()),
        );
        base_env.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
            Value::String(primary_model.to_string()),
        );
        base_env.insert(
            "DISABLE_PROMPT_CACHING".to_string(),
            Value::String("1".to_string()),
        );

        settings.insert("env".to_string(), Value::Object(base_env));
    } else {
        if let Some(Value::Object(backup_env)) = state.get("claude_backup_env").cloned() {
            if !backup_env.is_empty() {
                settings.insert("env".to_string(), Value::Object(backup_env));
            } else {
                settings.remove("env");
            }
            state.insert("claude_backup_env".to_string(), Value::Null);
        } else {
            let mut updated_env = current_env;
            for k in CLAUDE_MANAGED_ENV_KEYS {
                updated_env.remove(*k);
            }
            if !updated_env.is_empty() {
                settings.insert("env".to_string(), Value::Object(updated_env));
            } else {
                settings.remove("env");
            }
        }
    }

    let out = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|e| e.to_string())?
        + "\n";
    write_file_safely(&paths.claude_settings, &out, 0o600)
}

fn extract_top_level_toml_key(lines: &[String], key: &str) -> Option<String> {
    let pattern = Regex::new(&format!(r"^\s*{}\s*=\s*(.+?)\s*$", regex::escape(key))).ok()?;
    for line in lines {
        let stripped = line.trim();
        if stripped.starts_with('[') && !stripped.starts_with("[[") {
            break;
        }
        if pattern.is_match(line) {
            return Some(line.trim_end_matches('\n').to_string());
        }
    }
    None
}

fn remove_top_level_toml_keys(lines: &[String], keys: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_top_level = true;
    let patterns: Vec<Regex> = keys
        .iter()
        .filter_map(|k| Regex::new(&format!(r"^\s*{}\s*=", regex::escape(k))).ok())
        .collect();
    for line in lines {
        let stripped = line.trim();
        if stripped.starts_with('[') {
            in_top_level = false;
        }
        if in_top_level && patterns.iter().any(|p| p.is_match(line)) {
            continue;
        }
        result.push(line.clone());
    }
    result
}

fn strip_codex_managed_block(text: &str) -> String {
    let pattern = Regex::new(&format!(
        r"(?s)\n?{}.*?{}\n?",
        regex::escape(CODEX_BLOCK_BEGIN),
        regex::escape(CODEX_BLOCK_END)
    ))
    .unwrap();
    pattern.replace_all(text, "\n").into_owned()
}

fn is_codex_routed() -> Result<bool, String> {
    let paths = get_paths();
    let Some(raw) = read_file_safely(&paths.codex_config)? else {
        return Ok(false);
    };
    let re = Regex::new(r#"(?m)^\s*model_provider\s*=\s*"gemini_adc""#).unwrap();
    let has_provider = re.is_match(&raw);
    let has_section = raw.contains("[model_providers.gemini_adc]");
    Ok(has_provider && has_section)
}

fn configure_codex_routing(
    enable: bool,
    primary_model: &str,
    reasoning_effort: &str,
    state: &mut Map<String, Value>,
) -> Result<(), String> {
    let paths = get_paths();
    let raw = read_file_safely(&paths.codex_config)?.unwrap_or_default();
    let already_routed = is_codex_routed()?;

    let cleaned = strip_codex_managed_block(&raw);
    let mut lines: Vec<String> = cleaned.lines().map(|s| s.to_string()).collect();

    if enable {
        if !already_routed
            && state
                .get("codex_backup")
                .map(|v| v.is_null())
                .unwrap_or(true)
        {
            let mut backup = Map::new();
            backup.insert(
                "model_provider".to_string(),
                extract_top_level_toml_key(&lines, "model_provider")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            backup.insert(
                "model".to_string(),
                extract_top_level_toml_key(&lines, "model")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            backup.insert(
                "model_reasoning_effort".to_string(),
                extract_top_level_toml_key(&lines, "model_reasoning_effort")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            state.insert("codex_backup".to_string(), Value::Object(backup));
        }

        lines = remove_top_level_toml_keys(
            &lines,
            &["model_provider", "model", "model_reasoning_effort"],
        );
        while !lines.is_empty() && lines[0].trim().is_empty() {
            lines.remove(0);
        }

        let mut body_lines = vec![
            r#"model_provider = "gemini_adc""#.to_string(),
            format!(r#"model = "{}""#, primary_model),
            format!(r#"model_reasoning_effort = "{}""#, reasoning_effort),
        ];
        if !lines.is_empty() {
            body_lines.push(String::new());
            body_lines.extend(lines);
        }
        let body = body_lines.join("\n").trim_end().to_string();
        let managed_block = [
            CODEX_BLOCK_BEGIN.to_string(),
            "[model_providers.gemini_adc]".to_string(),
            r#"name = "Gemini ADC Router (Vertex AI)""#.to_string(),
            format!(r#"base_url = "{}/v1""#, router_url()),
            r#"wire_api = "responses""#.to_string(),
            CODEX_BLOCK_END.to_string(),
        ]
        .join("\n");

        let full_text = format!("{}\n\n{}\n", body, managed_block);
        write_file_safely(&paths.codex_config, &full_text, 0o600)
    } else {
        lines = remove_top_level_toml_keys(
            &lines,
            &["model_provider", "model", "model_reasoning_effort"],
        );
        let mut restored_top = Vec::new();
        if let Some(Value::Object(backup)) = state.get("codex_backup").cloned() {
            for k in ["model_provider", "model", "model_reasoning_effort"] {
                if let Some(Value::String(val)) = backup.get(k) {
                    if !val.trim().is_empty() {
                        restored_top.push(val.clone());
                    }
                }
            }
            state.insert("codex_backup".to_string(), Value::Null);
        }

        while !lines.is_empty() && lines[0].trim().is_empty() {
            lines.remove(0);
        }

        let mut combined = Vec::new();
        combined.extend(restored_top.clone());
        if !restored_top.is_empty() && !lines.is_empty() {
            combined.push(String::new());
        }
        combined.extend(lines);
        let mut full_text = combined.join("\n").trim().to_string();
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        write_file_safely(&paths.codex_config, &full_text, 0o600)
    }
}

fn local_http_request(
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Option<(u16, String)> {
    let url = router_url();
    let stripped = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(&url);
    let host_port = stripped.split('/').next().unwrap_or("127.0.0.1:4000");
    let addr: std::net::SocketAddr = host_port.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let payload = body.unwrap_or("");
    let req = if method == "POST" {
        format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            path,
            host_port,
            payload.len(),
            payload
        )
    } else {
        format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            path, host_port
        )
    };
    stream.write_all(req.as_bytes()).ok()?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp).ok()?;
    let (head, body_part) = resp.split_once("\r\n\r\n")?;
    let status_line = head.lines().next()?;
    let status_code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    Some((status_code, body_part.to_string()))
}

fn fetch_router_health(timeout: Duration) -> Option<Map<String, Value>> {
    let (status, body) = local_http_request("GET", "/health", None, timeout)?;
    if status == 200 {
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&body) {
            return Some(m);
        }
    }
    None
}

fn push_router_config(primary_model: &str, reasoning_effort: &str, hot_swap: bool) -> bool {
    let payload = json!({
        "primary_model": primary_model,
        "reasoning_effort": reasoning_effort,
        "hot_swap": hot_swap,
    })
    .to_string();
    matches!(
        local_http_request("POST", "/config", Some(&payload), Duration::from_secs(2)),
        Some((200, _))
    )
}

fn is_systemd_service_enabled() -> bool {
    if env::var("OMARCHY_GEMINI_ROUTER_MOCK_DAEMON").as_deref() == Ok("1") {
        return true;
    }
    let (rc, out, _) = run_cmd(
        &[
            "systemctl",
            "--user",
            "is-enabled",
            "claude-gemini-router.service",
        ],
        Duration::from_secs(3),
    );
    rc == 0 && out.trim().contains("enabled")
}

fn is_systemd_service_active() -> bool {
    if env::var("OMARCHY_GEMINI_ROUTER_MOCK_DAEMON").as_deref() == Ok("1") {
        return false;
    }
    let (rc, out, _) = run_cmd(
        &[
            "systemctl",
            "--user",
            "is-active",
            "claude-gemini-router.service",
        ],
        Duration::from_secs(3),
    );
    rc == 0 && out.trim() == "active"
}

fn start_router_daemon(state: &Map<String, Value>) -> (bool, String) {
    if env::var("OMARCHY_GEMINI_ROUTER_MOCK_DAEMON").as_deref() == Ok("1") {
        return (true, "mock-daemon-started".to_string());
    }

    let primary_model = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash");
    let reasoning_effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium");

    if let Some(health) = fetch_router_health(Duration::from_secs(1)) {
        if health.get("status").and_then(|v| v.as_str()) == Some("ok") {
            push_router_config(primary_model, reasoning_effort, true);
            return (true, "already-running".to_string());
        }
    }

    let (rc, _, _) = run_cmd(
        &[
            "systemctl",
            "--user",
            "start",
            "claude-gemini-router.service",
        ],
        Duration::from_secs(5),
    );
    if rc == 0 {
        for _ in 0..20 {
            if fetch_router_health(Duration::from_millis(500)).is_some() {
                push_router_config(primary_model, reasoning_effort, true);
                return (true, "systemd".to_string());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    let paths = get_paths();
    let home = home_dir();
    let candidates = [
        home.join(".local/bin/claude-gemini-router"),
        paths.project_dir.join("target/release/claude-gemini-router"),
    ];
    let Some(router_bin) = candidates.into_iter().find(|p| p.is_file()) else {
        return (
            false,
            "claude-gemini-router binary not found".to_string(),
        );
    };

    let _ = fs::create_dir_all(&paths.config_dir);
    let log_path = paths.config_dir.join("router.log");
    let Ok(log_file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log_path)
    else {
        return (false, "Failed to open router.log".to_string());
    };
    let Ok(log_err) = log_file.try_clone() else {
        return (false, "Failed to clone log fd".to_string());
    };

    let port_s = router_port().to_string();
    let spawn_res = Command::new(&router_bin)
        .args(["--host", "127.0.0.1", "--port", &port_s])
        .current_dir(&paths.project_dir)
        .env("GEMINI_PRIMARY_MODEL", primary_model)
        .env("GEMINI_REASONING_EFFORT", reasoning_effort)
        .env("ROUTER_PORT", &port_s)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .spawn();

    if let Err(e) = spawn_res {
        return (false, format!("Failed to spawn router: {}", e));
    }

    for _ in 0..25 {
        if fetch_router_health(Duration::from_millis(500)).is_some() {
            push_router_config(primary_model, reasoning_effort, true);
            return (true, "background-process".to_string());
        }
        thread::sleep(Duration::from_millis(200));
    }

    (
        false,
        "Router failed to respond on /health within timeout".to_string(),
    )
}

fn stop_router_daemon() -> (bool, String) {
    if env::var("OMARCHY_GEMINI_ROUTER_MOCK_DAEMON").as_deref() == Ok("1") {
        return (true, "mock-daemon-stopped".to_string());
    }

    let _ = run_cmd(
        &[
            "systemctl",
            "--user",
            "stop",
            "claude-gemini-router.service",
        ],
        Duration::from_secs(5),
    );
    let uid_s = current_uid().to_string();
    let (rc, out, _) = run_cmd(
        &["pgrep", "-u", &uid_s, "-f", "claude-gemini-router"],
        Duration::from_secs(3),
    );
    if rc == 0 && !out.trim().is_empty() {
        let my_pid = std::process::id() as i32;
        for line in out.lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                if pid != my_pid {
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
    }
    for _ in 0..10 {
        if fetch_router_health(Duration::from_millis(400)).is_none() {
            return (true, "stopped".to_string());
        }
        thread::sleep(Duration::from_millis(200));
    }
    (
        false,
        "Router is still responding after stop attempt".to_string(),
    )
}

fn probe_adc_status_offline() -> Value {
    let home = home_dir();
    let adc_file = env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".config/gcloud/application_default_credentials.json"));
    let has_adc_file = adc_file.is_file();

    let mut project_id = env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| env::var("ANTHROPIC_VERTEX_PROJECT_ID"))
        .unwrap_or_default();

    if project_id.is_empty() {
        let gcloud_cfg = home.join(".config/gcloud/configurations/config_default");
        if gcloud_cfg.is_file() {
            if let Ok(Some(raw_cfg)) = read_file_safely(&gcloud_cfg) {
                if let Ok(re) = Regex::new(r"(?m)^\s*project\s*=\s*(.+?)\s*$") {
                    if let Some(caps) = re.captures(&raw_cfg) {
                        if let Some(m) = caps.get(1) {
                            project_id = m.as_str().trim().to_string();
                        }
                    }
                }
            }
        }
    }

    json!({
        "adc_mode": true,
        "api_key_used": false,
        "credentials_type": if has_adc_file { Value::String("UserCredentials".to_string()) } else { Value::Null },
        "project_id": if project_id.is_empty() { "unknown".to_string() } else { project_id },
        "location": env::var("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|_| "global".to_string()),
        "token_valid": has_adc_file,
        "error": if has_adc_file { Value::Null } else { Value::String("ADC credentials file not found".to_string()) },
    })
}

fn which_in_path(binary: &str) -> bool {
    let home = home_dir();
    let current_path = env::var("PATH").unwrap_or_default();
    let search_path = format!(
        "{}/.local/share/mise/shims:{}/.local/bin:{}",
        home.display(),
        home.display(),
        current_path
    );
    for dir in search_path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(binary);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                return true;
            }
        }
    }
    false
}

fn build_status_dict() -> Result<Map<String, Value>, String> {
    let state = load_state()?;
    let health = fetch_router_health(Duration::from_millis(1500));
    let mut router_running = health
        .as_ref()
        .and_then(|h| h.get("status"))
        .and_then(|v| v.as_str())
        == Some("ok");

    if env::var("OMARCHY_GEMINI_ROUTER_MOCK_DAEMON").as_deref() == Ok("1") {
        router_running = state
            .get("mock_router_running")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
    } else if !router_running {
        router_running = is_systemd_service_active();
    }

    let mut default_stats = serde_json::from_value::<Map<String, Value>>(json!({
        "total_requests": 0,
        "claude_requests": 0,
        "codex_requests": 0,
        "chat_requests": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "reasoning_tokens": 0,
        "cached_tokens": 0,
        "total_tokens": 0,
        "fallback_count": 0,
        "error_count": 0,
        "last_client": Value::Null,
        "last_requested_model": Value::Null,
        "last_vertex_model": Value::Null,
        "last_latency_ms": Value::Null,
        "last_request_at": Value::Null,
    }))
    .unwrap();

    let state_primary = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash")
        .to_string();
    let state_effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();

    let (adc_info, primary_model, reasoning_effort, hot_swap_active, stats_info) =
        if let Some(ref h) = health {
            if let Some(adc_obj) = h.get("adc").filter(|v| v.is_object()) {
                let pm = h
                    .get("primary_model")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&state_primary)
                    .to_string();
                let re = h
                    .get("reasoning_effort")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&state_effort)
                    .to_string();
                let hs = h
                    .get("hot_swap_active")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(Value::Object(raw_stats)) = h.get("stats") {
                    for (k, v) in raw_stats {
                        default_stats.insert(k.clone(), v.clone());
                    }
                }
                (adc_obj.clone(), pm, re, hs, Value::Object(default_stats))
            } else {
                (
                    probe_adc_status_offline(),
                    state_primary,
                    state_effort,
                    false,
                    Value::Object(default_stats),
                )
            }
        } else {
            (
                probe_adc_status_offline(),
                state_primary,
                state_effort,
                false,
                Value::Object(default_stats),
            )
        };

    let claude_on = is_claude_routed()?;
    let codex_on = is_codex_routed()?;

    let is_g4 = primary_model.contains('4');
    let short_model = if is_g4 { "Gemini 4.0" } else { "Gemini 3.8" };
    let (status_label, badge_text) = if claude_on && codex_on {
        (
            format!("Claude + Codex -> {}", short_model),
            if is_g4 { "C+X -> G4" } else { "C+X -> G3.8" }.to_string(),
        )
    } else if claude_on {
        (
            format!("Claude -> {}", short_model),
            if is_g4 {
                "Claude -> G4"
            } else {
                "Claude -> G3.8"
            }
            .to_string(),
        )
    } else if codex_on {
        (
            format!("Codex -> {}", short_model),
            if is_g4 {
                "Codex -> G4"
            } else {
                "Codex -> G3.8"
            }
            .to_string(),
        )
    } else {
        ("Direct (Routing Off)".to_string(), "ADC Off".to_string())
    };

    let val = json!({
        "ok": true,
        "router_running": router_running,
        "service_enabled": is_systemd_service_enabled(),
        "router_url": router_url(),
        "router_port": router_port(),
        "claude_enabled": claude_on,
        "codex_enabled": codex_on,
        "any_enabled": claude_on || codex_on,
        "both_enabled": claude_on && codex_on,
        "claude_installed": which_in_path("claude"),
        "codex_installed": which_in_path("codex"),
        "primary_model": primary_model,
        "reasoning_effort": reasoning_effort,
        "hot_swap_active": hot_swap_active,
        "status_label": status_label,
        "badge_text": badge_text,
        "adc": adc_info,
        "stats": stats_info,
    });
    Ok(val.as_object().unwrap().clone())
}

fn cmd_enable(
    target: &str,
    model_arg: Option<&str>,
    effort_arg: Option<&str>,
) -> Result<Map<String, Value>, String> {
    if !VALID_TARGETS.contains(&target) {
        return Err(format!(
            "Invalid target '{}'. Must be one of {:?}",
            target, VALID_TARGETS
        ));
    }

    let mut state = load_state()?;
    if let Some(m) = model_arg {
        state.insert(
            "primary_model".to_string(),
            Value::String(normalize_model(Some(m))?),
        );
    }
    if let Some(e) = effort_arg {
        state.insert(
            "reasoning_effort".to_string(),
            Value::String(normalize_effort(Some(e))?),
        );
    }

    let primary_model = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash")
        .to_string();
    let reasoning_effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();

    if target == "claude" || target == "both" {
        configure_claude_routing(true, &primary_model, &mut state)?;
    }
    if target == "codex" || target == "both" {
        configure_codex_routing(true, &primary_model, &reasoning_effort, &mut state)?;
    }

    state.insert("mock_router_running".to_string(), Value::Bool(true));
    save_state(&state)?;

    let (started, mode) = start_router_daemon(&state);
    let mut res = build_status_dict()?;
    res.insert("daemon_mode".to_string(), Value::String(mode.clone()));
    if !started {
        res.insert(
            "warning".to_string(),
            Value::String(format!(
                "Routing config enabled, but router daemon reported: {}",
                mode
            )),
        );
    }
    Ok(res)
}

fn cmd_disable(target: &str) -> Result<Map<String, Value>, String> {
    if !VALID_TARGETS.contains(&target) {
        return Err(format!(
            "Invalid target '{}'. Must be one of {:?}",
            target, VALID_TARGETS
        ));
    }

    let mut state = load_state()?;
    let primary_model = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash")
        .to_string();
    let reasoning_effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();

    if target == "claude" || target == "both" {
        configure_claude_routing(false, &primary_model, &mut state)?;
    }
    if target == "codex" || target == "both" {
        configure_codex_routing(false, &primary_model, &reasoning_effort, &mut state)?;
    }
    save_state(&state)?;
    build_status_dict()
}

fn cmd_toggle(target: &str) -> Result<Map<String, Value>, String> {
    if !VALID_TARGETS.contains(&target) {
        return Err(format!(
            "Invalid target '{}'. Must be one of {:?}",
            target, VALID_TARGETS
        ));
    }

    let claude_on = is_claude_routed()?;
    let codex_on = is_codex_routed()?;
    let enable = match target {
        "claude" => !claude_on,
        "codex" => !codex_on,
        _ => !(claude_on && codex_on),
    };
    if enable {
        cmd_enable(target, None, None)
    } else {
        cmd_disable(target)
    }
}

fn cmd_set_model(
    model_arg: Option<&str>,
    effort_arg: Option<&str>,
    flip: bool,
) -> Result<Map<String, Value>, String> {
    let mut state = load_state()?;
    if flip {
        let current = state
            .get("primary_model")
            .and_then(|v| v.as_str())
            .unwrap_or("gemini-3.8-flash");
        let next = if current.contains('4') {
            "gemini-3.8-flash"
        } else {
            "gemini-4-argon"
        };
        state.insert("primary_model".to_string(), Value::String(next.to_string()));
    } else if let Some(m) = model_arg {
        state.insert(
            "primary_model".to_string(),
            Value::String(normalize_model(Some(m))?),
        );
    }
    if let Some(e) = effort_arg {
        state.insert(
            "reasoning_effort".to_string(),
            Value::String(normalize_effort(Some(e))?),
        );
    }

    let primary_model = state
        .get("primary_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-3.8-flash")
        .to_string();
    let reasoning_effort = state
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();

    if is_claude_routed()? {
        configure_claude_routing(true, &primary_model, &mut state)?;
    }
    if is_codex_routed()? {
        configure_codex_routing(true, &primary_model, &reasoning_effort, &mut state)?;
    }

    save_state(&state)?;
    push_router_config(&primary_model, &reasoning_effort, true);
    build_status_dict()
}

fn cmd_start_router() -> Result<Map<String, Value>, String> {
    let mut state = load_state()?;
    state.insert("mock_router_running".to_string(), Value::Bool(true));
    save_state(&state)?;
    let (ok, detail) = start_router_daemon(&state);
    let mut res = build_status_dict()?;
    res.insert("ok".to_string(), Value::Bool(ok));
    res.insert("detail".to_string(), Value::String(detail.clone()));
    if !ok {
        res.insert("error".to_string(), Value::String(detail));
    }
    Ok(res)
}

fn cmd_stop_router() -> Result<Map<String, Value>, String> {
    let mut state = load_state()?;
    state.insert("mock_router_running".to_string(), Value::Bool(false));
    save_state(&state)?;
    let (ok, detail) = stop_router_daemon();
    let mut res = build_status_dict()?;
    res.insert("ok".to_string(), Value::Bool(ok));
    res.insert("detail".to_string(), Value::String(detail));
    Ok(res)
}

fn cmd_restart_router() -> Result<Map<String, Value>, String> {
    let _ = stop_router_daemon();
    let mut state = load_state()?;
    state.insert("mock_router_running".to_string(), Value::Bool(true));
    save_state(&state)?;
    let (ok, detail) = start_router_daemon(&state);
    let mut res = build_status_dict()?;
    res.insert("ok".to_string(), Value::Bool(ok));
    res.insert("detail".to_string(), Value::String(detail));
    Ok(res)
}

fn cmd_logs(lines: usize) -> Result<Map<String, Value>, String> {
    let clamped = lines.clamp(1, 500);
    let paths = get_paths();
    let log_file = paths.config_dir.join("router.log");
    let lines_s = clamped.to_string();

    let (rc, out, _) = run_cmd(
        &[
            "journalctl",
            "--user",
            "-u",
            "claude-gemini-router.service",
            "-n",
            &lines_s,
            "--no-pager",
        ],
        Duration::from_secs(5),
    );

    let entries: Vec<String> = if rc == 0 && !out.trim().is_empty() && !out.contains("-- No entries --") {
        let all: Vec<String> = out.trim().lines().map(|s| s.to_string()).collect();
        let start = all.len().saturating_sub(clamped);
        all[start..].to_vec()
    } else if log_file.is_file() {
        let raw = read_file_safely(&log_file)?.unwrap_or_default();
        let all: Vec<String> = raw.trim().lines().map(|s| s.to_string()).collect();
        let start = all.len().saturating_sub(clamped);
        all[start..].to_vec()
    } else {
        Vec::new()
    };

    let text = if entries.is_empty() {
        "No router log entries yet.".to_string()
    } else {
        entries.join("\n")
    };
    let val = json!({
        "ok": true,
        "lines": entries,
        "text": text,
    });
    Ok(val.as_object().unwrap().clone())
}

fn cmd_adc_login() -> Result<Map<String, Value>, String> {
    let candidates: &[&[&str]] = &[
        &[
            "uwsm-app",
            "--",
            "xdg-terminal-exec",
            "gcloud",
            "auth",
            "application-default",
            "login",
        ],
        &[
            "xdg-terminal-exec",
            "gcloud",
            "auth",
            "application-default",
            "login",
        ],
        &[
            "kitty",
            "-e",
            "gcloud",
            "auth",
            "application-default",
            "login",
        ],
        &[
            "alacritty",
            "-e",
            "gcloud",
            "auth",
            "application-default",
            "login",
        ],
    ];

    for term_cmd in candidates {
        if which_in_path(term_cmd[0]) {
            if Command::new(term_cmd[0])
                .args(&term_cmd[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
            {
                let val = json!({
                    "ok": true,
                    "launched": true,
                    "command": term_cmd.join(" "),
                });
                return Ok(val.as_object().unwrap().clone());
            }
        }
    }

    let val = json!({
        "ok": false,
        "launched": false,
        "error": "No terminal emulator found. Run: gcloud auth application-default login",
    });
    Ok(val.as_object().unwrap().clone())
}

fn dispatch_cli() -> Result<Map<String, Value>, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return build_status_dict();
    }

    let sub = args[0].as_str();
    let mut target = "both".to_string();
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut flip = false;
    let mut lines: usize = 40;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--target" => {
                i += 1;
                if i < args.len() {
                    target = args[i].clone();
                }
            }
            arg if arg.starts_with("--target=") => {
                target = arg["--target=".len()..].to_string();
            }
            "--model" => {
                i += 1;
                if i < args.len() {
                    model = Some(args[i].clone());
                }
            }
            arg if arg.starts_with("--model=") => {
                model = Some(arg["--model=".len()..].to_string());
            }
            "--effort" => {
                i += 1;
                if i < args.len() {
                    effort = Some(args[i].clone());
                }
            }
            arg if arg.starts_with("--effort=") => {
                effort = Some(arg["--effort=".len()..].to_string());
            }
            "--flip" => {
                flip = true;
            }
            "--lines" => {
                i += 1;
                if i < args.len() {
                    lines = args[i].parse().unwrap_or(40);
                }
            }
            arg if arg.starts_with("--lines=") => {
                lines = arg["--lines=".len()..].parse().unwrap_or(40);
            }
            _ => {}
        }
        i += 1;
    }

    match sub {
        "status" => build_status_dict(),
        "enable" => cmd_enable(&target, model.as_deref(), effort.as_deref()),
        "disable" => cmd_disable(&target),
        "toggle" => cmd_toggle(&target),
        "set-model" => cmd_set_model(model.as_deref(), effort.as_deref(), flip),
        "start-router" => cmd_start_router(),
        "stop-router" => cmd_stop_router(),
        "restart-router" => cmd_restart_router(),
        "logs" => cmd_logs(lines),
        "adc-login" => cmd_adc_login(),
        other => Err(format!("Unknown subcommand: '{}'", other)),
    }
}

fn main() {
    match dispatch_cli() {
        Ok(res) => {
            println!("{}", serde_json::to_string(&Value::Object(res)).unwrap_or_default());
        }
        Err(err) => {
            println!(
                "{}",
                serde_json::to_string(&json!({"ok": false, "error": err})).unwrap_or_default()
            );
        }
    }
}
