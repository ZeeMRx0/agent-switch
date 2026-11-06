//! The menu as data. `build` turns the saved config and state into `Entry` values; `render` turns those
//! into a `muda` menu. Keeping the two apart lets the tests check every label and action without a screen.
//!
//! Account labels and server-side names are shown as plain text. A `muda` title treats `&` as a mnemonic
//! marker, so `escape_title` doubles it; nothing here is ever passed to a shell or to AppleScript.

use std::collections::HashMap;

use agent_switch_core::config::{Account, Binding, CodexCliMode, Config, Provider, SurfaceKind};
use agent_switch_core::policy;
use agent_switch_core::state::{State, UsageRecord, UsageState, REASON_SWITCH_NEEDED};
use muda::{CheckMenuItem, ContextMenu, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::login_item::LoginStatus;

/// Longest account label shown in a menu row.
const MAX_LABEL_CHARS: usize = 48;
/// Longest notice line shown at the top of the menu.
const MAX_NOTICE_CHARS: usize = 100;
const RELAUNCH_SUFFIX: &str = " (relaunches app)";

/// What a menu click asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Choose the account of a surface, or `unmanaged`.
    Bind(SurfaceKind, Binding),
    SetAuto(bool),
    SetCodexMode(CodexCliMode),
    /// Quit the app, switch it to its bound account and open it again.
    RelaunchApp(SurfaceKind),
    /// Desktop surfaces: bind the account, then quit and reopen the app on it. Clicking the account is the
    /// user's consent to the relaunch.
    SwitchApp(SurfaceKind, String),
    /// Like `RelaunchApp` (or `SwitchApp` when `account` is set), but also ends work in progress (a Claude
    /// Desktop session that is mid-reply). `titles` are the sessions the user saw offered; the worker
    /// refuses again if a different one is busy by then.
    ForceRelaunch { kind: SurfaceKind, account: Option<String>, titles: String },
    /// Open Claude on the profile of its bound account.
    OpenClaude,
    Undo(SurfaceKind),
    /// Answer a pending switch-needed event, like the notification button does.
    Relaunch { kind: SurfaceKind, event_id: String },
    SetLoginItem(bool),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Separator,
    /// A heading or a note; never clickable.
    Label(String),
    Item { text: String, action: Option<Action>, checked: Option<bool>, enabled: bool },
    Submenu { text: String, entries: Vec<Entry> },
    /// Level bars for the usage row just above. They are drawn as native bars, and `to_text` omits them
    /// because that row already shows the same numbers.
    Bars(Vec<Bar>),
}

/// How much of a usage window is left, as the color of its bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Green.
    Plenty,
    /// Yellow.
    Low,
    /// Red: auto-switch moves surfaces away from the account at this point.
    Critical,
}

/// One usage window as a level bar. `used` is the percent shown in the label; the bar shows what is left.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// `5h` or `7d`.
    pub window: &'static str,
    /// The used percent, 0-100, rounded like the menu text.
    pub used: u8,
    pub tier: Tier,
}

impl Bar {
    /// The percent left, which is how full the bar is drawn.
    pub fn remaining(&self) -> u8 {
        100 - self.used
    }
}

/// How many points above the red limit a bar turns yellow.
const LOW_MARGIN: u8 = 20;

/// The color of a bar with `remaining` percent left. Red when it is at or below what auto-switch leaves
/// (`100 - threshold`, so 10% for the default threshold of 90); yellow up to `LOW_MARGIN` points above
/// that; green otherwise.
pub fn tier(remaining: u8, threshold: u8) -> Tier {
    let critical = 100u8.saturating_sub(threshold);
    if remaining <= critical {
        Tier::Critical
    } else if remaining <= critical.saturating_add(LOW_MARGIN) {
        Tier::Low
    } else {
        Tier::Plenty
    }
}

/// The bars of a reading: one per window it has. None when there is no reading or it is not a clean one
/// (expired, dead, unreadable); the text row says why.
pub fn usage_bars(rec: Option<&UsageRecord>, now: u64, threshold: u8) -> Vec<Bar> {
    let Some(r) = rec.filter(|r| r.state == UsageState::Ok) else { return Vec::new() };
    [("5h", &r.windows.five_hour), ("7d", &r.windows.weekly)]
        .into_iter()
        .filter_map(|(window, snap)| {
            // `{:.0}` in `usage_text` rounds ties to even, so the label matches the text row.
            let used = snap.as_ref()?.pct_at(now).round_ties_even().clamp(0.0, 100.0) as u8;
            Some(Bar { window, used, tier: tier(100 - used, threshold) })
        })
        .collect()
}

/// `entry` followed by a bar row when `bars` is not empty.
fn with_bars(entry: Entry, bars: Vec<Bar>) -> Vec<Entry> {
    let mut out = vec![entry];
    if !bars.is_empty() {
        out.push(Entry::Bars(bars));
    }
    out
}

impl Entry {
    fn item(text: impl Into<String>, action: Action) -> Entry {
        Entry::Item { text: text.into(), action: Some(action), checked: None, enabled: true }
    }

    fn disabled(text: impl Into<String>) -> Entry {
        Entry::Item { text: text.into(), action: None, checked: None, enabled: false }
    }

    /// A check item. Clicking the one that is already checked does nothing.
    fn choice(text: impl Into<String>, checked: bool, action: Action) -> Entry {
        Entry::Item { text: text.into(), action: (!checked).then_some(action), checked: Some(checked), enabled: true }
    }
}

/// How long a refused switch can still be forced from the menu.
pub const FORCE_OFFER_SECS: u64 = 600;

/// A Desktop switch that was refused because work is in progress; the menu offers to force it.
#[derive(Debug, Clone, PartialEq)]
pub struct ForceOffer {
    pub kind: SurfaceKind,
    /// The account of the refused `SwitchApp` click; `None` for a relaunch of the bound account.
    pub account: Option<String>,
    /// The titles of the sessions that would end.
    pub titles: String,
    /// When the refusal happened (unix seconds).
    pub since: u64,
}

/// What the menu needs besides the config and the state.
pub struct Ctx {
    pub now: u64,
    pub login: LoginStatus,
    /// False once the watcher thread has ended.
    pub watcher_running: bool,
    /// The result of the latest click, shown as a disabled line because notifications may be denied.
    pub notice: Option<String>,
    /// A refused busy switch that the user may force; ignored once it is older than `FORCE_OFFER_SECS`.
    pub force_offer: Option<ForceOffer>,
}

/// Whether the tray menu must be rebuilt. A click forces it: the native check item flips by itself when
/// clicked, so only a rebuild puts its check mark back in line with the model.
pub fn should_rebuild(new: &[Entry], shown: &[Entry], forced: bool) -> bool {
    forced || new != shown
}

pub fn surface_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::ClaudeCli => "Claude CLI",
        SurfaceKind::ClaudeDesktop => "Claude Desktop",
        SurfaceKind::CodexCli => "Codex CLI",
        SurfaceKind::ChatgptDesktop => "ChatGPT",
    }
}

/// The label of an account, or its id when it has none. Control characters become spaces and a long
/// label is cut, so one row stays one line.
pub fn account_label(account: &Account) -> String {
    let label = account.label.trim();
    clean(if label.is_empty() { &account.id } else { label })
}

/// The label of the account `id`, or the id itself when the account is gone.
pub fn label_of(config: &Config, id: &str) -> String {
    config.account(id).map_or_else(|| clean(id), account_label)
}

fn clean(text: &str) -> String {
    clean_to(text, MAX_LABEL_CHARS)
}

fn clean_to(text: &str, max: usize) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max - 1).collect();
        format!("{cut}…")
    } else {
        flat.to_string()
    }
}

/// `5h 32% · 7d 40%`, with a note when the reading is not a clean one.
pub fn usage_text(rec: Option<&UsageRecord>, now: u64) -> String {
    let Some(r) = rec else { return "no reading yet".into() };
    let mut parts = Vec::new();
    if let Some(w) = &r.windows.five_hour {
        parts.push(format!("5h {:.0}%", w.pct_at(now)));
    }
    if let Some(w) = &r.windows.weekly {
        parts.push(format!("7d {:.0}%", w.pct_at(now)));
    }
    let mut text = if parts.is_empty() { "no reading yet".to_string() } else { parts.join(" · ") };
    match r.state {
        UsageState::Ok => {}
        UsageState::Expired => text.push_str(" (login expired)"),
        UsageState::Dead => text.push_str(" (sign in again)"),
        UsageState::Unreadable => text.push_str(" (unreadable)"),
    }
    text
}

fn provider_title(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Openai => "ChatGPT and Codex",
    }
}

/// The surface's submenu, followed by the bars of its current account.
fn surface_entry(config: &Config, state: &State, kind: SurfaceKind, now: u64) -> Vec<Entry> {
    let threshold = config.auto.threshold;
    let current = policy::current_account(config, state, kind);
    let title = match &current {
        Some(id) => format!("{}: {} · {}", surface_name(kind), label_of(config, id), usage_text(state.usage.get(id), now)),
        None => format!("{}: unmanaged", surface_name(kind)),
    };
    let mut entries: Vec<Entry> = config
        .accounts
        .iter()
        .filter(|a| a.provider == kind.provider())
        .flat_map(|a| {
            let rec = state.usage.get(&a.id);
            let text = format!("{} · {}", account_label(a), usage_text(rec, now));
            let active = current.as_deref() == Some(a.id.as_str());
            let row = if kind.is_desktop() {
                // Clicking an account here quits and reopens the app, so the row says so.
                let text = if active { text } else { format!("{text}{RELAUNCH_SUFFIX}") };
                Entry::choice(text, active, Action::SwitchApp(kind, a.id.clone()))
            } else {
                Entry::choice(text, active, Action::Bind(kind, Binding::Account(a.id.clone())))
            };
            with_bars(row, usage_bars(rec, now, threshold))
        })
        .collect();
    if entries.is_empty() {
        entries.push(Entry::disabled("No saved accounts"));
    }
    entries.push(Entry::Separator);
    entries.push(Entry::choice("Unmanaged (do not touch)", current.is_none(), Action::Bind(kind, Binding::Unmanaged)));
    let bars = current.as_deref().map_or_else(Vec::new, |id| usage_bars(state.usage.get(id), now, threshold));
    with_bars(Entry::Submenu { text: title, entries }, bars)
}

/// The open question for `kind`, as a row that does what the notification button does.
fn pending_entry(config: &Config, state: &State, kind: SurfaceKind, now: u64) -> Option<Entry> {
    let p = state.pending(kind).filter(|p| p.reason == REASON_SWITCH_NEEDED && !p.is_expired(now))?;
    let target = label_of(config, &p.target_account);
    let text = if kind.is_desktop() {
        format!("Relaunch {} as {target}…", surface_name(kind))
    } else {
        format!("Switch {} to {target}…", surface_name(kind))
    };
    Some(Entry::item(text, Action::Relaunch { kind, event_id: p.id.clone() }))
}

fn app_row(config: &Config, kind: SurfaceKind, text: &str, action: Action) -> Entry {
    if config.surfaces.get(kind).account_id().is_some() {
        Entry::item(text, action)
    } else {
        Entry::disabled(format!("{text} (bind {} first)", surface_name(kind)))
    }
}

fn undo_entry(config: &Config, state: &State) -> Entry {
    match state.undo_target(None) {
        Some(r) => match r.previous_account_id.as_deref() {
            Some(prev) => Entry::item(
                format!("Undo last switch: {} back to {}", surface_name(r.surface), label_of(config, prev)),
                Action::Undo(r.surface),
            ),
            None => Entry::disabled("Undo last switch (no earlier account)"),
        },
        None => Entry::disabled("Undo last switch"),
    }
}

/// A check item whose click flips it, so it stays clickable while checked.
fn toggle(text: &str, checked: bool, action: Action) -> Entry {
    Entry::Item { text: text.into(), action: Some(action), checked: Some(checked), enabled: true }
}

fn login_entry(login: &LoginStatus) -> Entry {
    match login {
        LoginStatus::Enabled => toggle("Start at login", true, Action::SetLoginItem(false)),
        LoginStatus::Disabled => toggle("Start at login", false, Action::SetLoginItem(true)),
        LoginStatus::NeedsApproval => toggle("Start at login (approve in System Settings)", false, Action::SetLoginItem(true)),
        LoginStatus::Unavailable(why) => Entry::disabled(format!("Start at login (unavailable: {why})")),
    }
}

fn footer(config: &Config, state: &State, ctx: &Ctx) -> Vec<Entry> {
    let auto = &config.auto;
    let mode = |mode: CodexCliMode, text: &str| Entry::choice(text, auto.codex_cli_mode == mode, Action::SetCodexMode(mode));
    vec![
        toggle("Auto-switch", auto.enabled, Action::SetAuto(!auto.enabled)),
        Entry::Submenu {
            text: "Codex CLI auto mode".into(),
            entries: vec![
                mode(CodexCliMode::NewSessions, "New sessions only"),
                mode(CodexCliMode::Notify, "Notify only"),
                mode(CodexCliMode::Force, "Force (running sessions may need a new sign-in)"),
            ],
        },
        undo_entry(config, state),
        Entry::Separator,
        login_entry(&ctx.login),
        Entry::item("Quit agent-switch", Action::Quit),
    ]
}

/// The whole menu.
pub fn build(config: &Config, state: &State, ctx: &Ctx) -> Vec<Entry> {
    let mut out = Vec::new();
    if let Some(offer) = ctx.force_offer.as_ref().filter(|o| ctx.now.saturating_sub(o.since) < FORCE_OFFER_SECS) {
        let titles = clean_to(&offer.titles, MAX_NOTICE_CHARS - 30);
        out.push(Entry::item(format!("Switch anyway (ends: {titles})"), Action::ForceRelaunch { kind: offer.kind, account: offer.account.clone(), titles: offer.titles.clone() }));
        out.push(Entry::Separator);
    }
    if let Some(notice) = &ctx.notice {
        out.push(Entry::Label(format!("Last: {}", clean_to(notice, MAX_NOTICE_CHARS))));
        out.push(Entry::Separator);
    }
    if !ctx.watcher_running {
        out.push(Entry::Label("The watcher stopped; quit and open agent-switch again".into()));
        out.push(Entry::Separator);
    }
    for provider in [Provider::Claude, Provider::Openai] {
        out.push(Entry::Label(provider_title(provider).into()));
        for kind in SurfaceKind::ALL.into_iter().filter(|k| k.provider() == provider) {
            out.extend(surface_entry(config, state, kind, ctx.now));
            out.extend(pending_entry(config, state, kind, ctx.now));
        }
        match provider {
            Provider::Claude => {
                out.push(app_row(config, SurfaceKind::ClaudeDesktop, "Relaunch Claude", Action::RelaunchApp(SurfaceKind::ClaudeDesktop)));
                out.push(app_row(config, SurfaceKind::ClaudeDesktop, "Open Claude (bound profile)", Action::OpenClaude));
            }
            Provider::Openai => {
                out.push(app_row(config, SurfaceKind::ChatgptDesktop, "Relaunch ChatGPT", Action::RelaunchApp(SurfaceKind::ChatgptDesktop)));
            }
        }
        out.push(Entry::Separator);
    }
    out.extend(footer(config, state, ctx));
    out
}

/// The menu when the config or the state cannot be read: the problem and the way out.
pub fn error_menu(message: &str, ctx: &Ctx) -> Vec<Entry> {
    vec![
        Entry::Label("agent-switch cannot read its files".into()),
        Entry::Label(clean(message)),
        Entry::Separator,
        login_entry(&ctx.login),
        Entry::item("Quit agent-switch", Action::Quit),
    ]
}

/// Plain text of the menu, for `--print-menu` and for reading test failures.
pub fn to_text(entries: &[Entry]) -> String {
    fn walk(entries: &[Entry], depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        for e in entries {
            match e {
                Entry::Separator => out.push_str(&format!("{pad}--------\n")),
                Entry::Label(t) => out.push_str(&format!("{pad}{t}\n")),
                Entry::Item { text, checked, enabled, action } => {
                    let mark = match checked {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let state = if !enabled {
                        " (disabled)"
                    } else if action.is_none() {
                        " (current)"
                    } else {
                        ""
                    };
                    out.push_str(&format!("{pad}{mark}{text}{state}\n"));
                }
                Entry::Submenu { text, entries } => {
                    out.push_str(&format!("{pad}{text} >\n"));
                    walk(entries, depth + 1, out);
                }
                Entry::Bars(_) => {}
            }
        }
    }
    let mut out = String::new();
    walk(entries, 0, &mut out);
    out
}

/// The text `muda` should be given so that `text` appears as written: `&` would start a mnemonic, and
/// `muda` also uses the sequence `[~~]` internally as its stand-in for an escaped `&`.
pub fn escape_title(text: &str) -> String {
    text.replace('&', "&&").replace("[~~]", "[~\u{200B}~]")
}

pub struct Rendered {
    pub menu: Menu,
    pub actions: HashMap<MenuId, Action>,
}

pub fn render(entries: &[Entry]) -> muda::Result<Rendered> {
    let menu = Menu::new();
    let mut actions = HashMap::new();
    let mut counter = 0usize;
    for item in items(entries, &mut actions, &mut counter)? {
        menu.append(item.as_ref())?;
    }
    // Each `Entry` became exactly one native item, so the bar rows can be found by position.
    crate::level_bar::attach(menu.ns_menu(), entries);
    Ok(Rendered { menu, actions })
}

fn items(entries: &[Entry], actions: &mut HashMap<MenuId, Action>, counter: &mut usize) -> muda::Result<Vec<Box<dyn IsMenuItem>>> {
    let mut out: Vec<Box<dyn IsMenuItem>> = Vec::new();
    let next_id = |counter: &mut usize| {
        *counter += 1;
        MenuId::new(format!("as-{counter}"))
    };
    for e in entries {
        match e {
            Entry::Separator => out.push(Box::new(PredefinedMenuItem::separator())),
            Entry::Label(t) => out.push(Box::new(MenuItem::with_id(next_id(counter), escape_title(t), false, None))),
            Entry::Item { text, action, checked, enabled } => {
                let id = next_id(counter);
                if let Some(a) = action {
                    actions.insert(id.clone(), a.clone());
                }
                let text = escape_title(text);
                match checked {
                    Some(c) => out.push(Box::new(CheckMenuItem::with_id(id, text, *enabled, *c, None))),
                    None => out.push(Box::new(MenuItem::with_id(id, text, *enabled, None))),
                }
            }
            Entry::Submenu { text, entries } => {
                let sub = Submenu::with_id(next_id(counter), escape_title(text), true);
                for child in items(entries, actions, counter)? {
                    sub.append(child.as_ref())?;
                }
                out.push(Box::new(sub));
            }
            // A text stand-in that `level_bar::attach` turns into bars; it stays as text if that fails.
            Entry::Bars(bars) => {
                let text = bars.iter().map(|b| format!("{} {}%", b.window, b.used)).collect::<Vec<_>>().join(" · ");
                out.push(Box::new(MenuItem::with_id(next_id(counter), text, false, None)));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_switch_core::state::{Pending, Windows, WindowSnap};

    const NOW: u64 = 1_800_000_000;

    fn account(id: &str, provider: Provider, label: &str) -> Account {
        Account { id: id.into(), provider, label: label.into(), email: None, uuid: None }
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.add_account(account("personal", Provider::Claude, "Personal")).unwrap();
        c.add_account(account("team", Provider::Claude, "Team")).unwrap();
        c.add_account(account("oa", Provider::Openai, "OpenAI Personal")).unwrap();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        c.bind(SurfaceKind::ClaudeDesktop, Binding::parse("team")).unwrap();
        c
    }

    fn reading(five: f64, week: f64) -> UsageRecord {
        UsageRecord {
            windows: Windows {
                five_hour: Some(WindowSnap { pct: five, resets_at: Some(NOW as i64 + 3600) }),
                weekly: Some(WindowSnap { pct: week, resets_at: Some(NOW as i64 + 86_400) }),
            },
            fetched_at: Some(NOW),
            ..UsageRecord::default()
        }
    }

    fn ctx() -> Ctx {
        Ctx { now: NOW, login: LoginStatus::Disabled, watcher_running: true, notice: None, force_offer: None }
    }

    fn find_submenu<'a>(entries: &'a [Entry], prefix: &str) -> &'a Vec<Entry> {
        entries
            .iter()
            .find_map(|e| match e {
                Entry::Submenu { text, entries } if text.starts_with(prefix) => Some(entries),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no submenu `{prefix}` in\n{}", to_text(entries)))
    }

    #[test]
    fn busy_refusal_offers_switch_anyway() {
        let c = config();
        let offer = ForceOffer { kind: SurfaceKind::ClaudeDesktop, account: None, titles: "Fix login, Refactor".into(), since: NOW };
        let menu = build(&c, &State::default(), &Ctx { force_offer: Some(offer.clone()), notice: Some("refused".into()), ..ctx() });
        assert_eq!(menu[0], Entry::item(
                "Switch anyway (ends: Fix login, Refactor)",
                Action::ForceRelaunch { kind: SurfaceKind::ClaudeDesktop, account: None, titles: "Fix login, Refactor".into() }
            ), "top item");
        assert_eq!(menu[1], Entry::Separator);
        assert_eq!(menu[2], Entry::Label("Last: refused".into()));
        // The offer lapses after ten minutes and is absent when there was no refusal.
        let late = build(&c, &State::default(), &Ctx { now: NOW + FORCE_OFFER_SECS, force_offer: Some(offer), ..ctx() });
        assert!(!to_text(&late).contains("Switch anyway"));
        assert!(!to_text(&build(&c, &State::default(), &ctx())).contains("Switch anyway"));
    }

    #[test]
    fn a_quit_veto_refusal_produces_the_switch_anyway_offer() {
        // The text `ClaudeDesktop::apply` returns when Claude's own dialog vetoed the quit.
        let text = agent_switch_core::error::Error::Invalid(format!(
            "{}a session Claude reports as working. Nothing was quit. Claude is asking whether to interrupt its work.",
            agent_switch_core::adapters::claude_desktop::session_mirror::BUSY_PREFIX
        ))
        .to_string();
        let titles = agent_switch_core::adapters::claude_desktop::session_mirror::busy_titles_in(&text).expect("the veto refusal parses as busy");
        let offer = ForceOffer { kind: SurfaceKind::ClaudeDesktop, account: None, titles, since: NOW };
        let menu = build(&config(), &State::default(), &Ctx { force_offer: Some(offer), ..ctx() });
        assert_eq!(
            menu[0],
            Entry::item(
                "Switch anyway (ends: a session Claude reports as working)",
                Action::ForceRelaunch {
                    kind: SurfaceKind::ClaudeDesktop,
                    account: None,
                    titles: "a session Claude reports as working".into()
                }
            )
        );
    }

    #[test]
    fn each_surface_row_shows_its_account_with_usage_and_a_check_on_the_active_one() {
        let c = config();
        let mut s = State::default();
        s.usage.insert("personal".into(), reading(32.0, 40.0));
        s.usage.insert("team".into(), reading(9.0, 6.0));
        s.set_active(SurfaceKind::ClaudeCli, Some("team".into())); // auto mode moved it; the binding stays
        let menu = build(&c, &s, &ctx());
        let text = to_text(&menu);
        assert!(text.contains("Claude CLI: Team · 5h 9% · 7d 6% >"), "{text}");
        assert!(text.contains("Claude Desktop: Team · 5h 9% · 7d 6% >"), "{text}");
        assert!(text.contains("Codex CLI: unmanaged >"), "{text}");

        let cli = find_submenu(&menu, "Claude CLI");
        assert_eq!(
            cli[0],
            Entry::choice("Personal · 5h 32% · 7d 40%", false, Action::Bind(SurfaceKind::ClaudeCli, Binding::parse("personal")))
        );
        // Each account with a reading is followed by its bars.
        let plenty = |window, used| Bar { window, used, tier: Tier::Plenty };
        assert_eq!(cli[1], Entry::Bars(vec![plenty("5h", 32), plenty("7d", 40)]));
        // The checked one is the active account and clicking it changes nothing.
        assert_eq!(cli[2], Entry::Item { text: "Team · 5h 9% · 7d 6%".into(), action: None, checked: Some(true), enabled: true });
        assert_eq!(cli[3], Entry::Bars(vec![plenty("5h", 9), plenty("7d", 6)]));
        assert_eq!(cli.last().unwrap(), &Entry::choice("Unmanaged (do not touch)", false, Action::Bind(SurfaceKind::ClaudeCli, Binding::Unmanaged)));
    }

    #[test]
    fn a_bar_turns_yellow_then_red_as_the_window_runs_out_following_the_auto_threshold() {
        // Default threshold 90: red at 10% left or less, yellow up to 30% left.
        assert_eq!(tier(100, 90), Tier::Plenty);
        assert_eq!(tier(31, 90), Tier::Plenty);
        assert_eq!(tier(30, 90), Tier::Low);
        assert_eq!(tier(11, 90), Tier::Low);
        assert_eq!(tier(10, 90), Tier::Critical);
        assert_eq!(tier(0, 90), Tier::Critical);
        // A lower threshold moves both limits up; the extremes never overflow.
        assert_eq!(tier(40, 60), Tier::Critical);
        assert_eq!(tier(41, 60), Tier::Low);
        assert_eq!(tier(61, 60), Tier::Plenty);
        assert_eq!(tier(0, 100), Tier::Critical);
        assert_eq!(tier(1, 100), Tier::Low);
        assert_eq!(tier(99, 1), Tier::Critical);
        assert_eq!(tier(100, 1), Tier::Low);
    }

    #[test]
    fn bars_come_only_from_a_clean_reading_and_follow_its_windows() {
        let r = reading(72.0, 95.4);
        assert_eq!(
            usage_bars(Some(&r), NOW, 90),
            vec![Bar { window: "5h", used: 72, tier: Tier::Low }, Bar { window: "7d", used: 95, tier: Tier::Critical }]
        );
        assert_eq!(usage_bars(Some(&r), NOW, 90)[1].remaining(), 5);
        // A reset window is empty again.
        assert_eq!(usage_bars(Some(&r), NOW + 4000, 90)[0], Bar { window: "5h", used: 0, tier: Tier::Plenty });
        // The label rounds like the text row.
        assert_eq!(usage_bars(Some(&reading(32.5, 0.0)), NOW, 90)[0].used, 32);
        assert_eq!(usage_text(Some(&reading(32.5, 0.0)), NOW), "5h 32% · 7d 0%");
        // Only the windows that were read.
        let mut one = reading(10.0, 20.0);
        one.windows.five_hour = None;
        assert_eq!(usage_bars(Some(&one), NOW, 90), vec![Bar { window: "7d", used: 20, tier: Tier::Plenty }]);
        // No reading, or one that is not clean: no bars.
        assert!(usage_bars(None, NOW, 90).is_empty());
        for bad in [UsageState::Expired, UsageState::Dead, UsageState::Unreadable] {
            assert!(usage_bars(Some(&UsageRecord { state: bad, ..reading(10.0, 20.0) }), NOW, 90).is_empty(), "{bad:?}");
        }
        assert!(usage_bars(Some(&UsageRecord::default()), NOW, 90).is_empty());
    }

    #[test]
    fn the_surface_row_is_followed_by_its_accounts_bars_which_stay_out_of_the_text_and_trigger_a_rebuild() {
        let mut c = config();
        c.auto.threshold = 80;
        let mut s = State::default();
        s.usage.insert("personal".into(), reading(85.0, 10.0));
        s.usage.insert("team".into(), UsageRecord { state: UsageState::Dead, ..reading(9.0, 6.0) });
        let menu = build(&c, &s, &ctx());
        let at = |prefix: &str| menu.iter().position(|e| matches!(e, Entry::Submenu { text, .. } if text.starts_with(prefix))).unwrap();
        // Claude CLI is on Personal: its bars follow, colored by the configured threshold.
        assert_eq!(
            menu[at("Claude CLI") + 1],
            Entry::Bars(vec![Bar { window: "5h", used: 85, tier: Tier::Critical }, Bar { window: "7d", used: 10, tier: Tier::Plenty }])
        );
        // Claude Desktop is on Team, whose reading is dead; Codex CLI is unmanaged.
        assert!(!matches!(menu[at("Claude Desktop") + 1], Entry::Bars(_)));
        assert!(!matches!(menu[at("Codex CLI") + 1], Entry::Bars(_)));
        let desktop = find_submenu(&menu, "Claude Desktop");
        assert!(matches!(desktop[1], Entry::Bars(_)) && !matches!(desktop[3], Entry::Bars(_)), "{desktop:?}");

        assert!(!to_text(&menu).contains("Bars"), "the bars add no text");
        assert_eq!(to_text(&menu).lines().count(), to_text(&menu.iter().filter(|e| !matches!(e, Entry::Bars(_))).cloned().collect::<Vec<_>>()).lines().count());

        s.usage.insert("personal".into(), reading(86.0, 10.0));
        assert!(should_rebuild(&build(&c, &s, &ctx()), &menu, false), "a new reading redraws the bars");
    }

    #[test]
    fn a_desktop_account_row_says_it_relaunches_and_dispatches_the_combined_action() {
        let c = config(); // Claude Desktop is bound to Team
        let menu = build(&c, &State::default(), &ctx());
        let desktop = find_submenu(&menu, "Claude Desktop");
        assert_eq!(
            desktop[0],
            Entry::choice("Personal · no reading yet (relaunches app)", false, Action::SwitchApp(SurfaceKind::ClaudeDesktop, "personal".into()))
        );
        // The active one has no suffix and no action.
        assert_eq!(desktop[1], Entry::Item { text: "Team · no reading yet".into(), action: None, checked: Some(true), enabled: true });
        // Unmanaged only unbinds, and CLI rows are unchanged.
        assert_eq!(desktop.last().unwrap(), &Entry::choice("Unmanaged (do not touch)", false, Action::Bind(SurfaceKind::ClaudeDesktop, Binding::Unmanaged)));
        let cli = find_submenu(&menu, "Claude CLI");
        assert!(matches!(&cli[1], Entry::Item { text, action: Some(Action::Bind(SurfaceKind::ClaudeCli, _)), .. } if !text.contains("relaunches")), "{cli:?}");
    }

    #[test]
    fn the_latest_notice_is_a_disabled_line_at_the_top_and_gone_when_cleared() {
        let c = config();
        let with = build(&c, &State::default(), &Ctx { notice: Some("Claude Desktop now uses Team\nsecond line".into()), ..ctx() });
        assert_eq!(with[0], Entry::Label("Last: Claude Desktop now uses Team second line".into()));
        assert!(!to_text(&build(&c, &State::default(), &ctx())).contains("Last:"));
    }

    #[test]
    fn a_click_forces_a_rebuild_even_when_the_model_is_unchanged() {
        let c = config();
        let a = build(&c, &State::default(), &ctx());
        assert!(!should_rebuild(&a, &a, false), "a periodic refresh skips an unchanged menu");
        assert!(should_rebuild(&a, &a, true), "a click rebuilds so the native check state matches");
        assert!(should_rebuild(&a, &[], false));
    }

    #[test]
    fn an_unmanaged_surface_checks_unmanaged_and_offers_only_accounts_of_its_provider() {
        let c = config();
        let menu = build(&c, &State::default(), &ctx());
        let codex = find_submenu(&menu, "Codex CLI");
        assert_eq!(codex.len(), 3, "{codex:?}");
        assert!(matches!(&codex[0], Entry::Item { text, .. } if text.starts_with("OpenAI Personal")));
        assert!(matches!(codex.last().unwrap(), Entry::Item { checked: Some(true), action: None, .. }));
    }

    #[test]
    fn usage_text_marks_a_reading_that_is_not_clean_and_rolls_over_a_reset_window() {
        let mut r = reading(32.0, 40.0);
        assert_eq!(usage_text(Some(&r), NOW), "5h 32% · 7d 40%");
        assert_eq!(usage_text(Some(&r), NOW + 4000), "5h 0% · 7d 40%", "the 5-hour window has reset");
        r.state = UsageState::Dead;
        assert_eq!(usage_text(Some(&r), NOW), "5h 32% · 7d 40% (sign in again)");
        r.state = UsageState::Unreadable;
        r.windows = Windows::default();
        assert_eq!(usage_text(Some(&r), NOW), "no reading yet (unreadable)");
        assert_eq!(usage_text(None, NOW), "no reading yet");
    }

    #[test]
    fn the_footer_has_auto_switch_the_codex_mode_undo_login_item_and_quit() {
        let mut c = config();
        c.auto.enabled = true;
        c.auto.codex_cli_mode = CodexCliMode::Notify;
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "personal", 1);
        s.record_switch(SurfaceKind::ClaudeCli, "team", 2);
        let menu = build(&c, &s, &Ctx { login: LoginStatus::Enabled, ..ctx() });
        let text = to_text(&menu);
        assert!(text.contains("[x] Auto-switch"), "{text}");
        assert!(text.contains("[x] Notify only"), "{text}");
        assert!(text.contains("[ ] New sessions only") && text.contains("[ ] Force"), "{text}");
        assert!(text.contains("Undo last switch: Claude CLI back to Personal"), "{text}");
        assert!(text.contains("[x] Start at login"), "{text}");
        assert!(text.contains("Quit agent-switch"), "{text}");

        let actions: Vec<Action> = flatten(&menu).into_iter().filter_map(|e| if let Entry::Item { action, .. } = e { action.clone() } else { None }).collect();
        assert!(actions.contains(&Action::SetAuto(false)));
        assert!(actions.contains(&Action::SetCodexMode(CodexCliMode::Force)));
        assert!(actions.contains(&Action::Undo(SurfaceKind::ClaudeCli)));
        assert!(actions.contains(&Action::SetLoginItem(false)));
        assert!(actions.contains(&Action::Quit));
        assert!(!actions.contains(&Action::SetCodexMode(CodexCliMode::Notify)), "the current mode is not clickable");
    }

    fn flatten(entries: &[Entry]) -> Vec<&Entry> {
        let mut out = Vec::new();
        for e in entries {
            out.push(e);
            if let Entry::Submenu { entries, .. } = e {
                out.extend(flatten(entries));
            }
        }
        out
    }

    #[test]
    fn relaunch_and_open_rows_need_a_bound_app() {
        let c = config();
        let text = to_text(&build(&c, &State::default(), &ctx()));
        assert!(text.contains("Relaunch Claude\n") && text.contains("Open Claude (bound profile)\n"), "{text}");
        assert!(text.contains("Relaunch ChatGPT (bind ChatGPT first) (disabled)"), "{text}");
    }

    #[test]
    fn a_pending_event_shows_as_a_row_until_it_expires() {
        let c = config();
        let mut s = State::default();
        s.set_pending(
            SurfaceKind::ClaudeDesktop,
            Some(Pending {
                id: "e1".into(),
                target_account: "personal".into(),
                reason: REASON_SWITCH_NEEDED.into(),
                from: Some("team".into()),
                created_at: NOW,
                expires_at: Some(NOW + 900),
            }),
        );
        let menu = build(&c, &s, &ctx());
        let row = flatten(&menu).into_iter().find(|e| matches!(e, Entry::Item { text, .. } if text.starts_with("Relaunch Claude Desktop as"))).expect("row");
        assert!(matches!(row, Entry::Item { text, action: Some(Action::Relaunch { kind: SurfaceKind::ClaudeDesktop, event_id }), .. } if text == "Relaunch Claude Desktop as Personal…" && event_id == "e1"));
        let later = build(&c, &s, &Ctx { now: NOW + 900, ..ctx() });
        assert!(!to_text(&later).contains("Relaunch Claude Desktop as"));
    }

    #[test]
    fn a_label_with_quotes_and_applescript_is_shown_literally() {
        let evil = r#"Team" & do shell script "touch /tmp/pwned" & "x"#;
        let mut c = Config::default();
        c.add_account(account("team", Provider::Claude, evil)).unwrap();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("team")).unwrap();
        let menu = build(&c, &State::default(), &ctx());
        let text = to_text(&menu);
        let shown: String = evil.chars().take(MAX_LABEL_CHARS - 1).collect();
        assert!(text.contains(&shown), "{text}");
        // What muda gets: `&` doubled, so it renders as `&` and starts no mnemonic.
        assert_eq!(escape_title(r#"a" & b"#), r#"a" && b"#);
        assert_eq!(escape_title("a [~~] b"), "a [~\u{200B}~] b");
        // No action carries a label.
        for e in flatten(&menu) {
            if let Entry::Item { action: Some(a), .. } = e {
                assert!(!format!("{a:?}").contains("shell script"), "{a:?}");
            }
        }
    }

    #[test]
    fn long_and_multi_line_labels_stay_on_one_short_line() {
        let a = account("x", Provider::Claude, "Line one\nLine two\u{7}");
        assert_eq!(account_label(&a), "Line one Line two");
        let long = account("y", Provider::Claude, &"w".repeat(200));
        assert_eq!(account_label(&long).chars().count(), MAX_LABEL_CHARS);
        assert_eq!(account_label(&account("z", Provider::Claude, "  ")), "z", "an empty label falls back to the id");
    }

    #[test]
    fn a_stopped_watcher_and_an_unreadable_state_are_said_plainly() {
        let c = config();
        let text = to_text(&build(&c, &State::default(), &Ctx { watcher_running: false, ..ctx() }));
        assert!(text.starts_with("The watcher stopped"), "{text}");
        let err = to_text(&error_menu("state.json: expected value", &ctx()));
        assert!(err.contains("cannot read its files") && err.contains("expected value") && err.contains("Quit"), "{err}");
    }

    #[test]
    fn login_item_states_map_to_clicks() {
        for (status, checked, action) in [
            (LoginStatus::Enabled, Some(true), Some(Action::SetLoginItem(false))),
            (LoginStatus::Disabled, Some(false), Some(Action::SetLoginItem(true))),
        ] {
            assert!(matches!(login_entry(&status), Entry::Item { checked: c, action: a, .. } if c == checked && a == action));
        }
        assert!(matches!(login_entry(&LoginStatus::Unavailable("x".into())), Entry::Item { enabled: false, .. }));
    }
}
