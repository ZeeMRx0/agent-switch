//! The auto-switch policy and the polling cadence. Pure functions of the config, the state and the clock, so
//! every rule is tested without a network or a Keychain.
//!
//! Ported from claude-swap `autoswitch.py` (threshold, hysteresis, cooldown, quarantine, freshen before
//! activation) and `poll_policy.py` (cadence), simplified to one threshold applied to both windows and no
//! `best` strategy. Accounts are tried in the surface's priority order (`Config::priority`).
//!
//! * **Per surface.** A surface whose auto-switch is off (`Auto::enabled_for`) is left alone.
//! * **Trigger.** The surface's current account has a window at or above `threshold`. A window whose reset
//!   time has passed counts as 0.
//! * **Candidates.** Same provider, in priority order, not the current account, not dead, not live on another
//!   surface that owns its token, and with both windows below `threshold - hysteresis`. The margin is what
//!   stops a switch at 91 from flipping back at 88. The caller freshens the candidate and re-checks it just
//!   before activating.
//! * **Return.** Below the threshold, with the cooldown over and no event waiting, the surface moves back to
//!   an account ranked above its current one, if one has a fresh `Ok` reading with headroom, is not dead or
//!   live elsewhere, and is not the target of another surface's waiting event. The outcome is marked `back`.
//!   A Codex CLI that shares `~/.codex` with the app never splits off to return. A manual choice makes the
//!   account the surface's first priority, so a return goes back to what the user picked.
//! * **Cooldown.** No second automatic switch on a surface within `cooldown_minutes`, unless the current
//!   account is exhausted (a window at 100).
//! * **CLI surfaces switch now. Desktop surfaces only raise an event** (`Outcome::Notify`); an app is never
//!   quit without a click. `codex-cli_mode = notify` makes the Codex CLI behave like a Desktop surface.
//! * **Cadence.** At most one usage request per 180 s per account; a rising window of the active account near
//!   its line polls every 60 s, bounded to 15 polls in a row; a 429 stretches the interval; an exhausted
//!   account is polled every 10 minutes (claude-swap `poll_policy.py`, measured limit about 28-30 requests per
//!   rolling hour).

use crate::config::{Auto, CodexCliMode, Config, Provider, SurfaceKind};
use crate::state::{State, UsageRecord, UsageState, Windows, REASON_SWITCH_NEEDED};
use crate::usage::holds_token_chain;

pub const MIN_INTERVAL_S: u64 = 180;
pub const URGENT_INTERVAL_S: u64 = 60;
pub const ACTIVE_MAX_INTERVAL_S: u64 = 300;
pub const CANDIDATE_DEFAULT_INTERVAL_S: u64 = 300;
pub const CANDIDATE_MAX_INTERVAL_S: u64 = 600;
pub const EXHAUSTED_INTERVAL_S: u64 = 600;
pub const POST_429_MIN_INTERVAL_S: u64 = 360;
pub const POST_429_MAX_INTERVAL_S: u64 = 1800;
pub const RECENT_429_WINDOW_S: u64 = 3600;
/// An urgent poll needs a window within this many points of the threshold.
pub const ESCALATION_MARGIN_PCT: f64 = 15.0;
/// Percent a window must rise between two polls to count as movement.
pub const MOVEMENT_DELTA_PCT: f64 = 1.0;
/// Margin / movement delta: the most urgent polls one episode can take.
pub const MAX_URGENT_STREAK: u32 = 15;
const RESET_SLACK_S: u64 = 60;
/// Retry delay after a request that never reached the server (`network` or `timeout`), so readings come back
/// soon after the connection returns.
pub const OFFLINE_RETRY_S: u64 = 60;
const JITTER_FRAC: f64 = 0.1;

/// A reading older than this is stale: it no longer supports a switch decision.
pub const STALE_AFTER_S: u64 = 1800;

/// A number in `[0, 1)` that differs between processes and calls, so pollers drift apart.
pub fn jitter_now() -> f64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    f64::from(nanos % 1000) / 1000.0
}

fn raw_worst(w: &Windows) -> Option<f64> {
    [w.five_hour, w.weekly].into_iter().flatten().map(|s| s.pct).fold(None, |m, p| Some(m.map_or(p, |m: f64| m.max(p))))
}

/// `jitter` in `[0, 1)` adds up to 10 % on top of the interval, never below it, so the 180 s floor holds.
fn schedule(now: u64, interval_s: u64, jitter: f64) -> u64 {
    now + (interval_s as f64 * (1.0 + JITTER_FRAC * jitter.clamp(0.0, 1.0))) as u64
}

fn recent_429(prev: Option<&UsageRecord>, now: u64) -> bool {
    prev.and_then(|p| p.last_429_at).is_some_and(|t| now.saturating_sub(t) < RECENT_429_WINDOW_S)
}

/// `(next_poll_at, interval_s, urgent_streak)` after a successful reading.
///
/// Movement halves the interval (never below 180 s); a still account backs off x1.5 toward its ceiling; the
/// active account with a window that rose into the band below its line polls every 60 s, at most
/// `MAX_URGENT_STREAK` times in a row; a recent 429 floors the cadence at 360 s and suppresses urgent mode
/// (`poll_policy.py:plan_after_fetch`).
pub fn plan_after_fetch(prev: Option<&UsageRecord>, new: &Windows, is_active: bool, threshold: u8, now: u64, jitter: f64) -> (u64, u64, u32) {
    let default = if is_active { MIN_INTERVAL_S } else { CANDIDATE_DEFAULT_INTERVAL_S };
    let ceiling = if is_active { ACTIVE_MAX_INTERVAL_S } else { CANDIDATE_MAX_INTERVAL_S };
    let base = prev.map(|p| p.interval_s).filter(|s| *s > 0).unwrap_or(default);
    let recent = recent_429(prev, now);

    let mut interval = match (prev.and_then(|p| raw_worst(&p.windows)), raw_worst(new)) {
        (Some(before), Some(after)) if (after - before).abs() >= MOVEMENT_DELTA_PCT => MIN_INTERVAL_S.max(base / 2),
        (Some(_), Some(_)) => ceiling.min(MIN_INTERVAL_S.max(base * 3 / 2)),
        _ => default,
    };

    // Per window, rising only: a window that dropped into the band just rolled over.
    let rising_in_band = prev.is_some_and(|p| {
        [(p.windows.five_hour, new.five_hour), (p.windows.weekly, new.weekly)].into_iter().any(|(before, after)| match (before, after) {
            (Some(b), Some(a)) => a.pct - b.pct >= MOVEMENT_DELTA_PCT && a.pct >= f64::from(threshold) - ESCALATION_MARGIN_PCT,
            _ => false,
        })
    });
    let prev_streak = prev.map_or(0, |p| p.urgent_streak);
    let mut streak = 0;
    if is_active && rising_in_band && !recent {
        streak = prev_streak.min(MAX_URGENT_STREAK);
        if streak < MAX_URGENT_STREAK {
            interval = URGENT_INTERVAL_S;
            streak += 1;
        }
    }
    if recent {
        let widened = (base * 3 / 2).max(POST_429_MIN_INTERVAL_S);
        interval = POST_429_MAX_INTERVAL_S.min(interval.max(widened));
    }
    let exhausted = raw_worst(new).is_some_and(|p| p >= 100.0);
    if exhausted {
        // Keep probing: a quota grant can arrive before the advertised reset.
        interval = interval.max(EXHAUSTED_INTERVAL_S);
    }

    let mut next = schedule(now, interval, jitter);
    // Never later than the next window reset: a stored reading is obsolete once its window rolls over. The
    // floor still applies, so a reset a few seconds away does not cause a burst of polls.
    let floor = now + if interval == URGENT_INTERVAL_S { URGENT_INTERVAL_S } else { MIN_INTERVAL_S };
    let upcoming = [new.five_hour, new.weekly].into_iter().flatten().filter_map(|w| w.resets_at).filter(|t| *t > now as i64).min();
    if let Some(reset) = upcoming {
        next = next.min(reset as u64 + RESET_SLACK_S).max(floor);
    }
    (next, interval, streak)
}

/// `(next_poll_at, interval_s)` after a reading that produced no numbers. A 429 stretches the interval
/// (AIMD, `poll_policy.py`), honoring `Retry-After`; other failures back off toward 10 minutes; an expired
/// token is only a local check, so it keeps the 180 s cadence; a dead login is rechecked every 10 minutes
/// in case a new sign-in replaced it. A request that never reached the server (offline) is retried after a
/// minute and keeps the stored interval, so an outage does not stretch the cadence.
pub fn plan_after_failure(prev: Option<&UsageRecord>, rec: &UsageRecord, retry_after_s: Option<u64>, now: u64, jitter: f64) -> (u64, u64) {
    let base = prev.map(|p| p.interval_s).filter(|s| *s > 0).unwrap_or(MIN_INTERVAL_S);
    if rec.state == UsageState::Unreadable && matches!(rec.detail.as_deref(), Some("network" | "timeout")) {
        return (schedule(now, OFFLINE_RETRY_S, jitter), base);
    }
    let mut interval = match rec.state {
        UsageState::Dead => EXHAUSTED_INTERVAL_S,
        UsageState::Expired | UsageState::Ok => MIN_INTERVAL_S,
        UsageState::Unreadable => CANDIDATE_MAX_INTERVAL_S.min(MIN_INTERVAL_S.max(base * 3 / 2)),
    };
    if rec.last_429_at == Some(now) || recent_429(prev, now) {
        let widened = (base * 3 / 2).max(POST_429_MIN_INTERVAL_S);
        interval = POST_429_MAX_INTERVAL_S.min(interval.max(widened));
    }
    if let Some(wait) = retry_after_s.filter(|_| rec.last_429_at == Some(now)) {
        interval = interval.max(wait + RESET_SLACK_S);
    }
    (schedule(now, interval, jitter), interval)
}

// ---------------------------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------------------------

pub fn is_stale(rec: Option<&UsageRecord>, now: u64) -> bool {
    rec.and_then(|r| r.fetched_at).is_none_or(|t| now.saturating_sub(t) > STALE_AFTER_S)
}

/// A window of `w` is exhausted when it has reached 100 %.
pub fn exhausted(w: &Windows, now: u64) -> bool {
    w.worst_at(now).is_some_and(|p| p >= 100.0)
}

/// Both windows are below `threshold - hysteresis` and neither is exhausted.
pub fn has_headroom(w: &Windows, auto: &Auto, now: u64) -> bool {
    let line = f64::from(auto.threshold) - f64::from(auto.hysteresis);
    w.worst_at(now).is_some_and(|p| p < line)
}

/// A candidate before its login is freshened: not dead, and not known to be over the line. A candidate with
/// no reading at all passes; the final check after freshening decides.
fn plausible(rec: Option<&UsageRecord>, auto: &Auto, now: u64) -> bool {
    match rec {
        None => true,
        Some(r) if r.state == UsageState::Dead => false,
        Some(r) => r.windows.worst_at(now).is_none_or(|_| has_headroom(&r.windows, auto, now)),
    }
}

/// The final check on a candidate whose login was just freshened and read again.
pub fn confirmed(rec: &UsageRecord, auto: &Auto, now: u64) -> bool {
    rec.state == UsageState::Ok && rec.fetched_at.is_some_and(|t| now.saturating_sub(t) <= MIN_INTERVAL_S) && has_headroom(&rec.windows, auto, now)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// The surface is unmanaged or has no account.
    NotManaged,
    /// Auto-switch is off for this surface.
    AutoOff,
    /// The current account is below the threshold on both windows, and there is no higher-priority account
    /// to return to now.
    NoTrigger,
    Cooldown,
    /// The current account's reading is stale, so nothing can be decided.
    Stale,
    /// A switch-needed event for this surface is still waiting for its click.
    EventPending,
    /// The Codex CLI shares `~/.codex` with the ChatGPT app and waits for the app's event.
    FollowsDesktop,
    NoCandidate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Hold(Hold),
    /// CLI surface: switch now to the first of `candidates` that passes the final check. `split` is true when
    /// the Codex CLI shares `~/.codex` with the app and must split off into its own home. `back` is true when
    /// this returns to a higher-priority account rather than leaving one at its limit.
    Switch { from: String, candidates: Vec<String>, split: bool, back: bool },
    /// Raise a switch-needed event. `follows_cli` is true for the ChatGPT app sharing `~/.codex` with a CLI
    /// that switches now: the event then targets the CLI's new account, so the click is the rejoin. `back`
    /// is as for `Switch`.
    Notify { from: String, candidates: Vec<String>, follows_cli: bool, back: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub surface: SurfaceKind,
    pub outcome: Outcome,
    /// The worst window of the current account, in percent, when it was read.
    pub pct: Option<f64>,
}

/// Account used by `kind` right now: the active one, or the binding before the first activation.
pub fn current_account(config: &Config, state: &State, kind: SurfaceKind) -> Option<String> {
    config.surfaces.get(kind).account_id()?;
    state.active(kind).or(config.surfaces.get(kind).account_id()).map(str::to_string)
}

/// Whether the Codex CLI and the ChatGPT app are bound to one account and share `~/.codex` right now.
pub fn codex_shares_home(config: &Config, state: &State) -> bool {
    let (cli, app) = (config.surfaces.codex_cli.account_id(), config.surfaces.chatgpt_desktop.account_id());
    cli.is_some() && cli == app && !state.codex_split()
}

/// Another surface that owns a token chain of the same provider already shows `cand`: switching `kind` to it
/// would put two live copies of one login in use. The one exception is the app moving to the account the
/// split Codex CLI already uses, which is the rejoin.
pub fn live_elsewhere(state: &State, kind: SurfaceKind, cand: &str) -> bool {
    SurfaceKind::ALL.into_iter().any(|other| {
        other != kind
            && other.provider() == kind.provider()
            && holds_token_chain(other)
            && state.active(other) == Some(cand)
            && !(kind == SurfaceKind::ChatgptDesktop && other == SurfaceKind::CodexCli && state.codex_split())
    })
}

/// While the Codex CLI is split, the app's event goes to the account the CLI already moved to.
fn prefer_cli_target(state: &State, kind: SurfaceKind, out: &mut Vec<String>) {
    if kind == SurfaceKind::ChatgptDesktop && state.codex_split() {
        if let Some(target) = state.pending(SurfaceKind::CodexCli).map(|p| p.target_account.clone()) {
            if let Some(i) = out.iter().position(|id| *id == target) {
                let t = out.remove(i);
                out.insert(0, t);
            }
        }
    }
}

/// Candidate account ids for `kind`, in priority order.
pub fn candidates(config: &Config, state: &State, kind: SurfaceKind, from: &str, now: u64) -> Vec<String> {
    let mut out: Vec<String> = config
        .priority(kind)
        .into_iter()
        .filter(|id| id != from)
        .filter(|id| !live_elsewhere(state, kind, id))
        .filter(|id| plausible(state.usage.get(id), &config.auto, now))
        .collect();
    prefer_cli_target(state, kind, &mut out);
    out
}

/// Another surface of `kind`'s provider has a waiting switch-needed event that would move it to `cand`.
fn pending_elsewhere(state: &State, kind: SurfaceKind, cand: &str, now: u64) -> bool {
    SurfaceKind::ALL.into_iter().any(|other| {
        other != kind
            && other.provider() == kind.provider()
            && state.pending(other).is_some_and(|p| p.reason == REASON_SWITCH_NEEDED && !p.is_expired(now) && p.target_account == cand)
    })
}

/// Accounts ranked above `from` in `kind`'s priority that `kind` can return to now: a fresh `Ok` reading
/// with headroom (so not dead and not stale), not live on another surface that owns its token, and not the
/// target of another surface's waiting event.
pub fn return_candidates(config: &Config, state: &State, kind: SurfaceKind, from: &str, now: u64) -> Vec<String> {
    let mut order = config.priority(kind);
    let Some(rank) = order.iter().position(|id| id == from) else { return Vec::new() };
    order.truncate(rank);
    let mut out: Vec<String> = order
        .into_iter()
        .filter(|id| !live_elsewhere(state, kind, id) && !pending_elsewhere(state, kind, id, now))
        .filter(|id| {
            let rec = state.usage.get(id);
            !is_stale(rec, now) && rec.is_some_and(|r| r.state == UsageState::Ok && has_headroom(&r.windows, &config.auto, now))
        })
        .collect();
    prefer_cli_target(state, kind, &mut out);
    out
}

/// Decide, for every managed surface, what auto mode does now. Surfaces come in `SurfaceKind::ALL` order, so
/// the Codex CLI is decided before the ChatGPT app.
pub fn decide(config: &Config, state: &State, now: u64) -> Vec<Decision> {
    let auto = &config.auto;
    let cooldown_s = u64::from(auto.cooldown_minutes) * 60;
    let mut out = Vec::new();
    for kind in SurfaceKind::ALL {
        let hold = |h: Hold, pct: Option<f64>| Decision { surface: kind, outcome: Outcome::Hold(h), pct };
        let Some(from) = current_account(config, state, kind) else {
            out.push(hold(Hold::NotManaged, None));
            continue;
        };
        if !auto.enabled_for(kind) {
            out.push(hold(Hold::AutoOff, None));
            continue;
        }
        let rec = state.usage.get(&from);
        if is_stale(rec, now) {
            out.push(hold(Hold::Stale, None));
            continue;
        }
        let windows = rec.map(|r| r.windows).unwrap_or_default();
        let pct = windows.worst_at(now);
        let cooling = state.last_auto_switch.get(&kind).is_some_and(|t| now.saturating_sub(*t) < cooldown_s);
        let waiting = state.pending(kind).is_some_and(|p| p.reason == REASON_SWITCH_NEEDED && !p.is_expired(now));
        let shares = kind == SurfaceKind::CodexCli && codex_shares_home(config, state);
        // Below the threshold the only move is back up the priority list. A shared Codex CLI does not split
        // off for it: splitting is only for an account at its limit.
        let back = pct.is_none_or(|p| p < f64::from(auto.threshold));
        let returns = if back {
            let returns = if cooling || waiting || shares { Vec::new() } else { return_candidates(config, state, kind, &from, now) };
            if returns.is_empty() {
                out.push(hold(Hold::NoTrigger, pct));
                continue;
            }
            Some(returns)
        } else {
            if cooling && !exhausted(&windows, now) {
                out.push(hold(Hold::Cooldown, pct));
                continue;
            }
            None
        };

        let cli_notifies = kind == SurfaceKind::CodexCli && auto.codex_cli_mode == CodexCliMode::Notify;
        if cli_notifies && shares {
            // The app's event moves both; the CLI needs none of its own.
            out.push(hold(Hold::FollowsDesktop, pct));
            continue;
        }
        let cands = returns.unwrap_or_else(|| candidates(config, state, kind, &from, now));
        if cands.is_empty() {
            out.push(hold(Hold::NoCandidate, pct));
            continue;
        }
        let notify = kind.is_desktop() || cli_notifies;
        if notify {
            if waiting {
                out.push(hold(Hold::EventPending, pct));
                continue;
            }
            // On a return the shared CLI does not move first, so the event does not follow it.
            let follows_cli = !back && kind == SurfaceKind::ChatgptDesktop && codex_shares_home(config, state) && auto.codex_cli_mode != CodexCliMode::Notify;
            out.push(Decision { surface: kind, outcome: Outcome::Notify { from, candidates: cands, follows_cli, back }, pct });
        } else {
            out.push(Decision { surface: kind, outcome: Outcome::Switch { from, candidates: cands, split: shares, back }, pct });
        }
    }
    out
}

/// Providers whose every account has a stale (or no) reading while at least one surface of that provider is
/// managed with auto-switch on: auto mode cannot decide anything for them and pauses.
pub fn paused_providers(config: &Config, state: &State, now: u64) -> Vec<Provider> {
    Provider::ALL
        .into_iter()
        .filter(|p| {
            SurfaceKind::ALL.into_iter().any(|k| k.provider() == *p && config.surfaces.get(k).account_id().is_some() && config.auto.enabled_for(k))
        })
        .filter(|p| config.accounts.iter().filter(|a| a.provider == *p).all(|a| is_stale(state.usage.get(&a.id), now)))
        .collect()
}

/// The earliest `next_poll_at` over all accounts, or `now` when one has never been read.
pub fn next_due(config: &Config, state: &State, now: u64) -> u64 {
    config.accounts.iter().map(|a| state.usage.get(&a.id).map_or(now, |r| r.next_poll_at)).min().unwrap_or(now + MIN_INTERVAL_S)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, Binding};
    use crate::state::{Pending, WindowSnap, REASON_REJOIN};
    use crate::usage::testkit::{windows, T0};

    fn acct(id: &str, p: Provider) -> Account {
        Account { id: id.into(), provider: p, label: id.into(), email: None, uuid: None }
    }

    fn rec(five: f64, week: f64) -> UsageRecord {
        UsageRecord { windows: windows(five, week), fetched_at: Some(T0 - 60), checked_at: T0 - 60, interval_s: 180, ..UsageRecord::default() }
    }

    /// claude: a (active on claude-cli), b, c. openai: x, y, z.
    struct World {
        config: Config,
        state: State,
    }

    fn world() -> World {
        let mut config = Config::default();
        for id in ["a", "b", "c"] {
            config.add_account(acct(id, Provider::Claude)).unwrap();
        }
        for id in ["x", "y", "z"] {
            config.add_account(acct(id, Provider::Openai)).unwrap();
        }
        config.bind(SurfaceKind::ClaudeCli, Binding::parse("a")).unwrap();
        config.auto.enabled = true;
        let mut state = State::default();
        state.set_active(SurfaceKind::ClaudeCli, Some("a".into()));
        for id in ["a", "b", "c", "x", "y", "z"] {
            state.usage.insert(id.into(), rec(10.0, 10.0));
        }
        World { config, state }
    }

    fn cli(w: &World) -> Outcome {
        decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::ClaudeCli).unwrap().outcome
    }

    fn switch_to(o: &Outcome) -> Vec<String> {
        match o {
            Outcome::Switch { candidates, .. } | Outcome::Notify { candidates, .. } => candidates.clone(),
            other => panic!("expected a switch, got {other:?}"),
        }
    }

    #[test]
    fn a_91_percent_window_on_either_window_triggers_and_89_does_not() {
        let mut w = world();
        w.state.usage.insert("a".into(), rec(89.0, 89.0));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger));
        w.state.usage.insert("a".into(), rec(91.0, 10.0));
        assert!(matches!(cli(&w), Outcome::Switch { .. }), "5-hour window");
        w.state.usage.insert("a".into(), rec(10.0, 91.0));
        assert!(matches!(cli(&w), Outcome::Switch { .. }), "weekly window");
        w.state.usage.insert("a".into(), rec(10.0, 90.0));
        assert!(matches!(cli(&w), Outcome::Switch { .. }), "at the threshold counts");
    }

    #[test]
    fn candidates_come_in_config_order_and_skip_dead_hot_and_unknown_state_correctly() {
        let mut w = world();
        w.state.usage.insert("a".into(), rec(91.0, 10.0));
        assert_eq!(switch_to(&cli(&w)), ["b", "c"]);
        w.state.usage.get_mut("b").unwrap().state = UsageState::Dead;
        assert_eq!(switch_to(&cli(&w)), ["c"], "a quarantined account is never a candidate");
        w.state.usage.insert("c".into(), rec(10.0, 86.0));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoCandidate), "86 is above threshold minus hysteresis (85)");
        w.state.usage.insert("c".into(), rec(10.0, 84.0));
        assert_eq!(switch_to(&cli(&w)), ["c"]);
        // Never read yet: worth trying, the final check decides.
        w.state.usage.remove("c");
        assert_eq!(switch_to(&cli(&w)), ["c"]);
    }

    #[test]
    fn hysteresis_stops_a_flip_back_at_88_after_a_switch_at_91() {
        let mut w = world();
        // a is at 91 and switches to b.
        w.state.usage.insert("a".into(), rec(91.0, 10.0));
        assert_eq!(switch_to(&cli(&w))[0], "b");
        w.state.set_active(SurfaceKind::ClaudeCli, Some("b".into()));
        // a drifts down to 88 while b climbs to 91: a is not a way back; c is.
        w.state.usage.insert("a".into(), rec(88.0, 10.0));
        w.state.usage.insert("b".into(), rec(91.0, 10.0));
        assert_eq!(switch_to(&cli(&w)), ["c"]);
        // Only below 85 may a come back.
        w.state.usage.insert("a".into(), rec(84.0, 10.0));
        assert_eq!(switch_to(&cli(&w)), ["a", "c"]);
    }

    #[test]
    fn a_window_past_its_reset_time_counts_as_zero_for_candidates_and_trigger() {
        let mut w = world();
        w.state.usage.insert("a".into(), rec(91.0, 10.0));
        let mut b = rec(99.0, 10.0);
        b.windows.five_hour = Some(WindowSnap { pct: 99.0, resets_at: Some(T0 as i64 - 1) });
        w.state.usage.insert("b".into(), b);
        assert_eq!(switch_to(&cli(&w))[0], "b", "b's 5-hour window has rolled over");
        let mut a = rec(91.0, 10.0);
        a.windows.five_hour = Some(WindowSnap { pct: 91.0, resets_at: Some(T0 as i64 - 1) });
        w.state.usage.insert("a".into(), a);
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger));
    }

    #[test]
    fn cooldown_blocks_a_second_switch_unless_the_account_is_exhausted() {
        let mut w = world();
        w.state.usage.insert("a".into(), rec(95.0, 10.0));
        w.state.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 299);
        assert_eq!(cli(&w), Outcome::Hold(Hold::Cooldown));
        w.state.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 300);
        assert!(matches!(cli(&w), Outcome::Switch { .. }));
        w.state.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 10);
        w.state.usage.insert("a".into(), rec(100.0, 10.0));
        assert!(matches!(cli(&w), Outcome::Switch { .. }), "a hard limit bypasses the cooldown");
    }

    #[test]
    fn a_stale_reading_decides_nothing() {
        let mut w = world();
        let mut a = rec(99.0, 99.0);
        a.fetched_at = Some(T0 - STALE_AFTER_S - 1);
        w.state.usage.insert("a".into(), a);
        assert_eq!(cli(&w), Outcome::Hold(Hold::Stale));
        w.state.usage.remove("a");
        assert_eq!(cli(&w), Outcome::Hold(Hold::Stale));
    }

    #[test]
    fn all_stale_pauses_claude_but_one_fresh_account_keeps_it_running() {
        let mut w = world();
        for id in ["a", "b", "c"] {
            w.state.usage.get_mut(id).unwrap().fetched_at = Some(T0 - STALE_AFTER_S - 5);
        }
        assert_eq!(paused_providers(&w.config, &w.state, T0), vec![Provider::Claude]);
        w.state.usage.get_mut("c").unwrap().fetched_at = Some(T0 - 5);
        assert!(paused_providers(&w.config, &w.state, T0).is_empty());
    }

    #[test]
    fn codex_cli_mode_new_sessions_and_force_switch_now_and_notify_raises_an_event() {
        for (mode, expect_switch) in [(CodexCliMode::NewSessions, true), (CodexCliMode::Force, true), (CodexCliMode::Notify, false)] {
            let mut w = world();
            w.config.bind(SurfaceKind::CodexCli, Binding::parse("x")).unwrap();
            w.config.auto.codex_cli_mode = mode;
            w.state.set_active(SurfaceKind::CodexCli, Some("x".into()));
            w.state.usage.insert("x".into(), rec(95.0, 10.0));
            let d = decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap();
            match (&d.outcome, expect_switch) {
                (Outcome::Switch { split, .. }, true) => assert!(!split, "{mode:?}: a separate home needs no split"),
                (Outcome::Notify { candidates, follows_cli, .. }, false) => assert_eq!((candidates.as_slice(), *follows_cli), (["y".to_string(), "z".to_string()].as_slice(), false)),
                other => panic!("{mode:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn desktop_surfaces_only_ever_get_an_event() {
        let mut w = world();
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("a")).unwrap();
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("x")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("a".into()));
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("x".into()));
        w.state.usage.insert("a".into(), rec(95.0, 10.0));
        w.state.usage.insert("x".into(), rec(95.0, 10.0));
        for d in decide(&w.config, &w.state, T0).into_iter().filter(|d| d.surface.is_desktop()) {
            assert!(matches!(d.outcome, Outcome::Notify { .. }), "{:?}", d);
        }
    }

    #[test]
    fn an_unexpired_event_is_not_raised_twice_and_an_expired_one_is_replaced() {
        let mut w = world();
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("a")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("a".into()));
        w.state.usage.insert("a".into(), rec(95.0, 10.0));
        let ev = |exp| Pending { id: "e".into(), target_account: "b".into(), reason: REASON_SWITCH_NEEDED.into(), from: Some("a".into()), created_at: T0 - 100, expires_at: Some(exp) };
        let desktop = |w: &World| decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::ClaudeDesktop).unwrap().outcome;
        w.state.set_pending(SurfaceKind::ClaudeDesktop, Some(ev(T0 + 800)));
        assert_eq!(desktop(&w), Outcome::Hold(Hold::EventPending));
        w.state.set_pending(SurfaceKind::ClaudeDesktop, Some(ev(T0)));
        assert!(matches!(desktop(&w), Outcome::Notify { .. }));
    }

    fn together() -> World {
        let mut w = world();
        w.config.bind(SurfaceKind::CodexCli, Binding::parse("x")).unwrap();
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("x")).unwrap();
        w.state.set_active(SurfaceKind::CodexCli, Some("x".into()));
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("x".into()));
        w.state.usage.insert("x".into(), rec(95.0, 10.0));
        w
    }

    #[test]
    fn bound_together_the_cli_splits_now_and_the_app_waits_for_the_click() {
        let w = together();
        let ds = decide(&w.config, &w.state, T0);
        let cli = ds.iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap();
        let app = ds.iter().find(|d| d.surface == SurfaceKind::ChatgptDesktop).unwrap();
        assert!(matches!(&cli.outcome, Outcome::Switch { split: true, .. }));
        assert!(matches!(&app.outcome, Outcome::Notify { follows_cli: true, .. }));
    }

    #[test]
    fn bound_together_in_notify_mode_only_the_app_gets_an_event() {
        let mut w = together();
        w.config.auto.codex_cli_mode = CodexCliMode::Notify;
        let ds = decide(&w.config, &w.state, T0);
        assert_eq!(ds.iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap().outcome, Outcome::Hold(Hold::FollowsDesktop));
        assert!(matches!(&ds.iter().find(|d| d.surface == SurfaceKind::ChatgptDesktop).unwrap().outcome, Outcome::Notify { follows_cli: false, .. }));
    }

    #[test]
    fn an_account_live_on_another_token_surface_is_not_a_candidate_except_for_the_rejoin() {
        let mut w = world();
        w.config.bind(SurfaceKind::CodexCli, Binding::parse("x")).unwrap();
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("y")).unwrap();
        w.state.set_active(SurfaceKind::CodexCli, Some("x".into()));
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("y".into()));
        w.state.usage.insert("x".into(), rec(95.0, 10.0));
        let d = decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap();
        assert_eq!(switch_to(&d.outcome), ["z"], "y is the app's login");

        // Split: the CLI moved to z while the app stays on y; the app's event goes to z first.
        let mut w = together();
        w.state.set_active(SurfaceKind::CodexCli, Some("z".into()));
        w.state.set_pending(SurfaceKind::CodexCli, Some(Pending { id: "r".into(), target_account: "z".into(), reason: REASON_REJOIN.into(), from: Some("x".into()), created_at: T0 - 10, expires_at: None }));
        assert_eq!(candidates(&w.config, &w.state, SurfaceKind::ChatgptDesktop, "x", T0), ["z", "y"]);
        assert_eq!(candidates(&w.config, &w.state, SurfaceKind::CodexCli, "z", T0), ["y"], "x is still the app's login and z is the CLI's own: only y is free");
    }

    #[test]
    fn candidates_follow_the_priority_of_each_surface() {
        let mut w = world();
        w.state.usage.insert("a".into(), rec(91.0, 10.0));
        w.config.priority.claude_cli = vec!["c".into(), "a".into(), "b".into()];
        assert_eq!(switch_to(&cli(&w)), ["c", "b"]);
        // The desktop surface keeps its own order (config order here).
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("a")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("a".into()));
        let d = decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::ClaudeDesktop).unwrap();
        assert_eq!(switch_to(&d.outcome), ["b", "c"]);
    }

    #[test]
    fn a_surface_with_auto_off_gets_no_decision_while_the_others_still_act() {
        let mut w = world();
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("b")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("b".into()));
        w.state.usage.insert("a".into(), rec(95.0, 10.0));
        w.state.usage.insert("b".into(), rec(95.0, 10.0));
        w.config.auto.set_enabled_for(SurfaceKind::ClaudeCli, false);
        let ds = decide(&w.config, &w.state, T0);
        assert_eq!(ds.iter().find(|d| d.surface == SurfaceKind::ClaudeCli).unwrap().outcome, Outcome::Hold(Hold::AutoOff));
        assert!(matches!(ds.iter().find(|d| d.surface == SurfaceKind::ClaudeDesktop).unwrap().outcome, Outcome::Notify { back: false, .. }));
        // With every account stale, a provider whose surfaces all have auto off is not reported as paused.
        for id in ["a", "b", "c"] {
            w.state.usage.get_mut(id).unwrap().fetched_at = Some(T0 - STALE_AFTER_S - 5);
        }
        assert_eq!(paused_providers(&w.config, &w.state, T0), vec![Provider::Claude]);
        w.config.auto.set_enabled_for(SurfaceKind::ClaudeDesktop, false);
        assert!(paused_providers(&w.config, &w.state, T0).is_empty());
    }

    #[test]
    fn the_antigravity_cli_switches_down_its_own_priority_returns_and_pauses_on_its_own() {
        let mut w = world();
        for id in ["g1", "g2", "g3"] {
            w.config.add_account(acct(id, Provider::Antigravity)).unwrap();
            w.state.usage.insert(id.into(), rec(10.0, 10.0));
        }
        w.config.bind(SurfaceKind::AntigravityCli, Binding::parse("g2")).unwrap();
        w.config.priority.antigravity_cli = vec!["g2".into(), "g3".into(), "g1".into()];
        w.state.set_active(SurfaceKind::AntigravityCli, Some("g2".into()));
        let agy = |w: &World| decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::AntigravityCli).unwrap().outcome;
        assert_eq!(agy(&w), Outcome::Hold(Hold::NoTrigger));
        w.state.usage.insert("g2".into(), rec(10.0, 95.0));
        assert_eq!(agy(&w), Outcome::Switch { from: "g2".into(), candidates: vec!["g3".into(), "g1".into()], split: false, back: false });
        // On g3, once g2 has room again, it goes back up.
        w.state.set_active(SurfaceKind::AntigravityCli, Some("g3".into()));
        w.state.last_auto_switch.insert(SurfaceKind::AntigravityCli, T0 - 300);
        w.state.usage.insert("g2".into(), rec(10.0, 40.0));
        assert_eq!(agy(&w), Outcome::Switch { from: "g3".into(), candidates: vec!["g2".into()], split: false, back: true });
        w.config.auto.set_enabled_for(SurfaceKind::AntigravityCli, false);
        assert_eq!(agy(&w), Outcome::Hold(Hold::AutoOff));
        w.config.auto.set_enabled_for(SurfaceKind::AntigravityCli, true);
        for id in ["g1", "g2", "g3"] {
            w.state.usage.get_mut(id).unwrap().fetched_at = Some(T0 - STALE_AFTER_S - 5);
        }
        assert_eq!(paused_providers(&w.config, &w.state, T0), vec![Provider::Antigravity]);
    }

    /// The Claude CLI on b, below its line, after a switch away from a long enough ago.
    fn on_b() -> World {
        let mut w = world();
        w.state.set_active(SurfaceKind::ClaudeCli, Some("b".into()));
        w.state.usage.insert("b".into(), rec(40.0, 10.0));
        w.state.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 300);
        w
    }

    #[test]
    fn a_cli_surface_returns_to_a_higher_priority_account_that_has_room_again() {
        let w = on_b();
        assert_eq!(cli(&w), Outcome::Switch { from: "b".into(), candidates: vec!["a".into()], split: false, back: true });
        // Only accounts ranked above the current one count: c has room but is lower.
        let mut w = on_b();
        w.state.usage.insert("a".into(), rec(95.0, 10.0));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger));
        // A changed priority changes where it returns to.
        let mut w = on_b();
        w.config.priority.claude_cli = vec!["c".into(), "a".into(), "b".into()];
        assert_eq!(switch_to(&cli(&w)), ["c", "a"]);
        // At the threshold the normal rule applies, not the return.
        let mut w = on_b();
        w.state.usage.insert("b".into(), rec(91.0, 10.0));
        assert!(matches!(cli(&w), Outcome::Switch { back: false, .. }));
    }

    #[test]
    fn no_return_during_the_cooldown_or_to_an_account_that_is_stale_unread_dead_or_at_its_line() {
        let mut w = on_b();
        w.state.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 299);
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "cooldown");

        let mut w = on_b();
        w.state.usage.get_mut("a").unwrap().fetched_at = Some(T0 - STALE_AFTER_S - 1);
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "stale reading");

        let mut w = on_b();
        w.state.usage.remove("a");
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "never read");

        let mut w = on_b();
        w.state.usage.get_mut("a").unwrap().state = UsageState::Dead;
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "dead");

        let mut w = on_b();
        w.state.usage.get_mut("a").unwrap().state = UsageState::Unreadable;
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "unreadable");

        // Hysteresis: 85 is not below threshold minus hysteresis; 84 is.
        let mut w = on_b();
        w.state.usage.insert("a".into(), rec(85.0, 10.0));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "at the line");
        w.state.usage.insert("a".into(), rec(84.0, 10.0));
        assert!(matches!(cli(&w), Outcome::Switch { back: true, .. }));

        // Auto off for the surface: no return either.
        let mut w = on_b();
        w.config.auto.set_enabled_for(SurfaceKind::ClaudeCli, false);
        assert_eq!(cli(&w), Outcome::Hold(Hold::AutoOff));
    }

    #[test]
    fn a_desktop_surface_gets_an_event_to_return_and_not_while_one_waits() {
        let mut w = world();
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("a")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("b".into()));
        let desktop = |w: &World| decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::ClaudeDesktop).unwrap().outcome;
        assert_eq!(desktop(&w), Outcome::Hold(Hold::NoTrigger), "a is the Claude CLI's live login");
        w.state.set_active(SurfaceKind::ClaudeCli, Some("c".into()));
        assert_eq!(desktop(&w), Outcome::Notify { from: "b".into(), candidates: vec!["a".into()], follows_cli: false, back: true });
        let ev = Pending { id: "e".into(), target_account: "a".into(), reason: REASON_SWITCH_NEEDED.into(), from: Some("b".into()), created_at: T0 - 100, expires_at: Some(T0 + 800) };
        w.state.set_pending(SurfaceKind::ClaudeDesktop, Some(ev));
        assert_eq!(desktop(&w), Outcome::Hold(Hold::NoTrigger));
    }

    #[test]
    fn a_return_skips_an_account_live_on_another_token_surface_and_keeps_the_codex_rules() {
        // The Codex CLI on y; x ranks higher and has room, but the ChatGPT app is logged in to it.
        let mut w = world();
        w.config.bind(SurfaceKind::CodexCli, Binding::parse("y")).unwrap();
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("x")).unwrap();
        w.config.priority.codex_cli = vec!["x".into(), "y".into()];
        w.state.set_active(SurfaceKind::CodexCli, Some("y".into()));
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("x".into()));
        let codex = |w: &World| decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap().outcome;
        assert_eq!(codex(&w), Outcome::Hold(Hold::NoTrigger));
        // With the app on z instead, x is free and the CLI returns with no split.
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("z")).unwrap();
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("z".into()));
        assert_eq!(codex(&w), Outcome::Switch { from: "y".into(), candidates: vec!["x".into()], split: false, back: true });
    }

    #[test]
    fn a_codex_cli_that_shares_its_home_never_splits_off_to_return() {
        // Bound together on y, with x ranked higher for both surfaces and room on x.
        let mut w = world();
        w.config.bind(SurfaceKind::CodexCli, Binding::parse("y")).unwrap();
        w.config.bind(SurfaceKind::ChatgptDesktop, Binding::parse("y")).unwrap();
        w.config.priority.codex_cli = vec!["x".into(), "y".into()];
        w.config.priority.chatgpt_desktop = vec!["x".into(), "y".into()];
        w.state.set_active(SurfaceKind::CodexCli, Some("y".into()));
        w.state.set_active(SurfaceKind::ChatgptDesktop, Some("y".into()));
        let ds = decide(&w.config, &w.state, T0);
        let of = |k| ds.iter().find(|d| d.surface == k).unwrap().outcome.clone();
        assert_eq!(of(SurfaceKind::CodexCli), Outcome::Hold(Hold::NoTrigger), "no split for a return");
        // The app's event moves both, since they share ~/.codex; it does not follow a CLI that never moved.
        assert_eq!(of(SurfaceKind::ChatgptDesktop), Outcome::Notify { from: "y".into(), candidates: vec!["x".into()], follows_cli: false, back: true });
        // At the limit, the split stays the rule.
        w.state.usage.insert("y".into(), rec(95.0, 10.0));
        assert!(matches!(decide(&w.config, &w.state, T0).into_iter().find(|d| d.surface == SurfaceKind::CodexCli).unwrap().outcome, Outcome::Switch { split: true, back: false, .. }));
    }

    #[test]
    fn a_surface_bound_below_the_first_account_of_an_old_config_does_not_return() {
        // No [priority] table: the bound account is the first priority, so nothing moves it away.
        let mut w = world();
        w.config.bind(SurfaceKind::ClaudeCli, Binding::parse("b")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeCli, Some("b".into()));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger));
        // After a switch away from b at its limit, it returns to b first, not to a.
        w.state.set_active(SurfaceKind::ClaudeCli, Some("c".into()));
        assert_eq!(switch_to(&cli(&w)), ["b", "a"]);
    }

    #[test]
    fn a_return_skips_the_target_of_another_surfaces_waiting_event() {
        let mut w = on_b();
        w.config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("c")).unwrap();
        w.state.set_active(SurfaceKind::ClaudeDesktop, Some("c".into()));
        let ev = |exp| Pending { id: "e".into(), target_account: "a".into(), reason: REASON_SWITCH_NEEDED.into(), from: Some("c".into()), created_at: T0 - 100, expires_at: Some(exp) };
        w.state.set_pending(SurfaceKind::ClaudeDesktop, Some(ev(T0 + 800)));
        assert_eq!(cli(&w), Outcome::Hold(Hold::NoTrigger), "the app may move to a on a click");
        // Once that event expired, the CLI may return.
        w.state.set_pending(SurfaceKind::ClaudeDesktop, Some(ev(T0)));
        assert!(matches!(cli(&w), Outcome::Switch { back: true, .. }));
    }

    // ----- cadence -----

    fn fetched(prev: Option<&UsageRecord>, w: &Windows, active: bool, now: u64) -> (u64, u64, u32) {
        plan_after_fetch(prev, w, active, 90, now, 0.0)
    }

    #[test]
    fn the_interval_never_drops_below_180_seconds_unless_urgent() {
        // Movement halves it, but only down to the floor.
        let mut prev = rec(10.0, 10.0);
        prev.interval_s = 200;
        let (next, interval, _) = fetched(Some(&prev), &windows(30.0, 10.0), true, T0);
        assert_eq!(interval, 180);
        assert!(next >= T0 + 180);
        // Jitter only adds.
        let (next, ..) = plan_after_fetch(Some(&prev), &windows(30.0, 10.0), true, 90, T0, 0.999);
        assert!((T0 + 180..=T0 + 199).contains(&next));
    }

    #[test]
    fn a_still_account_backs_off_toward_its_ceiling() {
        let mut prev = rec(10.0, 10.0);
        prev.interval_s = 180;
        let w = windows(10.0, 10.0);
        assert_eq!(fetched(Some(&prev), &w, true, T0).1, 270);
        prev.interval_s = 270;
        assert_eq!(fetched(Some(&prev), &w, true, T0).1, 300, "the active ceiling");
        prev.interval_s = 400;
        assert_eq!(fetched(Some(&prev), &w, false, T0).1, 600, "an idle alternate drifts to ten minutes");
        assert_eq!(fetched(None, &w, false, T0).1, CANDIDATE_DEFAULT_INTERVAL_S);
        assert_eq!(fetched(None, &w, true, T0).1, MIN_INTERVAL_S);
    }

    #[test]
    fn urgent_mode_needs_a_rising_window_in_the_band_of_the_active_account() {
        let prev = rec(70.0, 10.0);
        let rising = windows(76.0, 10.0); // 75 is the band edge (90 - 15)
        assert_eq!(fetched(Some(&prev), &rising, true, T0).1, URGENT_INTERVAL_S);
        assert_eq!(fetched(Some(&prev), &rising, false, T0).1, 180, "an inactive account never gets urgent polls");
        let falling = windows(60.0, 10.0);
        assert_ne!(fetched(Some(&rec(80.0, 10.0)), &falling, true, T0).1, URGENT_INTERVAL_S, "a window that dropped just rolled over");
        let below_band = windows(60.0, 10.0);
        assert_ne!(fetched(Some(&rec(50.0, 10.0)), &below_band, true, T0).1, URGENT_INTERVAL_S);
        // A flat higher weekly window does not hide a rising 5-hour window.
        assert_eq!(fetched(Some(&rec(70.0, 95.0)), &windows(76.0, 95.0), true, T0).1, URGENT_INTERVAL_S);
    }

    #[test]
    fn urgent_polls_are_bounded_and_decay_back_to_the_floor() {
        let mut streak = 0;
        let mut urgent = 0;
        let mut interval = 180;
        for i in 0..40u32 {
            // The window rises one point per poll inside the band.
            let pct = 76.0 + f64::from(i % 10);
            let prev = UsageRecord { windows: windows(pct - 1.0, 10.0), interval_s: interval, urgent_streak: streak, ..rec(0.0, 0.0) };
            let (_, next_interval, next_streak) = fetched(Some(&prev), &windows(pct, 10.0), true, T0);
            if next_interval == URGENT_INTERVAL_S {
                urgent += 1;
            }
            interval = next_interval;
            streak = next_streak;
        }
        assert_eq!(urgent, MAX_URGENT_STREAK, "one episode takes at most 15 urgent polls");
        // Once the rise stops, the next poll is back at the normal cadence and the streak resets.
        let prev = UsageRecord { windows: windows(85.0, 10.0), interval_s: interval, urgent_streak: streak, ..rec(0.0, 0.0) };
        let (_, interval, streak) = fetched(Some(&prev), &windows(85.0, 10.0), true, T0);
        assert!(interval >= MIN_INTERVAL_S);
        assert_eq!(streak, 0);
    }

    #[test]
    fn a_429_stretches_the_interval_and_suppresses_urgent_mode() {
        let mut prev = rec(70.0, 10.0);
        prev.last_429_at = Some(T0 - 100);
        prev.interval_s = 180;
        let (_, interval, streak) = fetched(Some(&prev), &windows(76.0, 10.0), true, T0);
        assert_eq!((interval, streak), (360, 0), "floor of 360 s, no urgent polls");
        prev.interval_s = 360;
        assert_eq!(fetched(Some(&prev), &windows(70.0, 10.0), true, T0).1, 540, "x1.5 while 429s linger");
        prev.interval_s = 1700;
        assert_eq!(fetched(Some(&prev), &windows(70.0, 10.0), true, T0).1, POST_429_MAX_INTERVAL_S);
        // An hour later the 429 is forgotten.
        assert!(fetched(Some(&prev), &windows(70.0, 10.0), true, T0 + RECENT_429_WINDOW_S).1 <= 300);
    }

    #[test]
    fn a_failed_429_backs_off_and_honors_retry_after() {
        let prev = rec(10.0, 10.0);
        let mut failed = UsageRecord { state: UsageState::Unreadable, last_429_at: Some(T0), ..UsageRecord::default() };
        let (next, interval) = plan_after_failure(Some(&prev), &failed, None, T0, 0.0);
        assert_eq!((interval, next), (360, T0 + 360));
        let (_, interval) = plan_after_failure(Some(&prev), &failed, Some(900), T0, 0.0);
        assert_eq!(interval, 960, "Retry-After plus a minute");
        failed.last_429_at = None;
        failed.state = UsageState::Unreadable;
        assert_eq!(plan_after_failure(Some(&prev), &failed, None, T0, 0.0).1, 270, "other failures back off gently");
        failed.state = UsageState::Expired;
        assert_eq!(plan_after_failure(Some(&prev), &failed, None, T0, 0.0).1, 180);
    }

    #[test]
    fn an_offline_failure_retries_in_a_minute_and_keeps_the_interval() {
        let mut prev = rec(10.0, 10.0);
        prev.interval_s = 600;
        for kind in ["network", "timeout"] {
            let failed = UsageRecord { state: UsageState::Unreadable, detail: Some(kind.into()), ..UsageRecord::default() };
            assert_eq!(plan_after_failure(Some(&prev), &failed, None, T0, 0.0), (T0 + OFFLINE_RETRY_S, 600), "{kind}");
        }
        let failed = UsageRecord { state: UsageState::Unreadable, detail: Some("http-503".into()), ..UsageRecord::default() };
        assert_eq!(plan_after_failure(Some(&prev), &failed, None, T0, 0.0).1, 600, "a server error still backs off");
    }

    #[test]
    fn an_exhausted_account_is_polled_about_every_ten_minutes_and_pulled_in_by_its_reset() {
        let w = windows(100.0, 50.0);
        let (next, interval, _) = fetched(None, &w, false, T0);
        assert_eq!(interval, 600);
        assert_eq!(next, T0 + 600);
        // The 5-hour window resets in 400 s: poll just after it instead.
        let mut w = windows(100.0, 50.0);
        w.five_hour = Some(WindowSnap { pct: 100.0, resets_at: Some(T0 as i64 + 400) });
        let (next, ..) = fetched(None, &w, false, T0);
        assert_eq!(next, T0 + 460);
        // A reset a few seconds away never causes a poll sooner than the floor.
        w.five_hour = Some(WindowSnap { pct: 100.0, resets_at: Some(T0 as i64 + 5) });
        assert_eq!(fetched(None, &w, false, T0).0, T0 + 180);
    }

    #[test]
    fn next_due_is_the_earliest_poll_or_now_for_an_unread_account() {
        let mut w = world();
        for id in ["a", "b", "c", "x", "y", "z"] {
            w.state.usage.get_mut(id).unwrap().next_poll_at = T0 + 500;
        }
        w.state.usage.get_mut("c").unwrap().next_poll_at = T0 + 100;
        assert_eq!(next_due(&w.config, &w.state, T0), T0 + 100);
        w.state.usage.remove("y");
        assert_eq!(next_due(&w.config, &w.state, T0), T0);
    }

    #[test]
    fn confirmed_needs_a_fresh_ok_reading_with_headroom() {
        let auto = Auto::default();
        let ok = |p| UsageRecord { fetched_at: Some(T0), ..rec(p, 10.0) };
        assert!(confirmed(&ok(84.0), &auto, T0));
        assert!(!confirmed(&ok(85.0), &auto, T0));
        assert!(!confirmed(&UsageRecord { state: UsageState::Unreadable, ..ok(10.0) }, &auto, T0));
        assert!(!confirmed(&UsageRecord { fetched_at: Some(T0 - 300), ..ok(10.0) }, &auto, T0), "an old reading is not a confirmation");
    }
}
