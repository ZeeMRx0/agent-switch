//! `agent-switch list [--json]`: every saved account with its 5-hour and weekly usage, each window with its
//! own reset time, the usage state, and the surfaces it is bound to or active on.
//!
//! A stored reading younger than three minutes is shown as it is; otherwise the account is read again (one
//! request each, never a refresh: the login of an account that a CLI or app is using is read from there, and
//! a saved login that has expired shows as `expired` until a switch or an opt-in background refresh renews
//! it). When a reading fails, the last-known numbers stay on screen with their age.

use agent_switch_core::config::{mask_email, Account, Config};
use agent_switch_core::state::{now_secs, State, UsageRecord, UsageState, WindowSnap};
use agent_switch_core::usage::{self, age_s, human_secs, Ctx, LiveApi};
use agent_switch_core::Result;
use serde_json::{json, Value};

use crate::commands::App;
use crate::registry::surface_for;

pub fn list(as_json: bool) -> Result<i32> {
    let app = App::open()?;
    let config = Config::load(&app.paths)?;
    if config.accounts.is_empty() {
        if as_json {
            println!("[]");
        } else {
            println!("no accounts yet; add one with `agent-switch add <surface>`");
        }
        return Ok(0);
    }
    let env = app.env();
    let api = LiveApi::production();
    let factory = |k| surface_for(k);
    let ctx = Ctx { env: &env, config: &config, api: &api, surfaces: &factory };
    let now = now_secs();

    let mut rows = Vec::new();
    for account in &config.accounts {
        let stored = State::load(&app.paths)?.usage.get(&account.id).cloned();
        let rec = if usage::needs_reading(stored.as_ref(), now) {
            match usage::poll_account(&ctx, account, now) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("note: cannot read the usage of `{}`: {e}", account.id);
                    stored
                }
            }
        } else {
            stored
        };
        rows.push((account, rec));
    }

    let state = State::load(&app.paths)?;
    if as_json {
        let out: Vec<Value> = rows.iter().map(|(a, r)| json_row(a, r.as_ref(), &config, &state, now)).collect();
        println!("{}", serde_json::to_string_pretty(&out).map_err(|e| agent_switch_core::Error::Invalid(e.to_string()))?);
    } else {
        for (a, r) in &rows {
            print_row(a, r.as_ref(), &config, &state, now);
        }
    }
    Ok(0)
}

fn names(kinds: Vec<agent_switch_core::config::SurfaceKind>) -> Vec<&'static str> {
    kinds.into_iter().map(|k| k.name()).collect()
}

fn reset_text(snap: &WindowSnap, now: u64) -> String {
    match snap.resets_at {
        Some(t) if t > now as i64 => format!("resets in {}", human_secs((t - now as i64) as u64)),
        Some(_) => "window has reset".into(),
        None => "reset time unknown".into(),
    }
}

fn window_text(label: &str, snap: Option<&WindowSnap>, now: u64) -> String {
    match snap {
        None => format!("  {label:<7} -"),
        Some(s) => {
            let shown = match s.resets_at {
                Some(t) if t <= now as i64 => 0.0,
                _ => s.pct,
            };
            format!("  {label:<7} {shown:>3.0}%   {}", reset_text(s, now))
        }
    }
}

fn state_text(rec: Option<&UsageRecord>, now: u64) -> String {
    let Some(r) = rec else { return "no reading yet".into() };
    let age = age_s(r, now).map(|a| if a < 60 { "just now".to_string() } else { format!("{} ago", human_secs(a)) });
    let why = r.detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
    match (r.state, age) {
        (UsageState::Ok, Some(a)) => format!("ok, read {a}"),
        (UsageState::Ok, None) => "ok".into(),
        (s, Some(a)) => format!("{s}{why} (last numbers read {a})"),
        (s, None) => format!("{s}{why}"),
    }
}

fn print_row(a: &Account, rec: Option<&UsageRecord>, config: &Config, state: &State, now: u64) {
    let email = a.email.as_deref().map(mask_email).unwrap_or_else(|| "-".into());
    println!("{}  ({}, {}, {})", a.id, a.provider, a.label, email);
    let bound = names(config.surfaces_bound_to(&a.id));
    let active = names(state.is_active_anywhere(&a.id));
    println!("  bound: {}   active: {}", if bound.is_empty() { "-".into() } else { bound.join(", ") }, if active.is_empty() { "-".into() } else { active.join(", ") });
    let w = rec.map(|r| r.windows).unwrap_or_default();
    println!("{}", window_text("5-hour", w.five_hour.as_ref(), now));
    println!("{}", window_text("weekly", w.weekly.as_ref(), now));
    println!("  state   {}", state_text(rec, now));
    println!();
}

fn window_json(snap: Option<&WindowSnap>, now: u64) -> Value {
    match snap {
        None => Value::Null,
        Some(s) => {
            let reset = s.resets_at.filter(|t| *t > now as i64);
            json!({
                "percent": if s.resets_at.is_some_and(|t| t <= now as i64) { 0.0 } else { s.pct },
                "last_percent": s.pct,
                "resets_at": s.resets_at,
                "resets_in_s": reset.map(|t| t - now as i64),
            })
        }
    }
}

fn json_row(a: &Account, rec: Option<&UsageRecord>, config: &Config, state: &State, now: u64) -> Value {
    let w = rec.map(|r| r.windows).unwrap_or_default();
    json!({
        "id": a.id,
        "provider": a.provider.to_string(),
        "label": a.label,
        "email": a.email.as_deref().map(mask_email),
        "bound": names(config.surfaces_bound_to(&a.id)),
        "active": names(state.is_active_anywhere(&a.id)),
        "state": rec.map(|r| r.state.to_string()),
        "detail": rec.and_then(|r| r.detail.clone()),
        "five_hour": window_json(w.five_hour.as_ref(), now),
        "weekly": window_json(w.weekly.as_ref(), now),
        "fetched_at": rec.and_then(|r| r.fetched_at),
        "age_s": rec.and_then(|r| age_s(r, now)),
    })
}
