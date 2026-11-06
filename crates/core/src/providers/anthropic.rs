//! Anthropic OAuth endpoints.
//!
//! * Profile: `GET /api/oauth/profile` with `Authorization: Bearer` and `anthropic-beta: oauth-2025-04-20`
//!   says who owns an access token (ported from claude-swap `oauth.py:281-340`, `fetch_oauth_profile`).
//! * Usage: `GET /api/oauth/usage` (same headers) returns `five_hour` and `seven_day` as
//!   `{utilization (percent), resets_at (ISO 8601)}` or `null` (claude-swap `oauth.py:413-424`, `:625-680`).
//! * Refresh: `POST https://platform.claude.com/v1/oauth/token`, JSON body (claude-swap `oauth.py:20-21`,
//!   `:187-238`). The contract and its error classes are written out in the phase 5 plan.
//!
//! HTTP goes through `ureq` with rustls and bundled roots, so it does not depend on the system certificate
//! setup. Tokens only ever travel in a request header or body; error text never includes them.

use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::providers::openai::parse_rfc3339_us;
use crate::state::{WindowSnap, Windows};
use crate::token_refresh::RefreshError;
use crate::usage::{classify_usage_reply, http_get, http_post_json, Credential, FetchError};
use crate::vault::{lineage_fingerprint, normalize_blob};

pub const API_BASE: &str = "https://api.anthropic.com";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TIMEOUT: Duration = Duration::from_secs(5);

/// The identity an access token belongs to. Missing optional fields stay `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub account_uuid: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub full_name: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    /// For example `claude_max`.
    pub organization_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

impl Profile {
    /// Plan name without the `claude_` prefix (`max`, `pro`, ...).
    pub fn plan(&self) -> Option<String> {
        self.organization_type.as_deref().map(|t| t.strip_prefix("claude_").unwrap_or(t).to_string())
    }

    /// Parse a profile response. A usable identity needs a non-empty `account.uuid`.
    pub fn from_json(body: &Value) -> Result<Profile> {
        let account = body.get("account").and_then(Value::as_object);
        let uuid = account.and_then(|a| a.get("uuid")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let (Some(account), Some(uuid)) = (account, uuid) else {
            return Err(Error::Invalid("profile response has no account uuid (the endpoint format may have changed)".into()));
        };
        let org = body.get("organization").and_then(Value::as_object);
        let text = |m: Option<&serde_json::Map<String, Value>>, k: &str| {
            m.and_then(|m| m.get(k)).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
        };
        Ok(Profile {
            account_uuid: uuid.to_string(),
            email: text(Some(account), "email").or_else(|| text(Some(account), "email_address")),
            display_name: text(Some(account), "display_name"),
            full_name: text(Some(account), "full_name"),
            organization_uuid: text(org, "uuid"),
            organization_name: text(org, "name"),
            organization_type: text(org, "organization_type"),
            rate_limit_tier: text(org, "rate_limit_tier"),
        })
    }
}

/// Seam for tests and for callers that must not touch the network.
pub trait ProfileSource {
    fn profile(&self, access_token: &str) -> Result<Profile>;
}

pub struct AnthropicApi {
    base: String,
    token_url: String,
}

impl Default for AnthropicApi {
    fn default() -> Self {
        AnthropicApi { base: API_BASE.to_string(), token_url: TOKEN_URL.to_string() }
    }
}

impl AnthropicApi {
    /// Point both the API and the token endpoint at `base` (tests use a local server).
    pub fn with_base(base: impl Into<String>) -> Self {
        let base = base.into();
        let token_url = format!("{}/v1/oauth/token", base.trim_end_matches('/'));
        AnthropicApi { base, token_url }
    }

    pub fn fetch_usage(&self, cred: &Credential) -> std::result::Result<Windows, FetchError> {
        let url = format!("{}/api/oauth/usage", self.base.trim_end_matches('/'));
        let auth = format!("Bearer {}", cred.access_token);
        let reply = http_get(
            &url,
            &[
                ("Authorization", &auth),
                ("anthropic-beta", OAUTH_BETA),
                ("User-Agent", concat!("agent-switch/", env!("CARGO_PKG_VERSION"))),
            ],
        )
        .map_err(|kind| FetchError::Failed { kind, retry_after_s: None })?;
        classify_usage_reply(reply, parse_usage)
    }

    /// Exchange the refresh token of the saved login `blob`. Returns the whole new login blob.
    pub fn refresh(&self, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
        let refresh_token = oauth_field(blob, "refreshToken")
            .ok_or_else(|| RefreshError::Dead("the saved login has no refresh token; sign in again".into()))?;
        let body = serde_json::json!({"grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": CLIENT_ID});
        let reply = http_post_json(&self.token_url, &body)
            .map_err(|kind| RefreshError::Transient { may_have_consumed: kind == "timeout", detail: kind })?;
        if reply.status != 200 {
            return Err(classify_refresh(reply.status, &reply.body));
        }
        let resp: Value = serde_json::from_str(&reply.body)
            .map_err(|_| RefreshError::Transient { detail: "the token endpoint answered with something unreadable".into(), may_have_consumed: true })?;
        apply_refresh(blob, &resp, now.saturating_mul(1000))
            .map_err(|e| RefreshError::Transient { detail: e.to_string(), may_have_consumed: true })
    }
}

/// `blob.login.claudeAiOauth.<key>` as text.
fn oauth_field(blob: &str, key: &str) -> Option<String> {
    let v: Value = serde_json::from_str(blob).ok()?;
    v.pointer(&format!("/login/claudeAiOauth/{key}")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The access token, expiry and refresh-token fingerprint of a saved Claude login (`ClaudeSlot` JSON).
pub fn credential(blob: &str) -> Result<Credential> {
    let v: Value = serde_json::from_str(blob).map_err(|e| Error::Invalid(format!("saved Claude login is not valid JSON: {e}")))?;
    let oauth = v.pointer("/login/claudeAiOauth").and_then(Value::as_object);
    let text = |k: &str| oauth.and_then(|o| o.get(k)).and_then(Value::as_str).filter(|s| !s.is_empty());
    let (Some(access), Some(refresh)) = (text("accessToken"), text("refreshToken")) else {
        return Err(Error::Invalid("saved Claude login has no access or refresh token".into()));
    };
    let expires_at = oauth.and_then(|o| o.get("expiresAt")).and_then(Value::as_u64).map(|ms| ms / 1000);
    Ok(Credential { access_token: access.to_string(), expires_at, workspace: None, fingerprint: lineage_fingerprint(refresh) })
}

/// Read the `/api/oauth/usage` body. A missing or `null` window means the account has no such limit; a
/// window with a bad `utilization` is a malformed response, and so is a body with neither window.
pub fn parse_usage(body: &Value) -> std::result::Result<Windows, FetchError> {
    let bad = || FetchError::Failed { kind: "bad-response".into(), retry_after_s: None };
    let window = |key: &str| -> std::result::Result<Option<WindowSnap>, FetchError> {
        let Some(w) = body.get(key).filter(|w| !w.is_null()) else { return Ok(None) };
        let w = w.as_object().ok_or_else(bad)?;
        let pct = match w.get("utilization") {
            Some(Value::Null) => return Ok(None),
            Some(v) => v.as_f64().filter(|p| p.is_finite() && *p >= 0.0).ok_or_else(bad)?,
            None => return Err(bad()),
        };
        let resets_at = w.get("resets_at").and_then(Value::as_str).and_then(parse_rfc3339_us).map(|us| us.div_euclid(1_000_000));
        Ok(Some(WindowSnap { pct, resets_at }))
    };
    let windows = Windows { five_hour: window("five_hour")?, weekly: window("seven_day")? };
    if windows.five_hour.is_none() && windows.weekly.is_none() {
        return Err(bad());
    }
    Ok(windows)
}

/// Sort a non-200 answer of the token endpoint into the three classes. A verdict needs an explicit top-level
/// `error` member (RFC 6749 section 5.2); anything ambiguous stays transient, because a wrongly transient
/// answer costs one retry while a wrongly permanent one would quarantine a live login (claude-swap
/// `oauth.py:207-238`).
pub fn classify_refresh(status: u16, body: &str) -> RefreshError {
    if matches!(status, 400 | 401 | 403) {
        let code = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        match code.as_deref() {
            Some("invalid_grant") => return RefreshError::Dead("the refresh token was rejected (invalid_grant); sign in to this account again".into()),
            Some("invalid_client") => return RefreshError::Systemic("the token endpoint rejected agent-switch's client (invalid_client)".into()),
            _ => {}
        }
    }
    RefreshError::Transient { detail: format!("the token endpoint answered HTTP {status}"), may_have_consumed: status >= 500 }
}

/// Merge a successful token response into the saved login: new access token and expiry, the rotated refresh
/// token and the scopes when the server sent them. Every other key stays (claude-swap `oauth.py:195-205`).
pub fn apply_refresh(blob: &str, resp: &Value, now_ms: u64) -> Result<String> {
    let mut v: Value = serde_json::from_str(blob).map_err(|e| Error::Invalid(e.to_string()))?;
    let access = resp.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty());
    let expires_in = resp.get("expires_in").and_then(Value::as_u64);
    let (Some(access), Some(expires_in)) = (access, expires_in) else {
        return Err(Error::Invalid("the token response has no access token or expiry".into()));
    };
    let oauth = v
        .pointer_mut("/login/claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::Invalid("saved Claude login has no claudeAiOauth object".into()))?;
    oauth.insert("accessToken".into(), access.into());
    oauth.insert("expiresAt".into(), (now_ms + expires_in * 1000).into());
    if let Some(rt) = resp.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        oauth.insert("refreshToken".into(), rt.into());
    }
    if let Some(scope) = resp.get("scope").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        oauth.insert("scopes".into(), scope.split_whitespace().collect::<Vec<_>>().into());
    }
    normalize_blob(&serde_json::to_string(&v).map_err(|e| Error::Invalid(e.to_string()))?)
}

impl ProfileSource for AnthropicApi {
    fn profile(&self, access_token: &str) -> Result<Profile> {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).max_redirects(0).build().into();
        let url = format!("{}/api/oauth/profile", self.base.trim_end_matches('/'));
        let mut resp = agent
            .get(&url)
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("anthropic-beta", OAUTH_BETA)
            .header("User-Agent", concat!("agent-switch/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => Error::Invalid(format!("profile request was refused (HTTP {code})")),
                other => Error::Invalid(format!("profile request failed: {other}")),
            })?;
        let text = resp.body_mut().read_to_string().map_err(|e| Error::Invalid(format!("profile response unreadable: {e}")))?;
        let body: Value = serde_json::from_str(&text).map_err(|_| Error::Invalid("profile response is not JSON".into()))?;
        Profile::from_json(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot HTTP server; returns the base URL and a handle yielding the raw request text.
    fn serve(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            sock.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (base, handle)
    }

    const BODY: &str = r#"{"account":{"uuid":"u-1","email":"a@b.com","display_name":"A"},"organization":{"uuid":"o-1","name":"Org","organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}"#;

    #[test]
    fn fetches_a_profile_with_bearer_and_beta_headers() {
        let (base, req) = serve("200 OK", BODY);
        let p = AnthropicApi::with_base(base).profile("tok-123").unwrap();
        assert_eq!(p.account_uuid, "u-1");
        assert_eq!(p.email.as_deref(), Some("a@b.com"));
        assert_eq!(p.plan().as_deref(), Some("max"));
        assert_eq!(p.organization_uuid.as_deref(), Some("o-1"));
        let raw = req.join().unwrap().to_lowercase();
        assert!(raw.starts_with("get /api/oauth/profile "), "{raw}");
        assert!(raw.contains("authorization: bearer tok-123"), "{raw}");
        assert!(raw.contains("anthropic-beta: oauth-2025-04-20"), "{raw}");
    }

    #[test]
    fn unauthorized_is_an_error_that_hides_the_token() {
        let (base, _req) = serve("401 Unauthorized", "{}");
        let err = AnthropicApi::with_base(base).profile("super-secret-token").unwrap_err().to_string();
        assert!(err.contains("401") && !err.contains("super-secret-token"), "{err}");
    }

    #[test]
    fn a_response_without_an_account_uuid_is_rejected() {
        let (base, _req) = serve("200 OK", r#"{"account":{"email":"a@b.com"}}"#);
        assert!(AnthropicApi::with_base(base).profile("t").is_err());
        assert!(Profile::from_json(&serde_json::json!({"account": {"uuid": "  "}})).is_err());
    }

    #[test]
    fn a_uuid_only_profile_resolves() {
        let p = Profile::from_json(&serde_json::json!({"account": {"uuid": "u"}})).unwrap();
        assert_eq!((p.plan(), p.email, p.organization_uuid), (None, None, None));
    }

    // ----- usage -----

    use crate::usage::testkit::serve as serve_full;

    const USAGE: &str = r#"{"five_hour":{"utilization":48.0,"resets_at":"2026-11-06T05:50:00.215883+00:00","limit_dollars":null},"seven_day":{"utilization":93.0,"resets_at":"2026-11-07T19:00:00.215904+00:00"},"seven_day_opus":null,"extra_usage":{}}"#;

    #[test]
    fn parses_both_windows_with_their_own_reset_times() {
        let w = parse_usage(&serde_json::from_str(USAGE).unwrap()).unwrap();
        let (five, week) = (w.five_hour.unwrap(), w.weekly.unwrap());
        assert_eq!((five.pct, week.pct), (48.0, 93.0));
        assert_eq!(five.resets_at, Some(1_793_944_200)); // 2026-11-06T05:50:00Z
        assert_eq!(week.resets_at, Some(1_794_078_000)); // 2026-11-07T19:00:00Z
        assert_ne!(five.resets_at, week.resets_at);
    }

    #[test]
    fn a_missing_or_null_window_is_absent_and_a_bad_one_is_an_error() {
        let w = parse_usage(&serde_json::json!({"five_hour": null, "seven_day": {"utilization": 7}})).unwrap();
        assert!(w.five_hour.is_none() && w.weekly.unwrap().pct == 7.0);
        assert!(parse_usage(&serde_json::json!({"five_hour": {"utilization": null}, "seven_day": {"utilization": 1}})).unwrap().five_hour.is_none());
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"five_hour": {"utilization": "9"}}),
            serde_json::json!({"five_hour": {"utilization": -1}}),
            serde_json::json!({"five_hour": {"resets_at": "x"}}),
            serde_json::json!({"five_hour": 5}),
        ] {
            assert!(matches!(parse_usage(&bad), Err(FetchError::Failed { .. })), "{bad}");
        }
    }

    fn cred() -> Credential {
        Credential { access_token: "tok-1".into(), expires_at: None, workspace: None, fingerprint: "f".into() }
    }

    #[test]
    fn fetches_usage_with_bearer_and_beta_headers() {
        let (base, req) = serve_full("200 OK", USAGE);
        let w = AnthropicApi::with_base(base).fetch_usage(&cred()).unwrap();
        assert_eq!(w.five_hour.unwrap().pct, 48.0);
        let raw = req.join().unwrap().to_lowercase();
        assert!(raw.starts_with("get /api/oauth/usage "), "{raw}");
        assert!(raw.contains("authorization: bearer tok-1") && raw.contains("anthropic-beta: oauth-2025-04-20"), "{raw}");
    }

    #[test]
    fn usage_status_codes_map_to_fetch_errors() {
        for (status, want) in [("401 Unauthorized", FetchError::Unauthorized), ("403 Forbidden", FetchError::Unauthorized)] {
            let (base, _r) = serve_full(status, "{}");
            assert_eq!(AnthropicApi::with_base(base).fetch_usage(&cred()).unwrap_err(), want);
        }
        let (base, _r) = serve_full("503 Service Unavailable", "{}");
        assert_eq!(AnthropicApi::with_base(base).fetch_usage(&cred()).unwrap_err(), FetchError::Failed { kind: "http-503".into(), retry_after_s: None });
        let (base, _r) = serve_full("200 OK", "not json");
        assert!(matches!(AnthropicApi::with_base(base).fetch_usage(&cred()), Err(FetchError::Failed { kind, .. }) if kind == "bad-response"));
        // Nothing listens on this port.
        assert!(matches!(AnthropicApi::with_base("http://127.0.0.1:1").fetch_usage(&cred()), Err(FetchError::Failed { kind, .. }) if kind == "network"));
    }

    #[test]
    fn a_429_carries_retry_after() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).unwrap();
            s.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        });
        assert_eq!(
            AnthropicApi::with_base(base).fetch_usage(&cred()).unwrap_err(),
            FetchError::Failed { kind: "http-429".into(), retry_after_s: Some(120) }
        );
    }

    // ----- refresh -----

    const SLOT: &str = r#"{"v":1,"login":{"claudeAiOauth":{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":1000,"scopes":["a"],"subscriptionType":"max"},"trustedDeviceToken":"dev"},"oauthAccount":{"accountUuid":"u"}}"#;

    #[test]
    fn reads_the_credential_from_a_saved_login() {
        let c = credential(SLOT).unwrap();
        assert_eq!((c.access_token.as_str(), c.expires_at), ("at-old", Some(1)));
        assert_eq!(c.fingerprint, lineage_fingerprint("rt-old"));
        assert!(credential(r#"{"login":{}}"#).is_err());
    }

    #[test]
    fn refresh_posts_json_and_keeps_every_other_key() {
        let (base, req) = serve_full("200 OK", r#"{"access_token":"at-new","expires_in":28800,"refresh_token":"rt-new","scope":"user:inference user:profile"}"#);
        let out = AnthropicApi::with_base(base).refresh(SLOT, 2_000).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let o = &v["login"]["claudeAiOauth"];
        assert_eq!((o["accessToken"].as_str(), o["refreshToken"].as_str()), (Some("at-new"), Some("rt-new")));
        assert_eq!(o["expiresAt"], 2_000_000 + 28_800_000);
        assert_eq!(o["scopes"], serde_json::json!(["user:inference", "user:profile"]));
        assert_eq!(o["subscriptionType"], "max");
        assert_eq!(v["login"]["trustedDeviceToken"], "dev");
        assert_eq!(v["oauthAccount"]["accountUuid"], "u");
        assert_eq!(credential(&out).unwrap().fingerprint, lineage_fingerprint("rt-new"));

        let raw = req.join().unwrap();
        assert!(raw.starts_with("POST /v1/oauth/token "), "{raw}");
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        let sent: Value = serde_json::from_str(body).unwrap();
        assert_eq!(sent, serde_json::json!({"grant_type": "refresh_token", "refresh_token": "rt-old", "client_id": CLIENT_ID}));
        assert!(raw.to_lowercase().contains("content-type: application/json"));
    }

    #[test]
    fn a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let out = apply_refresh(SLOT, &serde_json::json!({"access_token": "at2", "expires_in": 10}), 0).unwrap();
        assert_eq!(credential(&out).unwrap().fingerprint, lineage_fingerprint("rt-old"));
        assert!(apply_refresh(SLOT, &serde_json::json!({"expires_in": 10}), 0).is_err());
    }

    #[test]
    fn refresh_errors_fall_into_dead_systemic_and_transient() {
        use RefreshError::*;
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_grant"}"#), Dead(_)));
        assert!(matches!(classify_refresh(401, r#"{"error":"invalid_grant","error_description":"x"}"#), Dead(_)));
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_client"}"#), Systemic(_)));
        assert!(matches!(classify_refresh(403, r#"{"error":"invalid_client"}"#), Systemic(_)));
        // Ambiguous answers never quarantine.
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_request"}"#), Transient { .. }));
        assert!(matches!(classify_refresh(400, "<html>invalid_grant</html>"), Transient { .. }));
        assert!(matches!(classify_refresh(403, "challenge page"), Transient { .. }));
        assert!(matches!(classify_refresh(200, r#"{"error":"invalid_grant"}"#), Transient { .. }));
        assert!(matches!(classify_refresh(429, "{}"), Transient { may_have_consumed: false, .. }));
        assert!(matches!(classify_refresh(502, "{}"), Transient { may_have_consumed: true, .. }));
        // Nested error layouts are not Anthropic's: they stay transient here.
        assert!(matches!(classify_refresh(400, r#"{"error":{"code":"invalid_grant"}}"#), Transient { .. }));
    }

    #[test]
    fn the_refresh_call_surfaces_the_class_and_never_the_token() {
        let (base, _r) = serve_full("400 Bad Request", r#"{"error":"invalid_grant"}"#);
        let err = AnthropicApi::with_base(base).refresh(SLOT, 0).unwrap_err();
        assert!(matches!(err, RefreshError::Dead(_)));
        assert!(!err.to_string().contains("rt-old"));
        let down = AnthropicApi::with_base("http://127.0.0.1:1").refresh(SLOT, 0).unwrap_err();
        assert!(matches!(down, RefreshError::Transient { may_have_consumed: false, .. }), "{down:?}");
    }
}
