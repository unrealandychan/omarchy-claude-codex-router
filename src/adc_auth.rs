use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const FORBIDDEN_API_KEY_VARS: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY"];

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

pub struct ADCAuthManager {
    pub project_id: Mutex<Option<String>>,
    pub location: String,
    pub stripped_vars: Vec<String>,
    cached_token: Mutex<Option<CachedToken>>,
    http_client: reqwest::Client,
}

impl ADCAuthManager {
    pub fn new(project_id: Option<String>, location: Option<String>) -> Self {
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
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        };
        let _ = manager.resolve_project_id();
        manager
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

        {
            let guard = self.cached_token.lock().unwrap();
            if let Some(ref cached) = *guard {
                if Instant::now() + Duration::from_secs(60) < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }

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
                                            let token_str = tok.to_string();
                                            *self.cached_token.lock().unwrap() = Some(CachedToken {
                                                access_token: token_str.clone(),
                                                expires_at: Instant::now()
                                                    + Duration::from_secs(expires_in),
                                            });
                                            return Ok(token_str);
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

        // Fallback to gcloud CLI
        let gcloud_bin = Self::find_gcloud_binary();
        let out = Command::new(&gcloud_bin)
            .args(["auth", "application-default", "print-access-token", "--quiet"])
            .output()
            .map_err(|e| {
                let extra = oauth_err.as_deref().unwrap_or("");
                anyhow!(
                    "Failed to obtain Google Cloud ADC credentials ({} {}). Run: gcloud auth application-default login",
                    e,
                    extra
                )
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first_line = stderr.lines().next().unwrap_or(stderr.trim());
            return Err(anyhow!(
                "Failed to obtain Google Cloud ADC credentials: {}. Run: gcloud auth application-default login",
                first_line.trim()
            ));
        }

        let token_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if token_str.is_empty() {
            return Err(anyhow!(
                "ADC credentials did not return a valid access token."
            ));
        }
        *self.cached_token.lock().unwrap() = Some(CachedToken {
            access_token: token_str.clone(),
            expires_at: Instant::now() + Duration::from_secs(3000),
        });
        Ok(token_str)
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
        let project = self.resolve_project_id();
        match self.get_access_token().await {
            Ok(token) => {
                let ok = !token.is_empty();
                json!({
                    "authenticated": ok,
                    "token_valid": ok,
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
