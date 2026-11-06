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
    /// Turn auto-switch on or off for one surface.
    SetSurfaceAuto(SurfaceKind, bool),
    /// Move `account` one place up or down in the auto-switch priority of `kind`.
    MovePriority { kind: SurfaceKind, account: String, up: bool },
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
    /// One saved account, as a plain row that shows which surfaces use it and its usage.
    Account(AccountRow),
}

/// An account row: `[CLI][APP] name   5h ▬── 10%   wk ▬── 16%`. The native row draws the icons and the bars;
/// `row_text` is its plain form, for `to_text` and as the title when the drawing cannot be done. The row has
/// no submenu and no action; the account is chosen in the `CLI` and `Desktop` submenus.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    /// The provider's CLI (Claude CLI, Codex CLI) currently uses this account.
    pub cli: bool,
    /// The provider's desktop app (Claude Desktop, ChatGPT) currently uses this account.
    pub app: bool,
    /// The saved email, or the label, or the id.
    pub name: String,
    pub usage: Usage,
}

/// What an account row shows about its usage.
#[derive(Debug, Clone, PartialEq)]
pub enum Usage {
    /// One bar per window that was read.
    Bars(Vec<Bar>),
    /// Why there are no bars: `no reading yet`, `login expired`, `sign in again` or `unreadable`.
    Note(&'static str),
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

/// One usage window as a level bar. The fill and the number show `used`; the color shows what is left.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// `5h` or `wk` (the weekly window).
    pub window: &'static str,
    /// The used percent, 0-100.
    pub used: u8,
    pub tier: Tier,
}

impl Bar {
    /// The percent left, which sets the color.
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
/// (expired, dead, unreadable); `usage` then gives the note instead.
pub fn usage_bars(rec: Option<&UsageRecord>, now: u64, threshold: u8) -> Vec<Bar> {
    let Some(r) = rec.filter(|r| r.state == UsageState::Ok) else { return Vec::new() };
    [("5h", &r.windows.five_hour), ("wk", &r.windows.weekly)]
        .into_iter()
        .filter_map(|(window, snap)| {
            // Ties round to even, as `{:.0}` would.
            let used = snap.as_ref()?.pct_at(now).round_ties_even().clamp(0.0, 100.0) as u8;
            Some(Bar { window, used, tier: tier(100 - used, threshold) })
        })
        .collect()
}

/// The usage of an account row: its bars, or the note that says why there are none.
pub fn usage(rec: Option<&UsageRecord>, now: u64, threshold: u8) -> Usage {
    let bars = usage_bars(rec, now, threshold);
    if !bars.is_empty() {
        return Usage::Bars(bars);
    }
    Usage::Note(match rec.map(|r| r.state) {
        None | Some(UsageState::Ok) => "no reading yet",
        Some(UsageState::Expired) => "login expired",
        Some(UsageState::Dead) => "sign in again",
        Some(UsageState::Unreadable) => "unreadable",
    })
}

/// `5h 7% · wk 16%`, or the note.
pub fn usage_text(usage: &Usage) -> String {
    match usage {
        Usage::Bars(bars) => bars.iter().map(|b| format!("{} {}%", b.window, b.used)).collect::<Vec<_>>().join(" · "),
        Usage::Note(note) => (*note).to_string(),
    }
}

/// The plain form of an account row: `[CLI][APP] name · 5h 7% · wk 16%`, with blanks for a surface that
/// uses another account, so the names line up.
pub fn row_text(row: &AccountRow) -> String {
    let cli = if row.cli { "[CLI]" } else { "     " };
    let app = if row.app { "[APP]" } else { "     " };
    format!("{cli}{app} {} · {}", row.name, usage_text(&row.usage))
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

fn provider_title(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Openai => "ChatGPT and Codex",
    }
}

/// The name of an account row: the saved email, else the label, else the id.
pub fn account_name(account: &Account) -> String {
    match account.email.as_deref().map(str::trim) {
        Some(email) if !email.is_empty() => clean(email),
        _ => account_label(account),
    }
}

/// The CLI surface and the desktop surface of `provider`.
fn surfaces(provider: Provider) -> (SurfaceKind, SurfaceKind) {
    match provider {
        Provider::Claude => (SurfaceKind::ClaudeCli, SurfaceKind::ClaudeDesktop),
        Provider::Openai => (SurfaceKind::CodexCli, SurfaceKind::ChatgptDesktop),
    }
}

/// The row of `account`. `cli_current` and `app_current` are the current accounts of the provider's surfaces.
fn account_entry(state: &State, account: &Account, cli_current: Option<&str>, app_current: Option<&str>, threshold: u8, now: u64) -> Entry {
    let id = account.id.as_str();
    Entry::Account(AccountRow {
        cli: cli_current == Some(id),
        app: app_current == Some(id),
        name: account_name(account),
        usage: usage(state.usage.get(id), now, threshold),
    })
}

/// The name of account `id` as the rows show it, or the id itself when the account is gone.
fn name_of(config: &Config, id: &str) -> String {
    config.account(id).map_or_else(|| clean(id), account_name)
}

/// The submenu of one surface: `CLI: name` or `Desktop: name`. It picks the account (or none), turns
/// auto-switch on or off and orders the accounts for auto-switch.
fn surface_entry(config: &Config, kind: SurfaceKind, current: Option<&str>) -> Entry {
    let mut entries = Vec::new();
    for account in config.accounts.iter().filter(|a| a.provider == kind.provider()) {
        let id = account.id.as_str();
        let on = current == Some(id);
        let name = account_name(account);
        entries.push(if kind.is_desktop() {
            // Clicking it quits and reopens the app, so the row says so unless the app already uses the account.
            Entry::choice(format!("{name}{}", if on { "" } else { RELAUNCH_SUFFIX }), on, Action::SwitchApp(kind, id.to_string()))
        } else {
            Entry::choice(name, on, Action::Bind(kind, Binding::Account(id.to_string())))
        });
    }
    entries.push(Entry::choice("Leave unmanaged", current.is_none(), Action::Bind(kind, Binding::Unmanaged)));
    entries.push(Entry::Separator);
    match kind {
        SurfaceKind::ClaudeDesktop => {
            entries.push(app_row(config, kind, "Relaunch Claude", Action::RelaunchApp(kind)));
            entries.push(app_row(config, kind, "Open Claude (bound profile)", Action::OpenClaude));
            entries.push(Entry::Separator);
        }
        SurfaceKind::ChatgptDesktop => {
            entries.push(app_row(config, kind, "Relaunch ChatGPT", Action::RelaunchApp(kind)));
            entries.push(Entry::Separator);
        }
        SurfaceKind::ClaudeCli | SurfaceKind::CodexCli => {}
    }
    let auto = config.auto.enabled_for(kind);
    entries.push(toggle("Auto-switch", auto, Action::SetSurfaceAuto(kind, !auto)));
    entries.push(Entry::Submenu { text: "Priority".into(), entries: priority_entries(config, kind) });
    let prefix = if kind.is_desktop() { "Desktop" } else { "CLI" };
    let name = current.map_or_else(|| "unmanaged".to_string(), |id| name_of(config, id));
    Entry::Submenu { text: format!("{prefix}: {name}"), entries }
}

/// `1. name`, `2. name`, ... each with "Move up" and "Move down"; a move past either end is disabled.
fn priority_entries(config: &Config, kind: SurfaceKind) -> Vec<Entry> {
    let order = config.priority(kind);
    if order.is_empty() {
        return vec![Entry::disabled("No saved accounts")];
    }
    let step = |text: &str, possible: bool, id: &str, up: bool| {
        if possible {
            Entry::item(text, Action::MovePriority { kind, account: id.to_string(), up })
        } else {
            Entry::disabled(text)
        }
    };
    order
        .iter()
        .enumerate()
        .map(|(i, id)| Entry::Submenu {
            text: format!("{}. {}", i + 1, name_of(config, id)),
            entries: vec![step("Move up", i > 0, id, true), step("Move down", i + 1 < order.len(), id, false)],
        })
        .collect()
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
    let settings = vec![
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
    ];
    vec![Entry::Submenu { text: "Settings".into(), entries: settings }, Entry::item("Quit agent-switch", Action::Quit)]
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
        let (cli, app) = surfaces(provider);
        let cli_current = policy::current_account(config, state, cli);
        let app_current = policy::current_account(config, state, app);
        let before = out.len();
        for account in config.accounts.iter().filter(|a| a.provider == provider) {
            out.push(account_entry(state, account, cli_current.as_deref(), app_current.as_deref(), config.auto.threshold, ctx.now));
        }
        if out.len() == before {
            out.push(Entry::disabled("No saved accounts"));
        }
        out.push(surface_entry(config, cli, cli_current.as_deref()));
        out.push(surface_entry(config, app, app_current.as_deref()));
        // Open questions stay at the top level: they ask for an answer.
        for kind in [cli, app] {
            out.extend(pending_entry(config, state, kind, ctx.now));
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
                Entry::Account(row) => out.push_str(&format!("{pad}{}\n", row_text(row))),
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
    // Each `Entry` became exactly one native item, so the account rows can be found by position.
    crate::account_row::attach(menu.ns_menu(), entries);
    Ok(Rendered { menu, actions })
}

fn items(entries: &[Entry], actions: &mut HashMap<MenuId, Action>, counter: &mut usize) -> muda::Result<Vec<Box<dyn IsMenuItem>>> {
    let mut out: Vec<Box<dyn IsMenuItem>> = Vec::new();
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
            Entry::Submenu { text, entries } => out.push(submenu(text, entries, actions, counter)?),
            // An enabled item without an action: `account_row::attach` draws the icons and bars over this plain
            // title (it stays if that fails), and AppKit still highlights the row.
            Entry::Account(row) => out.push(Box::new(MenuItem::with_id(next_id(counter), escape_title(&row_text(row)), true, None))),
        }
    }
    Ok(out)
}

fn next_id(counter: &mut usize) -> MenuId {
    *counter += 1;
    MenuId::new(format!("as-{counter}"))
}

fn submenu(text: &str, entries: &[Entry], actions: &mut HashMap<MenuId, Action>, counter: &mut usize) -> muda::Result<Box<dyn IsMenuItem>> {
    let sub = Submenu::with_id(next_id(counter), escape_title(text), true);
    for child in items(entries, actions, counter)? {
        sub.append(child.as_ref())?;
    }
    Ok(Box::new(sub))
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

    fn find_row<'a>(entries: &'a [Entry], name: &str) -> &'a AccountRow {
        entries
            .iter()
            .find_map(|e| match e {
                Entry::Account(row) if row.name == name => Some(row),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no account row `{name}` in\n{}", to_text(entries)))
    }

    fn bars(five: (u8, Tier), week: (u8, Tier)) -> Usage {
        Usage::Bars(vec![Bar { window: "5h", used: five.0, tier: five.1 }, Bar { window: "wk", used: week.0, tier: week.1 }])
    }

    #[test]
    fn each_provider_lists_every_account_once_with_icons_for_the_surfaces_that_use_it() {
        let c = config(); // Claude CLI is bound to Personal, Claude Desktop to Team
        let mut s = State::default();
        s.usage.insert("personal".into(), reading(32.0, 40.0));
        s.usage.insert("team".into(), reading(9.0, 6.0));
        s.set_active(SurfaceKind::ClaudeCli, Some("team".into())); // auto mode moved it; the binding stays
        let menu = build(&c, &s, &ctx());
        let personal = find_row(&menu, "Personal");
        assert_eq!((personal.cli, personal.app), (false, false));
        assert_eq!(personal.usage, bars((32, Tier::Plenty), (40, Tier::Plenty)));
        let team = find_row(&menu, "Team");
        assert_eq!((team.cli, team.app), (true, true), "the active account, not the binding, gets the icon");
        // Nothing uses the OpenAI account; it is listed under its own provider.
        let oa = find_row(&menu, "OpenAI Personal");
        assert_eq!((oa.cli, oa.app, &oa.usage), (false, false, &Usage::Note("no reading yet")));

        let text = to_text(&menu);
        assert!(text.starts_with("Claude\n           Personal · 5h 32% · wk 40%\n[CLI][APP] Team · 5h 9% · wk 6%\nCLI: Team >\n"), "{text}");
        assert!(text.contains("ChatGPT and Codex\n           OpenAI Personal · no reading yet\nCLI: unmanaged >\n"), "{text}");
        assert_eq!(text.matches("Personal ·").count(), 2, "each account once, under its provider: {text}");
        assert!(!text.contains("Use for"), "account rows have no submenu: {text}");
    }

    #[test]
    fn each_provider_has_a_cli_and_a_desktop_submenu_with_account_auto_switch_and_priority() {
        let mut c = config(); // Claude CLI is bound to Personal, Claude Desktop to Team
        c.auto.set_enabled_for(SurfaceKind::ClaudeCli, true);
        let text = to_text(&build(&c, &State::default(), &ctx()));
        let claude = "\
Claude
[CLI]      Personal · no reading yet
     [APP] Team · no reading yet
CLI: Personal >
  [x] Personal (current)
  [ ] Team
  [ ] Leave unmanaged
  --------
  [x] Auto-switch
  Priority >
    1. Personal >
      Move up (disabled)
      Move down
    2. Team >
      Move up
      Move down (disabled)
Desktop: Team >
  [ ] Personal (relaunches app)
  [x] Team (current)
  [ ] Leave unmanaged
  --------
  Relaunch Claude
  Open Claude (bound profile)
  --------
  [ ] Auto-switch
  Priority >
    1. Team >
      Move up (disabled)
      Move down
    2. Personal >
      Move up
      Move down (disabled)
--------
ChatGPT and Codex
";
        assert!(text.starts_with(claude), "{text}");
        assert!(!text.contains("Options"), "{text}");
    }

    #[test]
    fn the_surface_submenus_carry_the_actions_of_each_choice() {
        let mut c = config(); // Claude CLI is bound to Personal, Claude Desktop to Team
        c.auto.enabled = true;
        c.priority.claude_desktop = vec!["team".into()];
        let menu = build(&c, &State::default(), &ctx());
        let bind = |kind, id: &str| Action::Bind(kind, Binding::Account(id.into()));

        let cli = submenu_of(&menu, "CLI: Personal");
        assert_eq!(cli[0], Entry::Item { text: "Personal".into(), action: None, checked: Some(true), enabled: true }, "the current account does nothing");
        assert_eq!(cli[1], Entry::choice("Team", false, bind(SurfaceKind::ClaudeCli, "team")));
        assert_eq!(cli[2], Entry::choice("Leave unmanaged", false, Action::Bind(SurfaceKind::ClaudeCli, Binding::Unmanaged)));
        assert_eq!(cli[4], toggle("Auto-switch", true, Action::SetSurfaceAuto(SurfaceKind::ClaudeCli, false)), "old `enabled = true` means on");

        let desktop = submenu_of(&menu, "Desktop: Team");
        assert_eq!(desktop[0], Entry::choice("Personal (relaunches app)", false, Action::SwitchApp(SurfaceKind::ClaudeDesktop, "personal".into())));
        assert_eq!(desktop[1], Entry::Item { text: "Team".into(), action: None, checked: Some(true), enabled: true });
        // The desktop's bound account (Team) leads its order; the CLI's (Personal) leads the CLI's.
        let order = |entries: &[Entry]| -> Vec<String> {
            let Some(Entry::Submenu { entries, .. }) = entries.iter().find(|e| matches!(e, Entry::Submenu { text, .. } if text == "Priority")) else { panic!("no Priority") };
            entries.iter().map(|e| if let Entry::Submenu { text, .. } = e { text.clone() } else { panic!("{e:?}") }).collect()
        };
        assert_eq!(order(cli), ["1. Personal", "2. Team"]);
        assert_eq!(order(desktop), ["1. Team", "2. Personal"]);
        let actions: Vec<Action> = flatten(desktop).into_iter().filter_map(|e| if let Entry::Item { action, .. } = e { action.clone() } else { None }).collect();
        assert!(actions.contains(&Action::MovePriority { kind: SurfaceKind::ClaudeDesktop, account: "team".into(), up: false }));
        assert!(actions.contains(&Action::MovePriority { kind: SurfaceKind::ClaudeDesktop, account: "personal".into(), up: true }));
        assert!(!actions.contains(&Action::MovePriority { kind: SurfaceKind::ClaudeDesktop, account: "team".into(), up: true }), "the top cannot move up");
        assert!(!actions.contains(&Action::MovePriority { kind: SurfaceKind::ClaudeDesktop, account: "personal".into(), up: false }), "the bottom cannot move down");

        // Unmanaged OpenAI surfaces: "Leave unmanaged" is the checked row, and auto-switch has its own toggle.
        let codex = submenu_of(&menu, "CLI: unmanaged");
        assert_eq!(codex[0], Entry::choice("OpenAI Personal", false, bind(SurfaceKind::CodexCli, "oa")));
        assert_eq!(codex[1], Entry::Item { text: "Leave unmanaged".into(), action: None, checked: Some(true), enabled: true });
        let chatgpt = submenu_of(&menu, "Desktop: unmanaged");
        assert_eq!(chatgpt[0], Entry::choice("OpenAI Personal (relaunches app)", false, Action::SwitchApp(SurfaceKind::ChatgptDesktop, "oa".into())));
        assert!(chatgpt.contains(&Entry::disabled("Relaunch ChatGPT (bind ChatGPT first)")));
        assert!(chatgpt.contains(&toggle("Auto-switch", true, Action::SetSurfaceAuto(SurfaceKind::ChatgptDesktop, false))));
    }

    #[test]
    fn a_provider_without_accounts_says_so() {
        let mut c = Config::default();
        c.add_account(account("p", Provider::Claude, "P")).unwrap();
        let text = to_text(&build(&c, &State::default(), &ctx()));
        assert!(text.contains("ChatGPT and Codex\nNo saved accounts (disabled)\n"), "{text}");
    }

    #[test]
    fn a_row_is_named_by_its_email_then_its_label_then_its_id() {
        let mut a = account("id1", Provider::Claude, "Work");
        assert_eq!(account_name(&a), "Work");
        a.email = Some("  me@example.com \n".into());
        assert_eq!(account_name(&a), "me@example.com");
        a.email = Some("   ".into());
        assert_eq!(account_name(&a), "Work", "a blank email falls back to the label");
        a.label = String::new();
        assert_eq!(account_name(&a), "id1");
        a.email = Some(format!("{}@example.com", "x".repeat(100)));
        assert_eq!(account_name(&a).chars().count(), MAX_LABEL_CHARS, "a long email is cut like a label");
    }

    #[test]
    fn a_row_without_bars_shows_a_short_note_and_a_new_reading_triggers_a_rebuild() {
        let mut r = reading(32.0, 40.0);
        assert_eq!(usage(Some(&r), NOW, 90), bars((32, Tier::Plenty), (40, Tier::Plenty)));
        assert_eq!(usage_text(&usage(Some(&r), NOW, 90)), "5h 32% · wk 40%");
        for (state, note) in [(UsageState::Expired, "login expired"), (UsageState::Dead, "sign in again"), (UsageState::Unreadable, "unreadable")] {
            r.state = state;
            assert_eq!(usage(Some(&r), NOW, 90), Usage::Note(note), "{state:?}");
        }
        assert_eq!(usage(None, NOW, 90), Usage::Note("no reading yet"));
        assert_eq!(usage(Some(&UsageRecord::default()), NOW, 90), Usage::Note("no reading yet"), "a clean reading with no windows");

        let c = config();
        let mut s = State::default();
        s.usage.insert("personal".into(), reading(85.0, 10.0));
        let menu = build(&c, &s, &ctx());
        assert!(to_text(&menu).contains("[CLI]      Personal · 5h 85% · wk 10%\n"), "{}", to_text(&menu));
        s.usage.insert("personal".into(), reading(86.0, 10.0));
        assert!(should_rebuild(&build(&c, &s, &ctx()), &menu, false), "a new reading redraws the row");
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
            vec![Bar { window: "5h", used: 72, tier: Tier::Low }, Bar { window: "wk", used: 95, tier: Tier::Critical }]
        );
        assert_eq!(usage_bars(Some(&r), NOW, 90)[1].remaining(), 5);
        // A reset window is empty again.
        assert_eq!(usage_bars(Some(&r), NOW + 4000, 90)[0], Bar { window: "5h", used: 0, tier: Tier::Plenty });
        // Ties round to even.
        assert_eq!(usage_bars(Some(&reading(32.5, 0.0)), NOW, 90)[0].used, 32);
        assert_eq!(usage_bars(Some(&reading(33.5, 0.0)), NOW, 90)[0].used, 34);
        // Only the windows that were read.
        let mut one = reading(10.0, 20.0);
        one.windows.five_hour = None;
        assert_eq!(usage_bars(Some(&one), NOW, 90), vec![Bar { window: "wk", used: 20, tier: Tier::Plenty }]);
        // No reading, or one that is not clean: no bars.
        assert!(usage_bars(None, NOW, 90).is_empty());
        for bad in [UsageState::Expired, UsageState::Dead, UsageState::Unreadable] {
            assert!(usage_bars(Some(&UsageRecord { state: bad, ..reading(10.0, 20.0) }), NOW, 90).is_empty(), "{bad:?}");
        }
        assert!(usage_bars(Some(&UsageRecord::default()), NOW, 90).is_empty());
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
    fn the_footer_has_the_codex_mode_undo_login_item_and_quit() {
        let mut c = config();
        c.auto.enabled = true;
        c.auto.codex_cli_mode = CodexCliMode::Notify;
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "personal", 1);
        s.record_switch(SurfaceKind::ClaudeCli, "team", 2);
        let menu = build(&c, &s, &Ctx { login: LoginStatus::Enabled, ..ctx() });
        let text = to_text(&menu);
        let settings = to_text(submenu_of(&menu, "Settings"));
        assert!(!settings.contains("Auto-switch"), "auto-switch is set per surface: {settings}");
        assert!(text.contains("[x] Notify only"), "{text}");
        assert!(text.contains("[ ] New sessions only") && text.contains("[ ] Force"), "{text}");
        assert!(text.contains("Undo last switch: Claude CLI back to Personal"), "{text}");
        assert!(text.contains("[x] Start at login"), "{text}");
        assert!(text.contains("Quit agent-switch"), "{text}");

        let actions: Vec<Action> = flatten(&menu).into_iter().filter_map(|e| if let Entry::Item { action, .. } = e { action.clone() } else { None }).collect();
        assert!(actions.contains(&Action::SetCodexMode(CodexCliMode::Force)));
        assert!(actions.contains(&Action::Undo(SurfaceKind::ClaudeCli)));
        assert!(actions.contains(&Action::SetLoginItem(false)));
        assert!(actions.contains(&Action::Quit));
        assert!(!actions.contains(&Action::SetCodexMode(CodexCliMode::Notify)), "the current mode is not clickable");
    }

    /// The entries of the top-level submenu titled `title`.
    fn submenu_of<'a>(entries: &'a [Entry], title: &str) -> &'a Vec<Entry> {
        entries
            .iter()
            .find_map(|e| match e {
                Entry::Submenu { text, entries } if text == title => Some(entries),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no submenu `{title}` in\n{}", to_text(entries)))
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
