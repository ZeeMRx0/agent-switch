//! Usage readings: what each account has used of its 5-hour and weekly windows.
//!
//! Where the login comes from decides whether a reading may refresh it:
//! * An account that is **active** on a surface that holds its token chain (`claude-cli`, `codex-cli`,
//!   `chatgpt-desktop`, `antigravity-cli`) is read **live** from that surface. The CLI or the app owns the
//!   refresh of that login, so agent-switch never refreshes it and reports `expired` when the access token
//!   has lapsed.
//! * Any other account is read from the vault. It is refreshed in the background only when the user opted in
//!   (`auto.refresh_inactive`); by default a lapsed token shows `expired` and the last-known numbers, and
//!   the login is refreshed just before a switch (see `token_refresh`).
//!
//! A failed reading keeps the last-known windows and their age (`UsageRecord`), so `list` and the menu bar
//! can show stale numbers with their age. Only a dead refresh token (`invalid_grant`) quarantines an account.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

use crate::config::{Account, Config, Provider, SurfaceKind};
use crate::error::{Error, Result};
use crate::policy;
use crate::providers::anthropic::AnthropicApi;
use crate::providers::antigravity::AntigravityApi;
use crate::providers::openai::OpenaiApi;
use crate::state::{State, UsageRecord, UsageState, Windows};
use crate::surface::{Env, Surface};
use crate::token_refresh::{self, FreshenError, RefreshError, FRESHEN_WITHIN_S};
use crate::vault::Slot;

/// Seconds a stored reading is served without a new request (one request per 3 minutes is the budget).
pub const SERVE_TTL_S: u64 = 180;

/// Treat an access token as lapsed this long before its stated expiry, so a request is not sent with a token
/// that dies in flight.
const EXPIRY_SKEW_S: u64 = 30;

/// The access token of one saved login. `Debug` hides the token.
#[derive(Clone)]
pub struct Credential {
    pub access_token: String,
    /// Unix seconds; `None` when the login does not say.
    pub expires_at: Option<u64>,
    /// ChatGPT workspace id, sent as `ChatGPT-Account-Id` so a team workspace reports its own quota.
    pub workspace: Option<String>,
    /// Fingerprint of the refresh-token chain (changes on every rotation).
    pub fingerprint: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Why a usage request produced no reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// HTTP 401 or 403: the endpoint refused the access token.
    Unauthorized,
    /// Anything else: `kind` is a short stable token such as `http-429`, `http-503`, `timeout`, `network` or
    /// `bad-response`; `retry_after_s` is the server's `Retry-After` when it sent one.
    Failed { kind: String, retry_after_s: Option<u64> },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Unauthorized => f.write_str("the access token was refused"),
            FetchError::Failed { kind, .. } => write!(f, "usage request failed ({kind})"),
        }
    }
}

/// Everything provider-specific the watcher needs. Tests use a scripted fake.
pub trait ProviderApi {
    /// Parse the access token and expiry out of a saved login.
    fn credential(&self, account: &Account, blob: &str) -> Result<Credential>;
    fn fetch_usage(&self, account: &Account, cred: &Credential) -> std::result::Result<Windows, FetchError>;
    /// Exchange the saved login's refresh token. Returns the whole new login blob (same shape as the input).
    /// `now` is Unix seconds, used for the new expiry.
    fn refresh(&self, account: &Account, blob: &str, now: u64) -> std::result::Result<String, RefreshError>;
}

/// The real endpoints.
pub struct LiveApi {
    anthropic: AnthropicApi,
    openai: OpenaiApi,
    antigravity: AntigravityApi,
}

impl LiveApi {
    pub fn production() -> LiveApi {
        LiveApi { anthropic: AnthropicApi::default(), openai: OpenaiApi::default(), antigravity: AntigravityApi::default() }
    }
}

impl ProviderApi for LiveApi {
    fn credential(&self, account: &Account, blob: &str) -> Result<Credential> {
        match account.provider {
            Provider::Claude => crate::providers::anthropic::credential(blob),
            Provider::Openai => crate::providers::openai::credential(blob),
            Provider::Antigravity => crate::providers::antigravity::credential(blob),
        }
    }

    fn fetch_usage(&self, account: &Account, cred: &Credential) -> std::result::Result<Windows, FetchError> {
        match account.provider {
            Provider::Claude => self.anthropic.fetch_usage(cred),
            Provider::Openai => self.openai.fetch_usage(cred),
            Provider::Antigravity => self.antigravity.fetch_usage(cred),
        }
    }

    fn refresh(&self, account: &Account, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
        match account.provider {
            Provider::Claude => self.anthropic.refresh(blob, now),
            Provider::Openai => self.openai.refresh(blob, now),
            Provider::Antigravity => self.antigravity.refresh(blob, now),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

pub(crate) struct Reply {
    pub status: u16,
    pub body: String,
    pub retry_after_s: Option<u64>,
}

/// A usage request waits at most this long (claude-swap `oauth.py:413-424` uses 5 s).
const GET_TIMEOUT: Duration = Duration::from_secs(5);
/// A refresh request runs while the account lock is held, so it must end before a contender's 10 s lock
/// timeout (claude-swap `oauth.py:165-168`).
const POST_TIMEOUT: Duration = Duration::from_secs(8);

/// These requests carry bearer tokens or refresh tokens, so they never follow a redirect: a redirect could
/// send the credential to a host we did not choose.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).max_redirects(0).build().into()
}

fn into_reply(mut resp: ureq::http::Response<ureq::Body>) -> std::result::Result<Reply, String> {
    let retry_after_s = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|_| "bad-response".to_string())?;
    Ok(Reply { status, body, retry_after_s })
}

/// A transport failure reduced to a short token; the error text could name a URL, so it is not kept.
fn transport_kind(e: &ureq::Error) -> String {
    if matches!(e, ureq::Error::Timeout(_)) { "timeout" } else { "network" }.to_string()
}

pub(crate) fn http_get(url: &str, headers: &[(&str, &str)]) -> std::result::Result<Reply, String> {
    let mut req = agent(GET_TIMEOUT).get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    into_reply(req.call().map_err(|e| transport_kind(&e))?)
}

pub(crate) fn http_post_json(url: &str, body: &Value) -> std::result::Result<Reply, String> {
    let req = agent(POST_TIMEOUT).post(url).header("User-Agent", concat!("agent-switch/", env!("CARGO_PKG_VERSION")));
    into_reply(req.send_json(body).map_err(|e| transport_kind(&e))?)
}

/// A token request with a form body (`application/x-www-form-urlencoded`), under the refresh timeout.
pub(crate) fn http_post_form(url: &str, form: &[(&str, &str)]) -> std::result::Result<Reply, String> {
    let req = agent(POST_TIMEOUT).post(url).header("User-Agent", concat!("agent-switch/", env!("CARGO_PKG_VERSION")));
    into_reply(req.send_form(form.iter().copied()).map_err(|e| transport_kind(&e))?)
}

/// A usage request that is a POST with a JSON body, under the usage timeout.
pub(crate) fn http_post_usage(url: &str, headers: &[(&str, &str)], body: &Value) -> std::result::Result<Reply, String> {
    let mut req = agent(GET_TIMEOUT).post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    into_reply(req.send_json(body).map_err(|e| transport_kind(&e))?)
}

/// Map an HTTP reply of the usage endpoint to a reading.
pub(crate) fn classify_usage_reply(reply: Reply, parse: impl FnOnce(&Value) -> std::result::Result<Windows, FetchError>) -> std::result::Result<Windows, FetchError> {
    match reply.status {
        200 => {
            let body: Value = serde_json::from_str(&reply.body).map_err(|_| FetchError::Failed { kind: "bad-response".into(), retry_after_s: None })?;
            parse(&body)
        }
        401 | 403 => Err(FetchError::Unauthorized),
        code => Err(FetchError::Failed { kind: format!("http-{code}"), retry_after_s: reply.retry_after_s }),
    }
}

// ---------------------------------------------------------------------------------------------
// One reading
// ---------------------------------------------------------------------------------------------

/// Gives the poller the surface adapters it may read live logins from. The CLI and the menu bar each have
/// one registry of adapters.
pub type SurfaceFactory<'a> = &'a dyn Fn(SurfaceKind) -> Box<dyn Surface>;

pub struct Ctx<'a> {
    pub env: &'a Env<'a>,
    pub config: &'a Config,
    pub api: &'a dyn ProviderApi,
    pub surfaces: SurfaceFactory<'a>,
}

/// Surfaces whose live login is a token chain that the CLI or the app refreshes itself. Claude Desktop keeps
/// a separate browser-style session, so the vault's Claude login is free to refresh while only the app uses it.
pub fn holds_token_chain(kind: SurfaceKind) -> bool {
    kind != SurfaceKind::ClaudeDesktop
}

/// The surfaces on which `id` is active and whose login is a token chain.
pub fn active_chains(state: &State, id: &str) -> Vec<SurfaceKind> {
    state.is_active_anywhere(id).into_iter().filter(|k| holds_token_chain(*k)).collect()
}

/// The login to read: the live copy when the account is active on a chain-holding surface and the surface
/// still shows that account, otherwise the vault copy. The flag is true for a live copy.
fn read_login(ctx: &Ctx, account: &Account, active: &[SurfaceKind]) -> Result<(Slot, bool)> {
    for &kind in active {
        let surface = (ctx.surfaces)(kind);
        let who = surface.current_identity(ctx.env).ok().flatten();
        if who.and_then(|i| i.account_id).as_deref() != Some(account.id.as_str()) {
            continue;
        }
        if let Ok(Some(slot)) = surface.capture(ctx.env, account) {
            return Ok((slot, true));
        }
    }
    let slot = ctx.env.vault.get(&account.id, account.provider.store())?;
    slot.map(|s| (s, false)).ok_or_else(|| Error::NotFound(format!("saved login for `{}`", account.id)))
}

fn lapsed(cred: &Credential, now: u64, within_s: u64) -> bool {
    cred.expires_at.is_some_and(|t| t <= now + within_s)
}

/// Keep what an earlier reading knew, and say why this one has nothing new.
fn without_new_reading(prev: Option<&UsageRecord>, state: UsageState, detail: impl Into<String>, now: u64) -> UsageRecord {
    UsageRecord {
        state,
        windows: prev.map(|p| p.windows).unwrap_or_default(),
        fetched_at: prev.and_then(|p| p.fetched_at),
        checked_at: now,
        detail: Some(detail.into()),
        last_429_at: prev.and_then(|p| p.last_429_at),
        dead_fp: prev.and_then(|p| p.dead_fp.clone()),
        ..UsageRecord::default()
    }
}

/// Take one reading of `account`, store it, and return it. Never refreshes an account that is active on a
/// surface that holds its token chain. Errors only when the state file cannot be written.
pub fn poll_account(ctx: &Ctx, account: &Account, now: u64) -> Result<UsageRecord> {
    let state = State::load(ctx.env.paths)?;
    let prev = state.usage.get(&account.id).cloned();
    let active = active_chains(&state, &account.id);
    let is_active = !state.is_active_anywhere(&account.id).is_empty();
    let auto = &ctx.config.auto;

    let mut rec = match read_login(ctx, account, &active) {
        Err(e) => without_new_reading(prev.as_ref(), UsageState::Unreadable, e.to_string(), now),
        Ok((slot, live)) => take_reading(ctx, account, slot, live, prev.as_ref(), !active.is_empty(), now),
    };
    if rec.state != UsageState::Ok {
        // `take_reading` parks the server's Retry-After in `interval_s` of a 429 record.
        let retry_after = Some(rec.interval_s).filter(|s| *s > 0 && rec.last_429_at == Some(now));
        let (next, interval) = policy::plan_after_failure(prev.as_ref(), &rec, retry_after, now, policy::jitter_now());
        rec.next_poll_at = next;
        rec.interval_s = interval;
    } else {
        let (next, interval, streak) = policy::plan_after_fetch(prev.as_ref(), &rec.windows, is_active, auto.threshold, now, policy::jitter_now());
        rec.next_poll_at = next;
        rec.interval_s = interval;
        rec.urgent_streak = streak;
    }
    let out = rec.clone();
    State::update(ctx.env.paths, |s| {
        s.usage.insert(account.id.clone(), rec);
    })?;
    Ok(out)
}

fn take_reading(ctx: &Ctx, account: &Account, slot: Slot, live: bool, prev: Option<&UsageRecord>, active_on_chain: bool, now: u64) -> UsageRecord {
    // A login that was found dead stays quarantined until its refresh token changes (a new login).
    if let Some(p) = prev.filter(|p| p.state == UsageState::Dead && p.dead_fp.as_deref() == Some(slot.fingerprint.as_str())) {
        return without_new_reading(Some(p), UsageState::Dead, p.detail.clone().unwrap_or_else(|| "the login is dead".into()), now);
    }
    let mut cred = match ctx.api.credential(account, &slot.blob) {
        Ok(c) => c,
        Err(e) => return without_new_reading(prev, UsageState::Unreadable, format!("saved login unreadable: {e}"), now),
    };
    // Background refresh: opt-in, vault copies only. The live login belongs to its CLI or app.
    if !live && !active_on_chain && ctx.config.auto.refresh_inactive && lapsed(&cred, now, FRESHEN_WITHIN_S) {
        let tr = token_refresh::Ctx { env: ctx.env, api: ctx.api };
        match token_refresh::freshen(&tr, account, now, FRESHEN_WITHIN_S) {
            Ok(_) => match ctx.env.vault.get(&account.id, account.provider.store()).ok().flatten().map(|s| ctx.api.credential(account, &s.blob)) {
                Some(Ok(c)) => cred = c,
                _ => return without_new_reading(prev, UsageState::Unreadable, "refreshed login unreadable", now),
            },
            Err(FreshenError::Dead(m)) => {
                let mut r = without_new_reading(prev, UsageState::Dead, m, now);
                r.dead_fp = Some(slot.fingerprint);
                return r;
            }
            Err(FreshenError::Halted(m)) => return without_new_reading(prev, UsageState::Expired, m, now),
            Err(e) => return without_new_reading(prev, UsageState::Unreadable, e.to_string(), now),
        }
    }
    if lapsed(&cred, now, EXPIRY_SKEW_S) {
        let why = if live { "the access token expired; its CLI or app refreshes it when used" } else { "the access token expired; it is refreshed just before a switch" };
        return without_new_reading(prev, UsageState::Expired, why, now);
    }
    match ctx.api.fetch_usage(account, &cred) {
        Ok(windows) => UsageRecord { state: UsageState::Ok, windows, fetched_at: Some(now), checked_at: now, last_429_at: prev.and_then(|p| p.last_429_at), ..UsageRecord::default() },
        Err(FetchError::Unauthorized) => without_new_reading(prev, UsageState::Expired, "the usage endpoint refused the access token", now),
        Err(FetchError::Failed { kind, retry_after_s }) => {
            let mut r = without_new_reading(prev, UsageState::Unreadable, kind.clone(), now);
            if kind == "http-429" {
                r.last_429_at = Some(now);
                r.interval_s = retry_after_s.unwrap_or(0);
            }
            r
        }
    }
}

/// True when a stored reading is old enough that a command like `list` should read again. A reading that is
/// inside a 429 backoff is served from the store.
pub fn needs_reading(rec: Option<&UsageRecord>, now: u64) -> bool {
    match rec {
        None => true,
        Some(r) => now >= r.checked_at + SERVE_TTL_S && !(r.last_429_at.is_some() && now < r.next_poll_at),
    }
}

/// How old a reading is, in seconds, or `None` when the account never had one.
pub fn age_s(rec: &UsageRecord, now: u64) -> Option<u64> {
    rec.fetched_at.map(|t| now.saturating_sub(t))
}

/// Short text for an age such as `45s`, `12m`, `3h`, `2d`.
pub fn human_secs(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Shared scripted fakes for the usage, refresh, policy and watcher tests.
    use super::*;
    use crate::config::{Binding, Config};
    use crate::keychain::MemoryKeychain;
    use crate::paths::Paths;
    use crate::state::{WindowSnap, Windows};
    use crate::surface::{Identity, Staged};
    use crate::vault::{lineage_fingerprint, Vault};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub const T0: u64 = 1_800_000_000;

    /// The blob format of the fake provider: `{"exp": <unix secs>, "rt": "<refresh token>"}`.
    pub fn blob(exp: u64, rt: &str) -> String {
        format!(r#"{{"exp":{exp},"rt":"{rt}"}}"#)
    }

    pub fn windows(five: f64, week: f64) -> Windows {
        Windows {
            five_hour: Some(WindowSnap { pct: five, resets_at: Some(T0 as i64 + 3600) }),
            weekly: Some(WindowSnap { pct: week, resets_at: Some(T0 as i64 + 86_400) }),
        }
    }

    #[derive(Default)]
    pub struct FakeApi {
        pub usage: Mutex<HashMap<String, Vec<std::result::Result<Windows, FetchError>>>>,
        pub refresh: Mutex<HashMap<String, std::result::Result<String, RefreshError>>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeApi {
        pub fn script_usage(&self, id: &str, replies: Vec<std::result::Result<Windows, FetchError>>) {
            self.usage.lock().unwrap().insert(id.into(), replies);
        }
        pub fn script_refresh(&self, id: &str, reply: std::result::Result<String, RefreshError>) {
            self.refresh.lock().unwrap().insert(id.into(), reply);
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        pub fn count(&self, prefix: &str) -> usize {
            self.calls().iter().filter(|c| c.starts_with(prefix)).count()
        }
    }

    impl ProviderApi for FakeApi {
        fn credential(&self, account: &Account, blob: &str) -> Result<Credential> {
            let v: Value = serde_json::from_str(blob).map_err(|e| Error::Invalid(e.to_string()))?;
            let rt = v["rt"].as_str().ok_or_else(|| Error::Invalid("no rt".into()))?;
            Ok(Credential {
                access_token: format!("tok-{}", account.id),
                expires_at: v["exp"].as_u64(),
                workspace: None,
                fingerprint: lineage_fingerprint(rt),
            })
        }
        fn fetch_usage(&self, account: &Account, _cred: &Credential) -> std::result::Result<Windows, FetchError> {
            self.calls.lock().unwrap().push(format!("usage:{}", account.id));
            let mut all = self.usage.lock().unwrap();
            let queue = all.get_mut(&account.id).expect("no usage scripted for this account");
            if queue.len() > 1 { queue.remove(0) } else { queue[0].clone() }
        }
        fn refresh(&self, account: &Account, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
            self.calls.lock().unwrap().push(format!("refresh:{}", account.id));
            if let Some(r) = self.refresh.lock().unwrap().get(&account.id) {
                return r.clone();
            }
            let v: Value = serde_json::from_str(blob).unwrap();
            Ok(self::blob(now + 28_800, &format!("{}-next", v["rt"].as_str().unwrap())))
        }
    }

    /// A surface that shows a fixed login: `account` is who it reports, `live_blob` what `capture` returns.
    pub struct LiveSurface {
        pub kind: SurfaceKind,
        pub account: Option<String>,
        pub live_blob: Option<String>,
    }

    impl Surface for LiveSurface {
        fn kind(&self) -> SurfaceKind {
            self.kind
        }
        fn current_identity(&self, _: &Env) -> Result<Option<Identity>> {
            Ok(Some(Identity { account_id: self.account.clone(), ..Identity::default() }))
        }
        fn capture(&self, _: &Env, _: &Account) -> Result<Option<Slot>> {
            Ok(self.live_blob.as_ref().map(|b| Slot { blob: b.clone(), fingerprint: lineage_fingerprint(b), refreshed_at: 1 }))
        }
        fn stage(&self, _: &Env, a: &Account) -> Result<Staged> {
            Ok(Staged::new(a.id.clone(), vec![]))
        }
        fn apply(&self, _: &Env, _: Staged) -> Result<()> {
            Ok(())
        }
    }

    /// Temp home, vault, config and the fake API.
    pub struct Fixture {
        pub _home: tempfile::TempDir,
        pub paths: Paths,
        pub vault: Vault,
        pub config: Config,
        pub api: FakeApi,
    }

    impl Fixture {
        /// Accounts in order: `(id, provider, exp offset from T0 in seconds)`. Every slot's refresh token is `rt-<id>`.
        pub fn new(accounts: &[(&str, Provider, i64)]) -> Fixture {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths::from_home(home.path());
            let vault = Vault::new(Arc::new(MemoryKeychain::new()));
            let mut config = Config::default();
            for (id, provider, exp) in accounts {
                config.add_account(Account { id: (*id).into(), provider: *provider, label: (*id).into(), email: None, uuid: None }).unwrap();
                let rt = format!("rt-{id}");
                let slot = Slot { blob: blob((T0 as i64 + exp) as u64, &rt), fingerprint: lineage_fingerprint(&rt), refreshed_at: 1 };
                vault.put(id, provider.store(), &slot).unwrap();
            }
            config.save(&paths).unwrap();
            Fixture { _home: home, paths, vault, config, api: FakeApi::default() }
        }

        pub fn bind(&mut self, kind: SurfaceKind, id: &str) {
            self.config.bind(kind, Binding::parse(id)).unwrap();
            self.config.save(&self.paths).unwrap();
        }

        pub fn env(&self) -> Env<'_> {
            Env { paths: &self.paths, vault: &self.vault }
        }

        pub fn set_active(&self, kind: SurfaceKind, id: &str) {
            State::update(&self.paths, |s| s.set_active(kind, Some(id.into()))).unwrap();
        }
    }

    /// One-shot HTTP server for the provider tests: serves `status` and `body` once and returns the raw
    /// request text through the handle.
    pub fn serve(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = 0;
            // Read until the headers (and a short JSON body) have arrived.
            loop {
                let n = sock.read(&mut buf[got..]).unwrap();
                got += n;
                let text = String::from_utf8_lossy(&buf[..got]);
                let done = text.find("\r\n\r\n").is_some_and(|i| {
                    let want = text.to_lowercase().split("content-length:").nth(1).and_then(|r| r.split("\r\n").next()).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    got >= i + 4 + want
                });
                if n == 0 || done {
                    break;
                }
            }
            sock.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..got]).to_string()
        });
        (base, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::state::{WindowSnap, Windows};

    fn surfaces_none() -> impl Fn(SurfaceKind) -> Box<dyn Surface> {
        |k| Box::new(crate::surface::StubSurface::new(k))
    }

    fn poll(f: &Fixture, id: &str, now: u64, surfaces: SurfaceFactory) -> UsageRecord {
        let env = f.env();
        let ctx = Ctx { env: &env, config: &f.config, api: &f.api, surfaces };
        poll_account(&ctx, f.config.account(id).unwrap(), now).unwrap()
    }

    #[test]
    fn a_redirect_is_not_followed_so_a_bearer_token_never_reaches_another_host() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", first.local_addr().unwrap());
        let location = format!("http://{}/stolen", elsewhere.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut sock, _) = first.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let reply = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            sock.write_all(reply.as_bytes()).unwrap();
        });
        let result = http_get(&url, &[("Authorization", "Bearer secret")]);
        server.join().unwrap();
        assert!(!matches!(result, Ok(Reply { status: 200, .. })), "{:?}", result.as_ref().map(|r| r.status));
        assert!(elsewhere.accept().is_err(), "nothing connected to the redirect target");
    }

    #[test]
    fn an_ok_reading_is_stored_with_both_windows_and_a_next_poll() {
        let f = Fixture::new(&[("a", Provider::Claude, 3600)]);
        f.api.script_usage("a", vec![Ok(windows(13.0, 89.0))]);
        let rec = poll(&f, "a", T0, &surfaces_none());
        assert_eq!(rec.state, UsageState::Ok);
        assert_eq!(rec.windows, windows(13.0, 89.0));
        assert_eq!(rec.fetched_at, Some(T0));
        assert!(rec.next_poll_at >= T0 + 160, "never sooner than the floor (less jitter): {}", rec.next_poll_at - T0);
        assert_eq!(State::load(&f.paths).unwrap().usage["a"], rec);
    }

    #[test]
    fn an_expired_token_shows_expired_with_the_last_known_numbers_and_sends_nothing() {
        let f = Fixture::new(&[("a", Provider::Claude, -10)]);
        State::update(&f.paths, |s| {
            s.usage.insert("a".into(), UsageRecord { windows: windows(40.0, 50.0), fetched_at: Some(T0 - 7200), ..UsageRecord::default() });
        })
        .unwrap();
        let rec = poll(&f, "a", T0, &surfaces_none());
        assert_eq!(rec.state, UsageState::Expired);
        assert_eq!(rec.windows, windows(40.0, 50.0), "last-known values are kept");
        assert_eq!(age_s(&rec, T0), Some(7200));
        assert_eq!(f.api.calls(), Vec::<String>::new(), "no usage request and no refresh");
    }

    #[test]
    fn an_antigravity_account_reads_its_quota_through_the_live_api_and_an_expired_login_shows_expired() {
        use crate::providers::antigravity::{tests::login_json, AntigravityApi, Login};
        let mut f = Fixture::new(&[]);
        f.config.add_account(Account { id: "g".into(), provider: Provider::Antigravity, label: "g".into(), email: None, uuid: None }).unwrap();
        f.config.save(&f.paths).unwrap();
        let put = |expiry: &str| {
            let blob = login_json("g@gmail.com", "ya29.fake", "1//rt-g", expiry);
            let fingerprint = Login::parse(&blob).unwrap().fingerprint;
            f.vault.put("g", "antigravity-oauth", &Slot { blob, fingerprint, refreshed_at: 1 }).unwrap();
        };
        let poll_with = |api: &LiveApi| {
            let env = f.env();
            let ctx = Ctx { env: &env, config: &f.config, api, surfaces: &surfaces_none() };
            poll_account(&ctx, f.config.account("g").unwrap(), T0).unwrap()
        };

        // T0 is 2027-01-15T08:00:00Z. An access token that lapsed an hour ago: nothing is sent (the port is closed).
        put("2027-01-15T14:00:00+07:00");
        let closed = LiveApi { antigravity: AntigravityApi::with_base("http://127.0.0.1:9"), ..LiveApi::production() };
        let rec = poll_with(&closed);
        assert_eq!(rec.state, UsageState::Expired, "{:?}", rec.detail);

        // A valid one: the most-used model with a reset decides each window. The endpoint's reset times are
        // classified against the clock, so they are built from it.
        put("2027-01-15T16:00:00+07:00");
        let wall = crate::state::now_secs();
        let at = crate::providers::openai::rfc3339_utc;
        let body = serde_json::json!({"models": {
            "gemini-3-pro": {"quotaInfo": {"remainingFraction": 0.25, "resetTime": at(wall + 5 * 86_400)}},
            "claude-sonnet": {"quotaInfo": {"remainingFraction": 0.9, "resetTime": at(wall + 7200)}},
            "no-limit": {}
        }});
        let (base, server) = serve("200 OK", &body.to_string());
        let rec = poll_with(&LiveApi { antigravity: AntigravityApi::with_base(&base), ..LiveApi::production() });
        let request = server.join().unwrap();
        assert!(request.contains("Bearer ya29.fake"));
        assert_eq!(rec.state, UsageState::Ok, "{:?}", rec.detail);
        let week = rec.windows.weekly.unwrap();
        let five = rec.windows.five_hour.unwrap();
        assert_eq!((week.pct.round(), five.pct.round()), (75.0, 10.0));
        assert_eq!(five.resets_at, Some(wall as i64 + 7200));
    }

    #[test]
    fn a_refused_token_is_expired_not_dead() {
        let f = Fixture::new(&[("a", Provider::Claude, 3600)]);
        f.api.script_usage("a", vec![Err(FetchError::Unauthorized)]);
        assert_eq!(poll(&f, "a", T0, &surfaces_none()).state, UsageState::Expired);
    }

    #[test]
    fn endpoint_failures_are_unreadable_and_keep_old_values() {
        let f = Fixture::new(&[("a", Provider::Claude, 3600)]);
        f.api.script_usage("a", vec![Ok(windows(10.0, 20.0)), Err(FetchError::Failed { kind: "http-503".into(), retry_after_s: None })]);
        poll(&f, "a", T0, &surfaces_none());
        let rec = poll(&f, "a", T0 + 400, &surfaces_none());
        assert_eq!(rec.state, UsageState::Unreadable);
        assert_eq!(rec.detail.as_deref(), Some("http-503"));
        assert_eq!(rec.windows, windows(10.0, 20.0));
        assert_eq!(rec.fetched_at, Some(T0));
    }

    #[test]
    fn a_429_records_the_time_and_backs_off_beyond_the_floor() {
        let f = Fixture::new(&[("a", Provider::Claude, 3600)]);
        f.api.script_usage("a", vec![Err(FetchError::Failed { kind: "http-429".into(), retry_after_s: Some(0) })]);
        let rec = poll(&f, "a", T0, &surfaces_none());
        assert_eq!(rec.last_429_at, Some(T0));
        assert!(rec.next_poll_at >= T0 + 300, "{}", rec.next_poll_at - T0);
        assert!(!needs_reading(Some(&rec), T0 + 200), "list serves the stored reading during the backoff");
        assert!(needs_reading(Some(&rec), rec.next_poll_at));
    }

    #[test]
    fn a_dead_login_stays_dead_until_its_refresh_token_changes() {
        let f = Fixture::new(&[("a", Provider::Claude, -10)]);
        let fp = f.vault.get("a", "claude-oauth").unwrap().unwrap().fingerprint;
        State::update(&f.paths, |s| {
            s.usage.insert("a".into(), UsageRecord { state: UsageState::Dead, dead_fp: Some(fp), detail: Some("invalid_grant".into()), ..UsageRecord::default() });
        })
        .unwrap();
        f.api.script_usage("a", vec![Ok(windows(1.0, 1.0))]);
        assert_eq!(poll(&f, "a", T0, &surfaces_none()).state, UsageState::Dead);
        assert_eq!(f.api.calls().len(), 0, "a dead login costs no request");
        // A new login (different refresh token) is read again.
        let rt = "rt-new";
        f.vault.put("a", "claude-oauth", &Slot { blob: blob(T0 + 3600, rt), fingerprint: crate::vault::lineage_fingerprint(rt), refreshed_at: 2 }).unwrap();
        assert_eq!(poll(&f, "a", T0, &surfaces_none()).state, UsageState::Ok);
    }

    #[test]
    fn an_active_account_is_read_live_and_never_refreshed() {
        let mut f = Fixture::new(&[("a", Provider::Claude, -10)]); // the vault copy has lapsed
        f.config.auto.refresh_inactive = true; // even with the opt-in on
        f.set_active(SurfaceKind::ClaudeCli, "a");
        f.api.script_usage("a", vec![Ok(windows(5.0, 6.0))]);
        let live = LiveSurface { kind: SurfaceKind::ClaudeCli, account: Some("a".into()), live_blob: Some(blob(T0 + 3600, "rt-live")) };
        let factory = move |_k: SurfaceKind| -> Box<dyn Surface> { Box::new(LiveSurface { kind: live.kind, account: live.account.clone(), live_blob: live.live_blob.clone() }) };
        let rec = poll(&f, "a", T0, &factory);
        assert_eq!(rec.state, UsageState::Ok, "the live token is fresh even though the vault copy lapsed");
        assert_eq!(f.api.count("refresh:"), 0);

        // The live token lapses too: still no refresh, the CLI owns it.
        let lapsed_live = move |_k: SurfaceKind| -> Box<dyn Surface> { Box::new(LiveSurface { kind: SurfaceKind::ClaudeCli, account: Some("a".into()), live_blob: Some(blob(T0 - 1, "rt-live")) }) };
        assert_eq!(poll(&f, "a", T0 + 500, &lapsed_live).state, UsageState::Expired);
        assert_eq!(f.api.count("refresh:"), 0);
    }

    #[test]
    fn a_surface_that_shows_another_account_is_not_trusted_for_this_one() {
        let f = Fixture::new(&[("a", Provider::Claude, 3600)]);
        f.set_active(SurfaceKind::ClaudeCli, "a");
        f.api.script_usage("a", vec![Ok(windows(1.0, 2.0))]);
        // The CLI was logged into someone else by hand: its login must not be used for `a`.
        let other = |_k: SurfaceKind| -> Box<dyn Surface> { Box::new(LiveSurface { kind: SurfaceKind::ClaudeCli, account: Some("zzz".into()), live_blob: Some(blob(T0 - 5, "rt-zzz")) }) };
        assert_eq!(poll(&f, "a", T0, &other).state, UsageState::Ok, "falls back to the vault copy");
    }

    #[test]
    fn background_refresh_is_off_by_default_and_works_for_inactive_accounts_when_on() {
        let mut f = Fixture::new(&[("a", Provider::Claude, -10)]);
        f.api.script_usage("a", vec![Ok(windows(7.0, 8.0))]);
        assert_eq!(poll(&f, "a", T0, &surfaces_none()).state, UsageState::Expired);
        assert_eq!(f.api.count("refresh:"), 0, "default: agent-switch sends no refresh request");

        f.config.auto.refresh_inactive = true;
        let rec = poll(&f, "a", T0 + 400, &surfaces_none());
        assert_eq!(rec.state, UsageState::Ok);
        assert_eq!(f.api.count("refresh:"), 1);
        let saved = f.vault.get("a", "claude-oauth").unwrap().unwrap();
        assert!(saved.blob.contains("rt-a-next"), "the rotated login was saved back");
    }

    #[test]
    fn a_dead_refresh_token_quarantines_the_account() {
        let mut f = Fixture::new(&[("a", Provider::Claude, -10)]);
        f.config.auto.refresh_inactive = true;
        f.api.script_refresh("a", Err(RefreshError::Dead("invalid_grant".into())));
        let rec = poll(&f, "a", T0, &surfaces_none());
        assert_eq!(rec.state, UsageState::Dead);
        assert!(rec.dead_fp.is_some());
        // The next pass makes no refresh request at all.
        poll(&f, "a", T0 + 400, &surfaces_none());
        assert_eq!(f.api.count("refresh:"), 1);
    }

    #[test]
    fn windows_without_data_are_left_out_not_zeroed() {
        let w = Windows { five_hour: None, weekly: Some(WindowSnap { pct: 5.0, resets_at: None }) };
        assert_eq!(w.worst_at(0), Some(5.0));
    }

    #[test]
    fn human_secs_reads_naturally() {
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(720), "12m");
        assert_eq!(human_secs(3 * 3600 + 5 * 60), "3h 05m");
        assert_eq!(human_secs(2 * 86_400 + 3 * 3600), "2d 3h");
    }

    #[test]
    fn credential_debug_hides_the_token() {
        let c = Credential { access_token: "super-secret".into(), expires_at: Some(1), workspace: None, fingerprint: "f".into() };
        assert!(!format!("{c:?}").contains("super-secret"));
    }
}
