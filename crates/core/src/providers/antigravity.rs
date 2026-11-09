//! Google Antigravity (`agy`) login: the Keychain item format, the token refresh and the quota endpoint.
//!
//! * Login: `agy` keeps one Keychain generic password, service `gemini`, account `antigravity`, whose value is
//!   `go-keyring-base64:` followed by base64 of a JSON object: `{"token": {access_token, token_type,
//!   refresh_token, expiry (RFC 3339 with offset), expires_in}, "auth_method", "id_token_email"}`. The vault
//!   slot holds that JSON as text; every key, known or not, is kept as it is.
//! * Refresh: `POST https://oauth2.googleapis.com/token`, form `{client_id, client_secret, refresh_token,
//!   grant_type=refresh_token}` with Antigravity's installed-app client. Google does not rotate the refresh
//!   token: the reply has a new `access_token` and `expires_in` and usually no `refresh_token`, so the old one
//!   is kept and the chain fingerprint stays the same.
//! * Usage: `POST https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels`, body `{}`. Each model
//!   reports `quotaInfo {remainingFraction, resetTime}`; models without `resetTime` have no limit and are
//!   ignored; a body with only such models is a reading with no windows (no limit). Models sharing a quota
//!   group report the same numbers. A model whose reset is more than six hours away counts as a weekly window
//!   (the extra hour absorbs clock skew on a fresh five-hour window), any other as a five-hour window; each
//!   window shows its most-used model. In the last six hours before a weekly reset that bucket shows as the
//!   five-hour window, which `Windows::worst_at` and the thresholds treat the same. A model with an unusable
//!   `remainingFraction` is skipped.
//!
//! Tokens only ever travel in a request header or body; error text never includes them.

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::providers::openai::{b64url_decode, parse_rfc3339_us, rfc3339_utc};
use crate::state::{now_secs, WindowSnap, Windows};
use crate::token_refresh::RefreshError;
use crate::usage::{classify_usage_reply, http_post_form, http_post_usage, Credential, FetchError};
use crate::vault::{lineage_fingerprint, normalize_blob};

pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Antigravity's installed-app OAuth client (the one `agy` uses). An `agy` release that changes it needs only
/// this pair updated.
pub const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "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";
pub const QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
/// The `agy` release whose User-Agent the quota request sends.
const AGY_VERSION: &str = "1.5.1";
/// The prefix go-keyring puts before the base64 text of the value.
pub const KEYRING_PREFIX: &str = "go-keyring-base64:";
/// A window whose reset is further away than this is the weekly window. Six hours rather than five, so a
/// fresh five-hour window still counts as one when the local clock is behind the server's.
const WEEKLY_AFTER_S: i64 = 6 * 3600;

type Obj = Map<String, Value>;

fn format_error(detail: &str) -> Error {
    Error::Invalid(format!(
        "the Antigravity login has an unexpected format ({detail}); agent-switch will not touch it. Update agent-switch if agy changed its storage"
    ))
}

/// The Keychain value `agy` writes, decoded to the login JSON text (printable ASCII, ready for the vault).
/// A plain JSON value is accepted too.
pub fn decode_item(text: &str) -> Result<String> {
    let text = text.trim();
    let json = match text.strip_prefix(KEYRING_PREFIX) {
        Some(b64) => {
            let bytes = b64url_decode(b64.trim()).ok_or_else(|| format_error("not base64"))?;
            String::from_utf8(bytes).map_err(|_| format_error("not UTF-8"))?
        }
        None if text.starts_with('{') => text.to_string(),
        None => return Err(format_error("no go-keyring-base64 prefix")),
    };
    let blob = normalize_blob(&json).map_err(|_| format_error("not JSON"))?;
    Login::parse(&blob)?;
    Ok(blob)
}

/// The Keychain value for a login JSON text, in the format `agy` reads.
pub fn encode_item(blob: &str) -> String {
    format!("{KEYRING_PREFIX}{}", b64_encode(blob.as_bytes()))
}

/// Standard base64 with padding, as Go's `base64.StdEncoding` writes it.
fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |a, (i, b)| a | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[((n >> (18 - 6 * i)) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn parse_obj(blob: &str) -> Result<Obj> {
    match serde_json::from_str::<Value>(blob) {
        Ok(Value::Object(o)) => Ok(o),
        _ => Err(format_error("not a JSON object")),
    }
}

fn token_text<'a>(login: &'a Obj, key: &str) -> Option<&'a str> {
    login.get("token").and_then(|t| t.get(key)).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// What a login says about its owner. Contains no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// `id_token_email`.
    pub email: Option<String>,
    /// Fingerprint of the refresh token. Google does not rotate it, so it names the sign-in.
    pub fingerprint: String,
}

impl Login {
    pub fn parse(blob: &str) -> Result<Login> {
        let login = parse_obj(blob)?;
        let refresh = token_text(&login, "refresh_token").ok_or_else(|| format_error("no token.refresh_token"))?;
        let email = login.get("id_token_email").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Login { email, fingerprint: lineage_fingerprint(refresh) })
    }
}

/// The access token, its expiry (`token.expiry`) and the refresh-token fingerprint of a saved login.
pub fn credential(blob: &str) -> Result<Credential> {
    let login = parse_obj(blob)?;
    let (Some(access), Some(refresh)) = (token_text(&login, "access_token"), token_text(&login, "refresh_token")) else {
        return Err(Error::Invalid("saved Antigravity login has no access or refresh token".into()));
    };
    let expires_at = token_text(&login, "expiry").and_then(parse_rfc3339_us).and_then(|us| u64::try_from(us.div_euclid(1_000_000)).ok());
    Ok(Credential { access_token: access.to_string(), expires_at, workspace: None, fingerprint: lineage_fingerprint(refresh) })
}

fn user_agent() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("antigravity/{AGY_VERSION} darwin/{arch}")
}

pub struct AntigravityApi {
    quota_url: String,
    token_url: String,
}

impl Default for AntigravityApi {
    fn default() -> Self {
        AntigravityApi { quota_url: QUOTA_URL.to_string(), token_url: TOKEN_URL.to_string() }
    }
}

impl AntigravityApi {
    /// Point both endpoints at `base` (tests use a local server).
    pub fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        AntigravityApi { quota_url: format!("{base}/v1internal:fetchAvailableModels"), token_url: format!("{base}/token") }
    }

    pub fn fetch_usage(&self, cred: &Credential) -> std::result::Result<Windows, FetchError> {
        let auth = format!("Bearer {}", cred.access_token);
        let ua = user_agent();
        let headers = [("Authorization", auth.as_str()), ("Accept", "application/json"), ("User-Agent", ua.as_str())];
        let reply = http_post_usage(&self.quota_url, &headers, &Value::Object(Obj::new()))
            .map_err(|kind| FetchError::Failed { kind, retry_after_s: None })?;
        let now = now_secs() as i64;
        classify_usage_reply(reply, |body| parse_usage(body, now))
    }

    /// Exchange the refresh token of the saved login `blob`. Returns the whole new login text.
    pub fn refresh(&self, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
        let refresh_token = parse_obj(blob)
            .ok()
            .and_then(|o| token_text(&o, "refresh_token").map(str::to_string))
            .ok_or_else(|| RefreshError::Dead("the saved login has no refresh token; sign in again".into()))?;
        let form = [("client_id", CLIENT_ID), ("client_secret", CLIENT_SECRET), ("refresh_token", refresh_token.as_str()), ("grant_type", "refresh_token")];
        let reply = http_post_form(&self.token_url, &form)
            .map_err(|kind| RefreshError::Transient { may_have_consumed: false, detail: kind })?;
        if reply.status != 200 {
            return Err(classify_refresh(reply.status, &reply.body));
        }
        let resp: Value = serde_json::from_str(&reply.body)
            .map_err(|_| RefreshError::Transient { detail: "the token endpoint answered with something unreadable".into(), may_have_consumed: false })?;
        apply_refresh(blob, &resp, now).map_err(|e| RefreshError::Transient { detail: e.to_string(), may_have_consumed: false })
    }
}

/// Read the `fetchAvailableModels` body at `now` (Unix seconds). No limited model at all is a reading with no
/// windows; limited models that are all unusable are a bad response.
pub fn parse_usage(body: &Value, now: i64) -> std::result::Result<Windows, FetchError> {
    let bad = || FetchError::Failed { kind: "bad-response".into(), retry_after_s: None };
    let models = body.get("models").and_then(Value::as_object).ok_or_else(bad)?;
    let mut out = Windows::default();
    let mut skipped = false;
    for model in models.values() {
        let Some(quota) = model.get("quotaInfo").and_then(Value::as_object) else { continue };
        let Some(resets_at) = quota.get("resetTime").and_then(Value::as_str).and_then(parse_rfc3339_us).map(|us| us.div_euclid(1_000_000)) else {
            continue;
        };
        let remaining = match quota.get("remainingFraction") {
            // Proto3 JSON leaves out zero values, so an exhausted quota arrives without the field.
            None | Some(Value::Null) => 0.0,
            Some(v) => match v.as_f64().filter(|f| (0.0..=1.0).contains(f)) {
                Some(f) => f,
                None => {
                    skipped = true;
                    continue;
                }
            },
        };
        let pct = (1.0 - remaining) * 100.0;
        let slot = if resets_at - now > WEEKLY_AFTER_S { &mut out.weekly } else { &mut out.five_hour };
        if slot.is_none_or(|s| pct > s.pct) {
            *slot = Some(WindowSnap { pct, resets_at: Some(resets_at) });
        }
    }
    if skipped && out.five_hour.is_none() && out.weekly.is_none() {
        return Err(bad());
    }
    Ok(out)
}

/// Sort a non-200 answer of Google's token endpoint into the three classes. `invalid_grant` is a dead
/// sign-in; `invalid_client` and `unauthorized_client` mean the client pair is wrong, which says nothing
/// about the account. Anything else is transient. Google keeps the refresh token, so a failed refresh never
/// spends it (`may_have_consumed` is false).
pub fn classify_refresh(status: u16, body: &str) -> RefreshError {
    if matches!(status, 400 | 401 | 403) {
        let code = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        match code.as_deref() {
            Some("invalid_grant") => return RefreshError::Dead("the refresh token was rejected (invalid_grant); sign in to this account again in agy".into()),
            Some(c @ ("invalid_client" | "unauthorized_client")) => {
                return RefreshError::Systemic(format!("the token endpoint rejected agent-switch's Antigravity client ({c})"))
            }
            _ => {}
        }
    }
    RefreshError::Transient { detail: format!("the token endpoint answered HTTP {status}"), may_have_consumed: false }
}

/// Merge a successful token response into the saved login: new access token, `expires_in` and `expiry`, and
/// the refresh token only when the server sent one. Every other key stays.
pub fn apply_refresh(blob: &str, resp: &Value, now: u64) -> Result<String> {
    let mut login = parse_obj(blob)?;
    let access = resp.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty());
    let expires_in = resp.get("expires_in").and_then(Value::as_u64);
    let (Some(access), Some(expires_in)) = (access, expires_in) else {
        return Err(Error::Invalid("the token response has no access token or expiry".into()));
    };
    let token = login.get_mut("token").and_then(Value::as_object_mut).ok_or_else(|| format_error("no token object"))?;
    token.insert("access_token".into(), access.into());
    token.insert("expires_in".into(), expires_in.into());
    token.insert("expiry".into(), rfc3339_utc(now + expires_in).into());
    if let Some(rt) = resp.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        token.insert("refresh_token".into(), rt.into());
    }
    normalize_blob(&serde_json::to_string(&login).map_err(|e| Error::Invalid(e.to_string()))?)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::usage::testkit::serve;

    /// A login JSON like `agy` writes, with fake tokens.
    pub fn login_json(email: &str, access: &str, refresh: &str, expiry: &str) -> String {
        serde_json::json!({
            "token": {"access_token": access, "token_type": "Bearer", "refresh_token": refresh, "expiry": expiry, "expires_in": 3599},
            "auth_method": "consumer",
            "id_token_email": email
        })
        .to_string()
    }

    /// The Keychain value `agy` would store for that login.
    pub fn item(email: &str, access: &str, refresh: &str, expiry: &str) -> String {
        encode_item(&login_json(email, access, refresh, expiry))
    }

    const NOW: i64 = 1_793_000_000;

    fn at(offset_s: i64) -> String {
        rfc3339_utc((NOW + offset_s) as u64)
    }

    #[test]
    fn the_keychain_value_round_trips_and_keeps_unknown_keys() {
        let mut v: Value = serde_json::from_str(&login_json("me@gmail.com", "ya29.fake", "1//rt-fake", "2026-11-06T16:58:16.636636+07:00")).unwrap();
        v["future_field"] = serde_json::json!({"x": 1});
        let blob = v.to_string();
        let text = encode_item(&blob);
        assert!(text.starts_with("go-keyring-base64:") && !text.contains("ya29"), "{text}");
        let back = decode_item(&text).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&back).unwrap(), v);
        // Padding of every length decodes, and a plain JSON value is accepted.
        for n in 0..3 {
            let b = format!("{{\"token\":{{\"refresh_token\":\"r{}\"}}}}", "x".repeat(n));
            assert_eq!(decode_item(&encode_item(&b)).unwrap(), b);
        }
        assert!(decode_item(&blob).is_ok());
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
        assert_eq!(b64_encode(b"M"), "TQ==");
    }

    #[test]
    fn values_that_are_not_an_agy_login_are_refused_without_echoing_them() {
        for bad in ["go-keyring-base64:@@@", "go-keyring-base64:", "hunter2", &encode_item(r#"{"token":{}}"#), &encode_item("[1]")] {
            let e = decode_item(bad).unwrap_err().to_string();
            assert!(e.contains("unexpected format") && !e.contains("hunter2"), "{e}");
        }
    }

    #[test]
    fn credential_reads_the_expiry_with_its_offset_and_the_fingerprint() {
        let blob = login_json("me@gmail.com", "ya29.fake", "1//rt-fake", "2026-11-06T16:58:16.636636+07:00");
        let c = credential(&blob).unwrap();
        let utc = parse_rfc3339_us("2026-11-06T09:58:16Z").unwrap() / 1_000_000;
        assert_eq!(c.expires_at, Some(utc as u64));
        assert_eq!(c.fingerprint, lineage_fingerprint("1//rt-fake"));
        assert_eq!(c.access_token, "ya29.fake");
        assert!(!format!("{c:?}").contains("ya29"));
        let login = Login::parse(&blob).unwrap();
        assert_eq!((login.email.as_deref(), login.fingerprint), (Some("me@gmail.com"), lineage_fingerprint("1//rt-fake")));
    }

    /// The shape recorded by the live probe (free tier): a Gemini group and a Claude/GPT group, both weekly,
    /// plus tab models without a reset.
    fn models(gemini_left: Option<f64>, gemini_reset: &str, claude_left: f64, claude_reset: &str) -> Value {
        let mut gemini = serde_json::json!({"resetTime": gemini_reset});
        if let Some(f) = gemini_left {
            gemini["remainingFraction"] = f.into();
        }
        serde_json::json!({
            "models": {
                "gemini-3-pro": {"displayName": "Gemini 3 Pro", "quotaInfo": gemini.clone()},
                "gemini-3-flash": {"displayName": "Gemini 3 Flash", "quotaInfo": gemini},
                "claude-sonnet-4-5": {"displayName": "Claude Sonnet 4.5", "quotaInfo": {"remainingFraction": claude_left, "resetTime": claude_reset}},
                "gpt-oss-120b": {"quotaInfo": {"remainingFraction": claude_left, "resetTime": claude_reset}},
                "tab_flash_lite": {"quotaInfo": {"remainingFraction": 1}},
                "chat_20706": {}
            },
            "defaultAgentModelId": "gemini-3-pro"
        })
    }

    #[test]
    fn two_weekly_groups_show_the_most_used_one_as_the_weekly_window() {
        let w = parse_usage(&models(Some(0.996_751_2), &at(157 * 3600), 1.0, &at(168 * 3600)), NOW).unwrap();
        assert!(w.five_hour.is_none());
        let week = w.weekly.unwrap();
        assert!((week.pct - 0.324_88).abs() < 1e-3, "{}", week.pct);
        assert_eq!(week.resets_at, Some(NOW + 157 * 3600));
    }

    #[test]
    fn a_reset_within_six_hours_is_the_five_hour_window_and_the_other_class_fills_weekly() {
        let w = parse_usage(&models(Some(0.25), &at(2 * 3600), 0.9, &at(100 * 3600)), NOW).unwrap();
        assert_eq!(w.five_hour, Some(WindowSnap { pct: 75.0, resets_at: Some(NOW + 2 * 3600) }));
        let week = w.weekly.unwrap();
        assert!((week.pct - 10.0).abs() < 1e-9 && week.resets_at == Some(NOW + 100 * 3600));
        // Five hours and a bit (a clock behind the server's) is still the five-hour window; past six hours is weekly.
        assert!(parse_usage(&models(Some(0.5), &at(5 * 3600 + 30), 0.5, &at(6 * 3600)), NOW).unwrap().weekly.is_none());
        assert!(parse_usage(&models(Some(0.5), &at(6 * 3600 + 1), 0.5, &at(6 * 3600 + 1)), NOW).unwrap().five_hour.is_none());
    }

    #[test]
    fn a_missing_remaining_fraction_means_the_quota_is_used_up() {
        let w = parse_usage(&models(None, &at(150 * 3600), 1.0, &at(168 * 3600)), NOW).unwrap();
        assert_eq!(w.weekly.unwrap().pct, 100.0);
    }

    #[test]
    fn a_body_without_models_or_with_only_unusable_limits_is_an_error() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"models": []}),
            serde_json::json!({"models": {"m": {"quotaInfo": {"remainingFraction": "x", "resetTime": at(10)}}}}),
            serde_json::json!({"models": {"m": {"quotaInfo": {"remainingFraction": 1.5, "resetTime": at(10)}}}}),
        ] {
            assert!(parse_usage(&bad, NOW).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_body_without_a_limited_model_is_a_reading_with_no_windows() {
        let body = serde_json::json!({"models": {"tab": {"quotaInfo": {"remainingFraction": 1}}, "other": {}}});
        assert_eq!(parse_usage(&body, NOW).unwrap(), Windows::default());
    }

    #[test]
    fn a_model_with_an_unusable_fraction_is_skipped_and_the_others_still_count() {
        let mut body = models(Some(0.4), &at(100 * 3600), 0.8, &at(2 * 3600));
        body["models"]["odd"] = serde_json::json!({"quotaInfo": {"remainingFraction": "lots", "resetTime": at(3600)}});
        body["models"]["negative"] = serde_json::json!({"quotaInfo": {"remainingFraction": -0.5, "resetTime": at(3600)}});
        let w = parse_usage(&body, NOW).unwrap();
        assert!((w.weekly.unwrap().pct - 60.0).abs() < 1e-9);
        assert!((w.five_hour.unwrap().pct - 20.0).abs() < 1e-9);
    }

    fn cred() -> Credential {
        Credential { access_token: "ya29.fake".into(), expires_at: None, workspace: None, fingerprint: "f".into() }
    }

    #[test]
    fn the_quota_request_is_a_json_post_with_the_bearer_token_and_an_agy_user_agent() {
        let reset = rfc3339_utc(now_secs() + 100 * 3600);
        let body = models(Some(0.5), &reset, 1.0, &reset).to_string();
        let (base, req) = serve("200 OK", &body);
        let w = AntigravityApi::with_base(&base).fetch_usage(&cred()).unwrap();
        assert_eq!(w.weekly.unwrap().pct, 50.0);
        let raw = req.join().unwrap();
        let lower = raw.to_lowercase();
        assert!(raw.starts_with("POST /v1internal:fetchAvailableModels "), "{raw}");
        assert!(lower.contains("authorization: bearer ya29.fake") && lower.contains("content-type: application/json"), "{raw}");
        assert!(lower.contains("user-agent: antigravity/1.5.1 darwin/"), "{raw}");
        assert_eq!(raw.split("\r\n\r\n").nth(1).unwrap(), "{}");
    }

    #[test]
    fn a_refused_token_is_unauthorized() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let (base, _r) = serve(status, "{}");
            assert_eq!(AntigravityApi::with_base(&base).fetch_usage(&cred()).unwrap_err(), FetchError::Unauthorized);
        }
    }

    #[test]
    fn refresh_posts_the_form_and_keeps_the_refresh_token_google_did_not_send() {
        let blob = login_json("me@gmail.com", "ya29.old", "1//rt-fake", "2026-11-06T16:58:16.636636+07:00");
        let (base, req) = serve("200 OK", r#"{"access_token":"ya29.new","expires_in":3599,"scope":"x","token_type":"Bearer","id_token":"eyJ.fake"}"#);
        let out = AntigravityApi::with_base(&base).refresh(&blob, NOW as u64).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token"]["access_token"], "ya29.new");
        assert_eq!(v["token"]["refresh_token"], "1//rt-fake");
        assert_eq!(v["token"]["expires_in"], 3599);
        assert_eq!(credential(&out).unwrap().expires_at, Some(NOW as u64 + 3599));
        assert_eq!((v["auth_method"].as_str(), v["id_token_email"].as_str()), (Some("consumer"), Some("me@gmail.com")), "other keys stay");
        assert!(v.get("id_token").is_none() && v["token"].get("id_token").is_none(), "the id token is not stored");
        let raw = req.join().unwrap();
        assert!(raw.starts_with("POST /token "), "{raw}");
        assert!(raw.to_lowercase().contains("content-type: application/x-www-form-urlencoded"), "{raw}");
        let sent = raw.split("\r\n\r\n").nth(1).unwrap();
        assert!(sent.contains("grant_type=refresh_token") && sent.contains("refresh_token=1%2F%2Frt-fake"), "{sent}");
        assert!(sent.contains(&format!("client_id={CLIENT_ID}")) && sent.contains("client_secret="), "{sent}");
    }

    #[test]
    fn a_rotated_refresh_token_is_taken_when_google_sends_one() {
        let blob = login_json("me@gmail.com", "ya29.old", "1//rt-a", "2026-11-06T16:58:16+07:00");
        let out = apply_refresh(&blob, &serde_json::json!({"access_token": "ya29.n", "expires_in": 10, "refresh_token": "1//rt-b"}), 5).unwrap();
        assert_eq!(Login::parse(&out).unwrap().fingerprint, lineage_fingerprint("1//rt-b"));
        assert!(apply_refresh(&blob, &serde_json::json!({"access_token": "x"}), 5).is_err(), "no expiry");
    }

    #[test]
    fn refresh_errors_fall_into_dead_systemic_and_transient() {
        use RefreshError::*;
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#), Dead(_)));
        assert!(matches!(classify_refresh(401, r#"{"error":"invalid_client"}"#), Systemic(_)));
        assert!(matches!(classify_refresh(401, r#"{"error":"unauthorized_client"}"#), Systemic(_)));
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_request"}"#), Transient { .. }));
        assert!(matches!(classify_refresh(403, "<html>"), Transient { .. }));
        assert!(matches!(classify_refresh(503, "{}"), Transient { may_have_consumed: false, .. }), "Google keeps the refresh token");
        assert!(matches!(classify_refresh(429, "{}"), Transient { may_have_consumed: false, .. }));
    }

    #[test]
    fn a_refresh_of_a_login_without_a_refresh_token_is_dead_without_a_request() {
        let r = AntigravityApi::with_base("http://127.0.0.1:9").refresh(r#"{"token":{"access_token":"a"}}"#, 1);
        assert!(matches!(r, Err(RefreshError::Dead(_))), "{r:?}");
    }
}
