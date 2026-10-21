use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FORBIDDEN_API_KEY_VARS: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY"];

struct CachedToken {
    access_token: String,
    refresh_after_epoch: u64,
    adc_fingerprint: Option<(SystemTime, u64)>,
}

struct AuthFailureState {
    status_code: u16,
    message: String,
    adc_fingerprint: Option<(SystemTime, u64)>,
}

pub struct ADCAuthManager {
    pub project_id: Mutex<Option<String>>,
    pub location: String,
    pub stripped_vars: Vec<String>,
    cached_token: Mutex<Option<CachedToken>>,
    last_rejected_token: Mutex<Option<String>>,
    last_auth_failure: Mutex<Option<AuthFailureState>>,
    last_notify_epoch: Mutex<u64>,
    http_client: reqwest::Client,
}

impl ADCAuthManager {
    pub fn new(project_id: Option<String>, location: Option<String>) -> Self {
        for key in ["NO_PROXY", "no_proxy"] {
            let current = env::var(key).unwrap_or_default();
            if !current.contains("127.0.0.1") {
                let updated = if current.trim().is_empty() {
                    "127.0.0.1,localhost,::1".to_string()
                } else {
                    format!("{},127.0.0.1,localhost,::1", current)
                };
                env::set_var(key, updated);
            }
        }
        let stripped_vars = Self::strip_api_key_env_vars();
        let resolved_location = location
            .or_else(|| env::var("GOOGLE_CLOUD_LOCATION").ok())
            .or_else(|| env::var("CLOUD_ML_REGION").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "global".to_string());

        let manager = Self {
            project_id: Mutex::new(project_id.filter(|s| !s.trim().is_empty())),
            location: resolved_location,
            stripped_vars,
            cached_token: Mutex::new(None),
            last_rejected_token: Mutex::new(None),
            last_auth_failure: Mutex::new(None),
            last_notify_epoch: Mutex::new(0),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .pool_idle_timeout(Duration::from_secs(30))
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        };
        let _ = manager.resolve_project_id();
        manager
    }

    fn now_epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn adc_fingerprint() -> Option<(SystemTime, u64)> {
        let adc_path = Self::adc_file_path();
        let meta = fs::metadata(&adc_path).ok()?;
        let modified = meta.modified().ok()?;
        Some((modified, meta.len()))
    }

    pub fn invalidate_token(&self) {
        *self.cached_token.lock().unwrap() = None;
        *self.last_rejected_token.lock().unwrap() = None;
        *self.last_auth_failure.lock().unwrap() = None;
    }

    pub fn prepare_auth_retry(&self) {
        let prev_tok = self
            .cached_token
            .lock()
            .unwrap()
            .take()
            .map(|c| c.access_token);
        if prev_tok.is_some() {
            *self.last_rejected_token.lock().unwrap() = prev_tok;
        }
        // Re-check project ID from disk in case 403 was caused by a project switch
        if let Some(disk_proj) = Self::read_project_id_from_disk() {
            *self.project_id.lock().unwrap() = Some(disk_proj);
        }
    }

    pub fn clear_auth_failure(&self) {
        *self.last_rejected_token.lock().unwrap() = None;
        *self.last_auth_failure.lock().unwrap() = None;
    }

    pub fn record_auth_failure(&self, status_code: u16, detail: &str) {
        let summary = format!(
            "HTTP {}: ADC credential reset required ({})",
            status_code,
            detail.lines().next().unwrap_or(detail).trim()
        );
        *self.last_auth_failure.lock().unwrap() = Some(AuthFailureState {
            status_code,
            message: summary.clone(),
            adc_fingerprint: Self::adc_fingerprint(),
        });
        self.notify_credential_reset_needed(status_code, &summary);
    }

    fn notify_credential_reset_needed(&self, status_code: u16, summary: &str) {
        if env::var("GEMINI_UPSTREAM_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        {
            return;
        }
        if let Ok(custom_adc) = env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            if custom_adc.starts_with("/tmp") {
                return;
            }
        }
        let now = Self::now_epoch_secs();
        {
            let mut last = self.last_notify_epoch.lock().unwrap();
            if now.saturating_sub(*last) < 60 {
                return;
            }
            *last = now;
        }
        let title = format!("Gemini ADC Router: Auth Error ({})", status_code);
        let body = format!(
            "Auto-recovery could not refresh credentials.\nPlease click 'Reset ADC Credentials' in the Gemini Router popup.\n{}",
            summary
        );
        let _ = Command::new("notify-send")
            .args([
                "-u",
                "critical",
                "-a",
                "Gemini ADC Router",
                &title,
                &body,
            ])
            .spawn();
    }

    fn read_project_id_from_disk() -> Option<String> {
        for env_key in [
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "ANTHROPIC_VERTEX_PROJECT_ID",
        ] {
            if let Ok(val) = env::var(env_key) {
                let val = val.trim().to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
        let adc_path = Self::adc_file_path();
        if adc_path.is_file() {
            if let Ok(text) = fs::read_to_string(&adc_path) {
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    if let Some(proj) = data
                        .get("quota_project_id")
                        .or_else(|| data.get("project_id"))
                        .and_then(|v| v.as_str())
                    {
                        let proj = proj.trim().to_string();
                        if !proj.is_empty() {
                            return Some(proj);
                        }
                    }
                }
            }
        }
        let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let gcloud_cfg = PathBuf::from(home).join(".config/gcloud/configurations/config_default");
        if gcloud_cfg.is_file() {
            if let Ok(text) = fs::read_to_string(&gcloud_cfg) {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("project") {
                        let rest = rest.trim_start();
                        if let Some(val) = rest.strip_prefix('=') {
                            let proj = val.trim().to_string();
                            if !proj.is_empty() {
                                return Some(proj);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    pub fn strip_api_key_env_vars() -> Vec<String> {
        let mut stripped = Vec::new();
        for &var in FORBIDDEN_API_KEY_VARS {
            if env::var_os(var).is_some() {
                env::remove_var(var);
                stripped.push(var.to_string());
            }
        }
        env::set_var("GOOGLE_API_USE_CLIENT_CERTIFICATE", "false");
        stripped
    }

    pub fn adc_file_path() -> PathBuf {
        if let Ok(custom) = env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            if !custom.trim().is_empty() {
                return PathBuf::from(custom);
            }
        }
        let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join(".config/gcloud/application_default_credentials.json")
    }

    pub fn resolve_project_id(&self) -> Option<String> {
        {
            let guard = self.project_id.lock().unwrap();
            if let Some(ref p) = *guard {
                if !p.is_empty() {
                    return Some(p.clone());
                }
            }
        }

        for env_key in [
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "ANTHROPIC_VERTEX_PROJECT_ID",
        ] {
            if let Ok(val) = env::var(env_key) {
                let val = val.trim().to_string();
                if !val.is_empty() {
                    *self.project_id.lock().unwrap() = Some(val.clone());
                    return Some(val);
                }
            }
        }

        let adc_path = Self::adc_file_path();
        if adc_path.is_file() {
            if let Ok(text) = fs::read_to_string(&adc_path) {
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    if let Some(proj) = data
                        .get("quota_project_id")
                        .or_else(|| data.get("project_id"))
                        .and_then(|v| v.as_str())
                    {
                        let proj = proj.trim().to_string();
                        if !proj.is_empty() {
                            *self.project_id.lock().unwrap() = Some(proj.clone());
                            return Some(proj);
                        }
                    }
                }
            }
        }

        let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let gcloud_cfg = PathBuf::from(home).join(".config/gcloud/configurations/config_default");
        if gcloud_cfg.is_file() {
            if let Ok(text) = fs::read_to_string(&gcloud_cfg) {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("project") {
                        let rest = rest.trim_start();
                        if let Some(val) = rest.strip_prefix('=') {
                            let proj = val.trim().to_string();
                            if !proj.is_empty() {
                                *self.project_id.lock().unwrap() = Some(proj.clone());
                                return Some(proj);
                            }
                        }
                    }
                }
            }
        }

        if let Ok(out) = Command::new(Self::find_gcloud_binary())
            .args(["config", "get-value", "project", "--quiet"])
            .output()
        {
            if out.status.success() {
                let proj = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !proj.is_empty() && proj != "(unset)" {
                    *self.project_id.lock().unwrap() = Some(proj.clone());
                    return Some(proj);
                }
            }
        }

        None
    }

    fn find_gcloud_binary() -> PathBuf {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/arch".to_string());
        let candidates = [
            PathBuf::from(&home).join("google-cloud-sdk/bin/gcloud"),
            PathBuf::from("/home/arch/google-cloud-sdk/bin/gcloud"),
            PathBuf::from("/opt/google-cloud-sdk/bin/gcloud"),
            PathBuf::from("/usr/bin/gcloud"),
            PathBuf::from("/usr/local/bin/gcloud"),
        ];
        for c in candidates {
            if c.is_file() {
                return c;
            }
        }
        PathBuf::from("gcloud")
    }

    pub async fn get_access_token(&self) -> Result<String> {
        if let Ok(upstream) = env::var("GEMINI_UPSTREAM_URL") {
            if !upstream.trim().is_empty() {
                return Ok("keyless-upstream".to_string());
            }
        }

        let current_fp = Self::adc_fingerprint();
        {
            let guard = self.cached_token.lock().unwrap();
            if let Some(ref cached) = *guard {
                let now = Self::now_epoch_secs();
                if now < cached.refresh_after_epoch && cached.adc_fingerprint == current_fp {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        let rejected_tok = self.last_rejected_token.lock().unwrap().clone();
        let mut oauth_candidate: Option<String> = None;
        let mut oauth_err: Option<String> = None;
        let adc_path = Self::adc_file_path();
        if adc_path.is_file() {
            if let Ok(text) = fs::read_to_string(&adc_path) {
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    let client_id = data.get("client_id").and_then(|v| v.as_str());
                    let client_secret = data.get("client_secret").and_then(|v| v.as_str());
                    let refresh_token = data.get("refresh_token").and_then(|v| v.as_str());
                    let token_uri = data
                        .get("token_uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("https://oauth2.googleapis.com/token");

                    if let (Some(cid), Some(csec), Some(rtok)) =
                        (client_id, client_secret, refresh_token)
                    {
                        let params = [
                            ("client_id", cid),
                            ("client_secret", csec),
                            ("refresh_token", rtok),
                            ("grant_type", "refresh_token"),
                        ];
                        match self.http_client.post(token_uri).form(&params).send().await {
                            Ok(resp) => {
                                let status = resp.status();
                                if let Ok(body) = resp.json::<Value>().await {
                                    if status.is_success() {
                                        if let Some(tok) =
                                            body.get("access_token").and_then(|v| v.as_str())
                                        {
                                            let expires_in = body
                                                .get("expires_in")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(3600);
                                            let margin = std::cmp::min(300, expires_in / 2);
                                            let refresh_after_epoch = Self::now_epoch_secs()
                                                .saturating_add(expires_in.saturating_sub(margin));
                                            let token_str = tok.to_string();
                                            if rejected_tok.as_deref() != Some(token_str.as_str()) {
                                                *self.last_rejected_token.lock().unwrap() = None;
                                                *self.cached_token.lock().unwrap() =
                                                    Some(CachedToken {
                                                        access_token: token_str.clone(),
                                                        refresh_after_epoch,
                                                        adc_fingerprint: Self::adc_fingerprint(),
                                                    });
                                                return Ok(token_str);
                                            }
                                            oauth_candidate = Some(token_str);
                                        }
                                    } else {
                                        let desc = body
                                            .get("error_description")
                                            .or_else(|| body.get("error"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("OAuth2 refresh failed");
                                        oauth_err = Some(desc.to_string());
                                    }
                                }
                            }
                            Err(e) => {
                                oauth_err = Some(e.to_string());
                            }
                        }
                    }
                }
            }
        }

        // Fallback to gcloud CLI (also used if OAuth refresh returned a previously rejected token)
        let gcloud_bin = Self::find_gcloud_binary();
        if let Ok(out) = Command::new(&gcloud_bin)
            .args(["auth", "application-default", "print-access-token", "--quiet"])
            .output()
        {
            if out.status.success() {
                let token_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !token_str.is_empty() && rejected_tok.as_deref() != Some(token_str.as_str()) {
                    *self.last_rejected_token.lock().unwrap() = None;
                    *self.cached_token.lock().unwrap() = Some(CachedToken {
                        access_token: token_str.clone(),
                        refresh_after_epoch: Self::now_epoch_secs().saturating_add(3000),
                        adc_fingerprint: Self::adc_fingerprint(),
                    });
                    return Ok(token_str);
                }
            } else if oauth_err.is_none() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let first_line = stderr.lines().next().unwrap_or(stderr.trim());
                oauth_err = Some(first_line.trim().to_string());
            }
        }

        if let Some(candidate) = oauth_candidate {
            *self.cached_token.lock().unwrap() = Some(CachedToken {
                access_token: candidate.clone(),
                refresh_after_epoch: Self::now_epoch_secs().saturating_add(3300),
                adc_fingerprint: Self::adc_fingerprint(),
            });
            return Ok(candidate);
        }

        let err_detail = oauth_err
            .unwrap_or_else(|| "No valid ADC token found".to_string());
        self.record_auth_failure(401, &err_detail);
        Err(anyhow!(
            "Failed to obtain Google Cloud ADC credentials: {}. Run: gcloud auth application-default login",
            err_detail
        ))
    }

    pub async fn get_auth_headers(&self) -> Result<Vec<(String, String)>> {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Ok(upstream) = env::var("GEMINI_UPSTREAM_URL") {
            if !upstream.trim().is_empty() {
                return Ok(headers);
            }
        }
        let token = self.get_access_token().await?;
        headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
        if let Some(project) = self.resolve_project_id() {
            headers.push(("x-goog-user-project".to_string(), project));
        }
        Ok(headers)
    }

    pub async fn status(&self) -> Value {
        let adc_file = Self::adc_file_path();
        let current_fp = Self::adc_fingerprint();

        // Check if a previous unrecoverable 401/403 occurred and the ADC file hasn't been updated yet
        let active_failure = {
            let mut fail_guard = self.last_auth_failure.lock().unwrap();
            if let Some(ref f) = *fail_guard {
                if f.adc_fingerprint == current_fp {
                    Some((f.status_code, f.message.clone()))
                } else {
                    // User updated ADC file on disk -> clear failure and force token re-read
                    *fail_guard = None;
                    *self.cached_token.lock().unwrap() = None;
                    *self.last_rejected_token.lock().unwrap() = None;
                    None
                }
            } else {
                None
            }
        };

        let project = self.resolve_project_id();
        if let Some((code, msg)) = active_failure {
            return json!({
                "authenticated": false,
                "token_valid": false,
                "auth_alert": true,
                "auth_status_code": code,
                "api_key_used": false,
                "auth_mode": "adc_only",
                "adc_file_exists": adc_file.is_file(),
                "adc_file": adc_file.to_string_lossy(),
                "project_id": project,
                "location": self.location,
                "stripped_api_key_vars": self.stripped_vars,
                "error": msg,
            });
        }

        match self.get_access_token().await {
            Ok(token) => {
                let ok = !token.is_empty();
                json!({
                    "authenticated": ok,
                    "token_valid": ok,
                    "auth_alert": false,
                    "api_key_used": false,
                    "auth_mode": "adc_only",
                    "adc_file_exists": adc_file.is_file(),
                    "adc_file": adc_file.to_string_lossy(),
                    "project_id": project,
                    "location": self.location,
                    "stripped_api_key_vars": self.stripped_vars,
                    "error": Value::Null,
                })
            }
            Err(err) => json!({
                "authenticated": false,
                "token_valid": false,
                "auth_alert": true,
                "auth_status_code": 401,
                "api_key_used": false,
                "auth_mode": "adc_only",
                "adc_file_exists": adc_file.is_file(),
                "adc_file": adc_file.to_string_lossy(),
                "project_id": project,
                "location": self.location,
                "stripped_api_key_vars": self.stripped_vars,
                "error": err.to_string(),
            }),
        }
    }
}
