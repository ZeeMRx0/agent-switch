//! OpenAI (ChatGPT and Codex) login: who owns an `auth.json`, read from its `id_token` claims, plus the
//! usage and token-refresh endpoints.
//!
//! * Usage: `GET https://chatgpt.com/backend-api/wham/usage` with the `ChatGPT-Account-Id` header, so a team
//!   workspace reports its own quota (caam `internal/usage/codex.go:127-262`). `rate_limit.primary_window`
//!   and `.secondary_window` carry `used_percent`, `limit_window_seconds` and `reset_at` (Unix seconds).
//! * Refresh: `POST https://auth.openai.com/oauth/token`, JSON `{client_id, grant_type: "refresh_token",
//!   refresh_token, scope}` (caam `internal/refresh/codex.go:17-51`). Error classes follow caam
//!   `internal/refresh/errors.go:165-211`. The contract is written out in the phase 5 plan.
//!
//! The token claims are decoded without verifying the signature. That is fine for the purpose here: the
//! token comes from a file on this Mac and the result only labels the account for the user.

use serde_json::Value;

use crate::config::Account;
use crate::error::{Error, Result};
use crate::state::{WindowSnap, Windows};
use crate::token_refresh::RefreshError;
use crate::usage::{classify_usage_reply, http_get, http_post_json, Credential, FetchError};
use crate::vault::{lineage_fingerprint, normalize_blob};

pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const SCOPE: &str = "openid profile email";
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// What an `auth.json` says about its owner. Contains no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub email: Option<String>,
    /// `plus`, `team`, `pro`, ...
    pub plan: Option<String>,
    /// The ChatGPT workspace id (`chatgpt_account_id`). One person has a different id per workspace.
    pub account_id: String,
    pub user_id: Option<String>,
    /// Unix microseconds of `last_refresh`; the newest copy of one login has the largest value.
    pub last_refresh_us: Option<i64>,
    /// Identifies the refresh-token chain of this copy. It changes on every rotation.
    pub fingerprint: String,
}

impl Login {
    pub fn parse(auth_json: &str) -> Result<Login> {
        let v: serde_json::Value =
            serde_json::from_str(auth_json).map_err(|e| Error::Invalid(format!("auth.json is not valid JSON: {e}")))?;
        let tokens = v.get("tokens").filter(|t| t.is_object()).ok_or_else(|| {
            Error::Invalid("auth.json has no ChatGPT login (it may hold an API key; agent-switch manages ChatGPT logins only)".into())
        })?;
        let text = |k: &str| tokens.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty());
        let refresh = text("refresh_token").ok_or_else(|| Error::Invalid("auth.json has no refresh token".into()))?;
        let claims = text("id_token").map(jwt_claims).transpose()?.unwrap_or(serde_json::Value::Null);
        let auth = claims.get(AUTH_CLAIM);
        let claim = |k: &str| auth.and_then(|a| a.get(k)).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
        let account_id = text("account_id")
            .map(str::to_string)
            .or_else(|| claim("chatgpt_account_id"))
            .ok_or_else(|| Error::Invalid("auth.json names no ChatGPT account id".into()))?;
        Ok(Login {
            email: claims.get("email").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string),
            plan: claim("chatgpt_plan_type"),
            account_id,
            user_id: claim("chatgpt_user_id"),
            last_refresh_us: v.get("last_refresh").and_then(|x| x.as_str()).and_then(parse_rfc3339_us),
            fingerprint: lineage_fingerprint(refresh),
        })
    }

    /// True when `account` is this login: same workspace id and, when both sides know it, the same email.
    /// An account saved without a workspace id never matches, so a stray file cannot be adopted by mistake.
    pub fn is_account(&self, account: &Account) -> bool {
        if account.uuid.as_deref() != Some(self.account_id.as_str()) {
            return false;
        }
        match (&account.email, &self.email) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        }
    }

    /// True when both describe the same person in the same workspace.
    pub fn same_identity(&self, other: &Login) -> bool {
        self.account_id == other.account_id
            && match (&self.email, &other.email) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => true,
            }
    }
}

pub struct OpenaiApi {
    usage_url: String,
    token_url: String,
}

impl Default for OpenaiApi {
    fn default() -> Self {
        OpenaiApi { usage_url: USAGE_URL.to_string(), token_url: TOKEN_URL.to_string() }
    }
}

impl OpenaiApi {
    /// Point both endpoints at `base` (tests use a local server).
    pub fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        OpenaiApi { usage_url: format!("{base}/wham/usage"), token_url: format!("{base}/oauth/token") }
    }

    pub fn fetch_usage(&self, cred: &Credential) -> std::result::Result<Windows, FetchError> {
        let auth = format!("Bearer {}", cred.access_token);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
            ("User-Agent", concat!("agent-switch/", env!("CARGO_PKG_VERSION"))),
        ];
        // A header value with a control character would fail the whole request: no id is better than a bad one.
        if let Some(ws) = cred.workspace.as_deref().filter(|w| !w.is_empty() && !w.chars().any(char::is_control)) {
            headers.push(("ChatGPT-Account-Id", ws));
        }
        let reply = http_get(&self.usage_url, &headers).map_err(|kind| FetchError::Failed { kind, retry_after_s: None })?;
        classify_usage_reply(reply, parse_usage)
    }

    /// Exchange the refresh token of the saved `auth.json` text. Returns the whole new `auth.json` text.
    pub fn refresh(&self, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
        let rt = serde_json::from_str::<Value>(blob)
            .ok()
            .and_then(|v| v.pointer("/tokens/refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
            .ok_or_else(|| RefreshError::Dead("the saved login has no refresh token; sign in again".into()))?;
        let body = serde_json::json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": rt, "scope": SCOPE});
        let reply = http_post_json(&self.token_url, &body)
            .map_err(|kind| RefreshError::Transient { may_have_consumed: kind == "timeout", detail: kind })?;
        if reply.status != 200 {
            return Err(classify_refresh(reply.status, &reply.body));
        }
        let resp: Value = serde_json::from_str(&reply.body)
            .map_err(|_| RefreshError::Transient { detail: "the token endpoint answered with something unreadable".into(), may_have_consumed: true })?;
        apply_refresh(blob, &resp, now).map_err(|e| RefreshError::Transient { detail: e.to_string(), may_have_consumed: true })
    }
}

/// The access token, its expiry (the `exp` claim of the access-token JWT), the workspace id and the
/// refresh-token fingerprint of a saved `auth.json`.
pub fn credential(blob: &str) -> Result<Credential> {
    let login = Login::parse(blob)?;
    let v: Value = serde_json::from_str(blob).map_err(|e| Error::Invalid(e.to_string()))?;
    let access = v
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Invalid("auth.json has no access token".into()))?;
    let expires_at = jwt_claims(access).ok().and_then(|c| c.get("exp").and_then(Value::as_u64));
    Ok(Credential { access_token: access.to_string(), expires_at, workspace: Some(login.account_id), fingerprint: login.fingerprint })
}

/// Read the `wham/usage` body. The window length says which window is which (about 5 hours or about a week);
/// without it, primary is the 5-hour window and secondary the weekly one.
pub fn parse_usage(body: &Value) -> std::result::Result<Windows, FetchError> {
    let bad = || FetchError::Failed { kind: "bad-response".into(), retry_after_s: None };
    let limit = body.get("rate_limit").filter(|l| l.is_object()).ok_or_else(bad)?;
    let read = |key: &str| -> std::result::Result<Option<(WindowSnap, Option<u64>)>, FetchError> {
        let Some(w) = limit.get(key).filter(|w| !w.is_null()) else { return Ok(None) };
        let pct = w.get("used_percent").and_then(Value::as_f64).filter(|p| p.is_finite() && *p >= 0.0).ok_or_else(bad)?;
        let resets_at = w.get("reset_at").and_then(Value::as_i64);
        Ok(Some((WindowSnap { pct, resets_at }, w.get("limit_window_seconds").and_then(Value::as_u64))))
    };
    let mut out = Windows::default();
    for (key, default_is_five_hour) in [("primary_window", true), ("secondary_window", false)] {
        let Some((snap, secs)) = read(key)? else { continue };
        let five_hour = secs.map_or(default_is_five_hour, |s| s <= 6 * 3600);
        let slot = if five_hour { &mut out.five_hour } else { &mut out.weekly };
        slot.get_or_insert(snap);
    }
    if out.five_hour.is_none() && out.weekly.is_none() {
        return Err(bad());
    }
    Ok(out)
}

fn oauth_error_code(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let e = v.get("error")?;
    let code = e.as_str().or_else(|| e.get("code").and_then(Value::as_str))?;
    let ok = !code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    ok.then(|| code.to_string())
}

/// Sort a non-200 answer of the token endpoint into the three classes (caam `errors.go:165-178`): a 401 is a
/// refusal whatever the body says (the Codex CLI treats it as permanent too); a 403 counts only with an OAuth
/// error code, because an edge proxy can answer 403 with a challenge page; a 400 counts only when the provider
/// names a grant error. `invalid_client` is ours to fix and never quarantines an account.
pub fn classify_refresh(status: u16, body: &str) -> RefreshError {
    let code = oauth_error_code(body);
    if code.as_deref() == Some("invalid_client") {
        return RefreshError::Systemic("the token endpoint rejected agent-switch's client (invalid_client)".into());
    }
    let rejected = match status {
        401 => true,
        403 => code.is_some(),
        400 => code.as_deref().is_some_and(|c| c == "invalid_grant" || c.starts_with("refresh_token_")),
        _ => false,
    };
    if rejected {
        let why = code.map(|c| format!(" ({c})")).unwrap_or_default();
        return RefreshError::Dead(format!("the refresh token was rejected{why}; sign in to this account again"));
    }
    RefreshError::Transient { detail: format!("the token endpoint answered HTTP {status}"), may_have_consumed: status >= 500 }
}

/// Merge a successful token response into the saved `auth.json`: new access token, and the id token and the
/// rotated refresh token when sent; `last_refresh` becomes `now`. Every other key stays.
pub fn apply_refresh(blob: &str, resp: &Value, now: u64) -> Result<String> {
    let mut v: Value = serde_json::from_str(blob).map_err(|e| Error::Invalid(e.to_string()))?;
    let access = resp.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| Error::Invalid("the token response has no access token".into()))?;
    let tokens = v.get_mut("tokens").and_then(Value::as_object_mut).ok_or_else(|| Error::Invalid("auth.json has no tokens".into()))?;
    tokens.insert("access_token".into(), access.into());
    for key in ["id_token", "refresh_token"] {
        if let Some(t) = resp.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            tokens.insert(key.into(), t.into());
        }
    }
    v["last_refresh"] = rfc3339_utc(now).into();
    normalize_blob(&serde_json::to_string(&v).map_err(|e| Error::Invalid(e.to_string()))?)
}

/// Unix seconds to `2026-10-27T07:32:17.000000Z`, the form Codex writes into `last_refresh`.
pub fn rfc3339_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000000Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn jwt_claims(jwt: &str) -> Result<serde_json::Value> {
    let payload = jwt.split('.').nth(1).ok_or_else(|| Error::Invalid("id_token is not a JWT".into()))?;
    let bytes = b64url_decode(payload).ok_or_else(|| Error::Invalid("id_token payload is not base64".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("id_token payload is not JSON".into()))
}

/// Decode base64 in either alphabet (standard or URL-safe), with or without padding.
pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// `2026-10-27T07:32:17.030949Z` (or with a `+hh:mm` offset) to Unix microseconds.
pub fn parse_rfc3339_us(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let split = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(split);
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut t = hms.split(':');
    let (h, mi, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let micros: i64 = format!("{frac:0<6}")[..6].parse().ok()?;
    let offset_min = match zone.chars().next() {
        None | Some('Z') | Some('z') => 0,
        Some(sign @ ('+' | '-')) => {
            let (oh, om) = zone[1..].split_once(':')?;
            let total = oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?;
            if sign == '+' { total } else { -total }
        }
        _ => return None,
    };
    // Days since 1970-01-01 (Howard Hinnant's civil-from-days algorithm, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((((days * 24 + h) * 60 + mi - offset_min) * 60 + sec) * 1_000_000 + micros)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn b64url(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    /// A syntactically valid, unsigned JWT with the claims the real id_token carries.
    pub fn fake_jwt(email: &str, account_id: &str, plan: &str) -> String {
        let claims = serde_json::json!({
            "email": email,
            "exp": 1_893_456_000,
            AUTH_CLAIM: {"chatgpt_account_id": account_id, "chatgpt_plan_type": plan, "chatgpt_user_id": "user-1"}
        });
        format!("{}.{}.c2ln", b64url(br#"{"alg":"none"}"#), b64url(claims.to_string().as_bytes()))
    }

    /// An `auth.json` text like Codex writes, with a distinct refresh token per `rt`.
    pub fn auth_json(email: &str, account_id: &str, plan: &str, rt: &str, last_refresh: &str) -> String {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": fake_jwt(email, account_id, plan),
                "access_token": fake_jwt(email, account_id, plan),
                "refresh_token": rt,
                "account_id": account_id
            },
            "last_refresh": last_refresh
        })
        .to_string()
    }

    #[test]
    fn reads_identity_from_the_id_token_claims() {
        let l = Login::parse(&auth_json("Zee@Example.com", "acct-1", "team", "rt-1", "2026-10-27T07:32:17.030949Z")).unwrap();
        assert_eq!(l.email.as_deref(), Some("Zee@Example.com"));
        assert_eq!((l.plan.as_deref(), l.account_id.as_str(), l.user_id.as_deref()), (Some("team"), "acct-1", Some("user-1")));
        assert_eq!(l.fingerprint, lineage_fingerprint("rt-1"));
        assert_eq!(l.last_refresh_us, Some(1_793_086_337_030_949));
    }

    #[test]
    fn account_id_falls_back_to_the_claim_and_rejects_non_chatgpt_files() {
        let mut v: serde_json::Value = serde_json::from_str(&auth_json("a@b.c", "acct-9", "plus", "rt", "2026-01-01T00:00:00Z")).unwrap();
        v["tokens"].as_object_mut().unwrap().remove("account_id");
        assert_eq!(Login::parse(&v.to_string()).unwrap().account_id, "acct-9");
        assert!(Login::parse(r#"{"OPENAI_API_KEY":"sk-x"}"#).is_err());
        assert!(Login::parse("nope").is_err());
        assert!(Login::parse(r#"{"tokens":{"id_token":"a.b.c","refresh_token":"r"}}"#).is_err());
    }

    #[test]
    fn matches_an_account_by_workspace_id_and_email() {
        let l = Login::parse(&auth_json("zee@x.com", "acct-1", "team", "rt", "2026-01-01T00:00:00Z")).unwrap();
        let acct = |uuid: Option<&str>, email: Option<&str>| Account {
            id: "a".into(),
            provider: crate::config::Provider::Openai,
            label: "a".into(),
            email: email.map(str::to_string),
            uuid: uuid.map(str::to_string),
        };
        assert!(l.is_account(&acct(Some("acct-1"), Some("ZEE@x.com"))));
        assert!(!l.is_account(&acct(Some("acct-2"), Some("zee@x.com"))));
        assert!(!l.is_account(&acct(Some("acct-1"), Some("other@x.com"))));
        assert!(!l.is_account(&acct(None, Some("zee@x.com"))));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_rfc3339_us("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_us("2000-03-01T00:00:00.5Z"), Some(951_868_800_500_000));
        assert_eq!(parse_rfc3339_us("2026-11-06T10:04:59+07:00"), parse_rfc3339_us("2026-11-06T03:04:59Z"));
        assert!(parse_rfc3339_us("garbage").is_none());
        assert!(parse_rfc3339_us("2026-13-01T00:00:00Z").is_none());
        assert!(parse_rfc3339_us("2026-11-06T03:04:59.469401Z") > parse_rfc3339_us("2026-11-06T03:04:59.469400Z"));
    }

    // ----- usage and refresh -----

    use crate::usage::testkit::serve;

    const WHAM: &str = r#"{"plan_type":"team","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_after_seconds":1,"reset_at":1793958795},"secondary_window":{"used_percent":34,"limit_window_seconds":604800,"reset_after_seconds":2,"reset_at":1794545595}},"credits":null}"#;

    fn cred() -> Credential {
        Credential { access_token: "tok-1".into(), expires_at: None, workspace: Some("ws-1".into()), fingerprint: "f".into() }
    }

    #[test]
    fn parses_primary_as_five_hour_and_secondary_as_weekly_by_window_length() {
        let w = parse_usage(&serde_json::from_str(WHAM).unwrap()).unwrap();
        assert_eq!((w.five_hour.unwrap().pct, w.five_hour.unwrap().resets_at), (12.0, Some(1_793_958_795)));
        assert_eq!((w.weekly.unwrap().pct, w.weekly.unwrap().resets_at), (34.0, Some(1_794_545_595)));
        // A plan with only a weekly window reports it as primary: the length decides.
        let only_week = serde_json::json!({"rate_limit": {"primary_window": {"used_percent": 5, "limit_window_seconds": 604800, "reset_at": 9}}});
        let w = parse_usage(&only_week).unwrap();
        assert!(w.five_hour.is_none() && w.weekly.unwrap().pct == 5.0);
        // No length: primary is the 5-hour window.
        let bare = serde_json::json!({"rate_limit": {"primary_window": {"used_percent": 1}, "secondary_window": {"used_percent": 2}}});
        let w = parse_usage(&bare).unwrap();
        assert_eq!((w.five_hour.unwrap().pct, w.weekly.unwrap().pct), (1.0, 2.0));
    }

    #[test]
    fn a_body_without_a_measured_window_is_an_error() {
        for bad in [serde_json::json!({}), serde_json::json!({"rate_limit": null}), serde_json::json!({"rate_limit": {}}), serde_json::json!({"rate_limit": {"primary_window": {"used_percent": "x"}}})] {
            assert!(parse_usage(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn usage_request_sends_the_workspace_header() {
        let (base, req) = serve("200 OK", WHAM);
        let w = OpenaiApi::with_base(&base).fetch_usage(&cred()).unwrap();
        assert_eq!(w.weekly.unwrap().pct, 34.0);
        let raw = req.join().unwrap().to_lowercase();
        assert!(raw.starts_with("get /wham/usage "), "{raw}");
        assert!(raw.contains("authorization: bearer tok-1") && raw.contains("chatgpt-account-id: ws-1"), "{raw}");
    }

    #[test]
    fn usage_401_means_the_token_was_refused() {
        let (base, _r) = serve("401 Unauthorized", "{}");
        assert_eq!(OpenaiApi::with_base(&base).fetch_usage(&cred()).unwrap_err(), FetchError::Unauthorized);
    }

    #[test]
    fn credential_reads_expiry_from_the_access_token_and_the_workspace() {
        let blob = auth_json("a@b.c", "ws-9", "team", "rt-1", "2026-10-27T07:32:17Z");
        let c = credential(&blob).unwrap();
        assert_eq!((c.expires_at, c.workspace.as_deref()), (Some(1_893_456_000), Some("ws-9")));
        assert_eq!(c.fingerprint, lineage_fingerprint("rt-1"));
    }

    #[test]
    fn refresh_posts_the_documented_body_and_merges_the_answer() {
        let blob = auth_json("a@b.c", "ws-9", "team", "rt-1", "2026-10-27T07:32:17Z");
        let (base, req) = serve("200 OK", r#"{"access_token":"at-new","id_token":"idt-new","refresh_token":"rt-2","expires_in":864000}"#);
        let out = OpenaiApi::with_base(&base).refresh(&blob, 1_793_086_337).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tokens"]["access_token"], "at-new");
        assert_eq!(v["tokens"]["id_token"], "idt-new");
        assert_eq!(v["tokens"]["refresh_token"], "rt-2");
        assert_eq!(v["tokens"]["account_id"], "ws-9", "other keys stay");
        assert_eq!(v["auth_mode"], "chatgpt");
        assert_eq!(v["last_refresh"], "2026-10-27T07:32:17.000000Z");
        let raw = req.join().unwrap();
        assert!(raw.starts_with("POST /oauth/token "), "{raw}");
        let sent: Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(sent, serde_json::json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": "rt-1", "scope": SCOPE}));
    }

    #[test]
    fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
        let blob = auth_json("a@b.c", "ws-9", "team", "rt-1", "2026-10-27T07:32:17Z");
        let out = apply_refresh(&blob, &serde_json::json!({"access_token": "x"}), 5).unwrap();
        assert_eq!(Login::parse(&out).unwrap().fingerprint, lineage_fingerprint("rt-1"));
    }

    #[test]
    fn refresh_errors_fall_into_dead_systemic_and_transient() {
        use RefreshError::*;
        assert!(matches!(classify_refresh(401, ""), Dead(_)), "a 401 is a refusal whatever the body says");
        assert!(matches!(classify_refresh(401, r#"{"error":{"code":"refresh_token_reused"}}"#), Dead(_)));
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_grant"}"#), Dead(_)));
        assert!(matches!(classify_refresh(400, r#"{"error":{"code":"refresh_token_expired"}}"#), Dead(_)));
        assert!(matches!(classify_refresh(403, r#"{"error":"access_denied"}"#), Dead(_)));
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_client"}"#), Systemic(_)));
        assert!(matches!(classify_refresh(401, r#"{"error":"invalid_client"}"#), Systemic(_)), "client errors never quarantine");
        assert!(matches!(classify_refresh(403, "<html>challenge</html>"), Transient { .. }));
        assert!(matches!(classify_refresh(400, r#"{"error":"invalid_request"}"#), Transient { .. }));
        assert!(matches!(classify_refresh(429, "{}"), Transient { may_have_consumed: false, .. }));
        assert!(matches!(classify_refresh(503, "{}"), Transient { may_have_consumed: true, .. }));
    }

    #[test]
    fn timestamps_format_and_parse_back() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000000Z");
        for secs in [951_868_800u64, 1_793_086_337, 1_800_000_000, 4_102_444_800] {
            assert_eq!(parse_rfc3339_us(&rfc3339_utc(secs)), Some(secs as i64 * 1_000_000), "{secs}");
        }
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00.000000Z");
    }
}
