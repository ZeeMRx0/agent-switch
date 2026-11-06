//! The menu as data. `build` turns the saved config and state into `Entry` values; `render` turns those
//! into a `muda` menu. Keeping the two apart lets the tests check every label and action without a screen.
//!
//! Account labels and server-side names are shown as plain text. A `muda` title treats `&` as a mnemonic
//! marker, so `escape_title` doubles it; nothing here is ever passed to a shell or to AppleScript.

use std::collections::HashMap;

use agent_switch_core::config::{Account, Binding, CodexCliMode, Config, Provider, SurfaceKind};
use agent_switch_core::policy;
use agent_switch_core::state::{State, UsageRecord, UsageState, REASON_SWITCH_NEEDED};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::login_item::LoginStatus;

/// Longest account label shown in a menu row.
const MAX_LABEL_CHARS: usize = 48;

/// What a menu click asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Choose the account of a surface, or `unmanaged`.
    Bind(SurfaceKind, Binding),
    SetAuto(bool),
    SetCodexMode(CodexCliMode),
    /// Quit the app, switch it to its bound account and open it again.
    RelaunchApp(SurfaceKind),
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

/// What the menu needs besides the config and the state.
pub struct Ctx {
    pub now: u64,
    pub login: LoginStatus,
    /// False once the watcher thread has ended.
    pub watcher_running: bool,
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
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.chars().count() > MAX_LABEL_CHARS {
        let cut: String = flat.chars().take(MAX_LABEL_CHARS - 1).collect();
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

fn surface_entry(config: &Config, state: &State, kind: SurfaceKind, now: u64) -> Entry {
    let current = policy::current_account(config, state, kind);
    let title = match &current {
        Some(id) => format!("{}: {} · {}", surface_name(kind), label_of(config, id), usage_text(state.usage.get(id), now)),
        None => format!("{}: unmanaged", surface_name(kind)),
    };
    let mut entries: Vec<Entry> = config
        .accounts
        .iter()
        .filter(|a| a.provider == kind.provider())
        .map(|a| {
            let text = format!("{} · {}", account_label(a), usage_text(state.usage.get(&a.id), now));
            Entry::choice(text, current.as_deref() == Some(a.id.as_str()), Action::Bind(kind, Binding::Account(a.id.clone())))
        })
        .collect();
    if entries.is_empty() {
        entries.push(Entry::disabled("No saved accounts"));
    }
    entries.push(Entry::Separator);
    entries.push(Entry::choice("Unmanaged (do not touch)", current.is_none(), Action::Bind(kind, Binding::Unmanaged)));
    Entry::Submenu { text: title, entries }
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
    if !ctx.watcher_running {
        out.push(Entry::Label("The watcher stopped; quit and open agent-switch again".into()));
        out.push(Entry::Separator);
    }
    for provider in [Provider::Claude, Provider::Openai] {
        out.push(Entry::Label(provider_title(provider).into()));
        for kind in SurfaceKind::ALL.into_iter().filter(|k| k.provider() == provider) {
            out.push(surface_entry(config, state, kind, ctx.now));
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
        Ctx { now: NOW, login: LoginStatus::Disabled, watcher_running: true }
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
        // The checked one is the active account and clicking it changes nothing.
        assert_eq!(cli[1], Entry::Item { text: "Team · 5h 9% · 7d 6%".into(), action: None, checked: Some(true), enabled: true });
        assert_eq!(cli.last().unwrap(), &Entry::choice("Unmanaged (do not touch)", false, Action::Bind(SurfaceKind::ClaudeCli, Binding::Unmanaged)));
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
