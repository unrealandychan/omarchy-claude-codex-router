"""Strict Application Default Credentials (ADC) authentication manager.

Guarantees zero API key usage (strips GEMINI_API_KEY / GOOGLE_API_KEY) and
handles Google Cloudtop Certificate-Bound Access (CBA/mTLS) token avoidance so
standard HTTPS requests to Vertex AI succeed with pure ADC.
"""

from __future__ import annotations

import json
import os
import time
import urllib.parse
import urllib.request
from typing import Any

import google.auth
import google.auth.transport.requests

FORBIDDEN_API_KEY_VARS = ("GEMINI_API_KEY", "GOOGLE_API_KEY")
CLOUD_PLATFORM_SCOPE = "https://www.googleapis.com/auth/cloud-platform"
DEFAULT_LOCATION = "global"


class ADCAuthManager:
    """Manages keyless Google Cloud ADC Bearer tokens and project resolution."""

    def __init__(
        self,
        project_id: str | None = None,
        location: str | None = None,
    ) -> None:
        self.stripped_api_key_vars = self._strip_api_keys_and_fix_cloudtop_env()
        self._cached_token: str | None = None
        self._token_expiry: float = 0.0
        self._adc_file_path = os.path.expanduser(
            os.environ.get(
                "GOOGLE_APPLICATION_CREDENTIALS",
                "~/.config/gcloud/application_default_credentials.json",
            )
        )
        self.project_id = (
            project_id
            or os.environ.get("VERTEX_PROJECT")
            or os.environ.get("GOOGLE_CLOUD_PROJECT")
            or self._detect_project_id()
        )
        self.location = (
            location
            or os.environ.get("VERTEX_LOCATION")
            or os.environ.get("GOOGLE_CLOUD_LOCATION")
            or DEFAULT_LOCATION
        )

    @staticmethod
    def _strip_api_keys_and_fix_cloudtop_env() -> list[str]:
        """Strip API key env vars and disable mTLS client cert binding for standard TLS."""
        stripped: list[str] = []
        for var in FORBIDDEN_API_KEY_VARS:
            if var in os.environ:
                os.environ.pop(var, None)
                stripped.append(var)
        # Prevent Cloudtop ECP from minting CBA tokens that fail over standard HTTPS
        os.environ["GOOGLE_API_USE_CLIENT_CERTIFICATE"] = "false"
        return stripped

    def _detect_project_id(self) -> str:
        """Detect project ID from ADC JSON quota_project_id or google.auth.default()."""
        if os.path.isfile(self._adc_file_path):
            try:
                with open(self._adc_file_path, encoding="utf-8") as f:
                    data = json.load(f)
                if data.get("quota_project_id"):
                    return str(data["quota_project_id"])
            except Exception:
                pass
        try:
            _, default_proj = google.auth.default(scopes=[CLOUD_PLATFORM_SCOPE])
            if default_proj:
                return str(default_proj)
        except Exception:
            pass
        return "gcp-moodle-dev"

    def get_access_token(self) -> str:
        """Return a valid non-CBA OAuth2 access token from ADC (cached in memory)."""
        now = time.time()
        if self._cached_token and now < (self._token_expiry - 60):
            return self._cached_token

        # 1. Prefer direct refresh_token exchange from ADC JSON (avoids Cloudtop CBA issues)
        if os.path.isfile(self._adc_file_path):
            try:
                with open(self._adc_file_path, encoding="utf-8") as f:
                    adc = json.load(f)
                if all(k in adc for k in ("client_id", "client_secret", "refresh_token")):
                    form = urllib.parse.urlencode(
                        {
                            "client_id": adc["client_id"],
                            "client_secret": adc["client_secret"],
                            "refresh_token": adc["refresh_token"],
                            "grant_type": "refresh_token",
                        }
                    ).encode("utf-8")
                    req = urllib.request.Request(
                        "https://oauth2.googleapis.com/token",
                        data=form,
                        headers={"Content-Type": "application/x-www-form-urlencoded"},
                    )
                    with urllib.request.urlopen(req, timeout=10) as resp:
                        token_data = json.loads(resp.read().decode("utf-8"))
                    self._cached_token = str(token_data["access_token"])
                    self._token_expiry = now + float(token_data.get("expires_in", 3600))
                    return self._cached_token
            except Exception:
                pass

        # 2. Fallback to google.auth.default() (Service Account / Metadata Server / WIF)
        creds, _ = google.auth.default(scopes=[CLOUD_PLATFORM_SCOPE])
        creds.refresh(google.auth.transport.requests.Request())
        if not creds.token:
            raise RuntimeError(
                "ADC authentication failed. Run: gcloud auth application-default login"
            )
        self._cached_token = str(creds.token)
        self._token_expiry = now + 3000.0
        return self._cached_token

    def status_dict(self) -> dict[str, Any]:
        """Return diagnostic status confirming ADC-only mode."""
        token_ok = False
        error_msg = None
        try:
            tok = self.get_access_token()
            token_ok = bool(tok)
        except Exception as exc:
            error_msg = str(exc)

        return {
            "auth_mode": "ADC (Application Default Credentials)",
            "api_key_used": False,
            "stripped_api_key_vars": self.stripped_api_key_vars,
            "project_id": self.project_id,
            "location": self.location,
            "adc_file_exists": os.path.isfile(self._adc_file_path),
            "token_valid": token_ok,
            "error": error_msg,
        }
