# agent-switch

A Rust CLI and a macOS menu bar app that switch Claude, ChatGPT and Antigravity accounts per surface, keep your history and settings in place, and can switch by itself when an account nears its usage limit.

| Surface | What a switch does |
|---|---|
| `claude-cli` (Claude Code) | Swaps the login in the Keychain item `Claude Code-credentials` and `oauthAccount` in `~/.claude.json`. A running session follows within about 30 seconds. |
| `claude-desktop` | One Claude Desktop profile folder per account. A switch quits the app, then reopens it on the target profile. |
| `chatgpt-desktop` (ChatGPT app) | Quits the app, saves the login back, swaps `~/.codex/auth.json`, reopens. |
| `codex-cli` | Its own `CODEX_HOME` (a "shallow home": links to `~/.codex` plus its own `auth.json`), or the same swap as the app when bound with it. |
| `antigravity-cli` (`agy`) | Saves the current login back, then swaps the login in the Keychain item `gemini` / `antigravity`. New `agy` sessions use the new account. |

Each surface is bound to an account or left `unmanaged`. A switch touches only the surfaces you bound. macOS only. Design, mechanics and test evidence are in [docs/architecture.md](docs/architecture.md).

## Install

Two ways. Both install a prebuilt, ad-hoc signed `Agent Switch.app` for Apple silicon Macs on macOS 13 or later. Neither needs Rust.

### Homebrew

```sh
brew install zeemrx0/tap/agent-switch
```

This downloads the prebuilt app, so it is fast and needs no Rust. It shows no "unidentified developer" prompt, because the cask clears the download quarantine flag. It then:

- puts `Agent Switch.app` in `~/Applications`;
- links the `agent-switch` command onto your `PATH` from inside that app;
- opens the app.

Upgrade with `brew upgrade`. This quits the running app and opens the new one. Remove the app and the command with `brew uninstall --cask agent-switch`. Your saved accounts in the Keychain vault and the app data stay on the Mac.

### Manual download

1. Download `Agent-Switch-<version>-arm64.dmg` (or the `.zip`) from the [releases page](https://github.com/ZeeMRx0/agent-switch/releases).
2. Drag `Agent Switch.app` to `/Applications` or `~/Applications`.
3. Open it. macOS blocks the first launch because the app has no Apple certificate. Open **System Settings > Privacy & Security**, find the message about "Agent Switch" and click **Open Anyway**. You do this once.
4. To use the `agent-switch` command, link it from inside the app into `~/.local/bin`. Use `~/Applications` instead of `/Applications` if you put the app there. Add `~/.local/bin` to your `PATH` if it is not there yet.

```sh
mkdir -p ~/.local/bin
ln -sf "/Applications/Agent Switch.app/Contents/MacOS/agent-switch" ~/.local/bin/agent-switch
```

The vault keeps logins in the Keychain through `/usr/bin/security`. A login larger than about 4 KB is split into parts. Each release has a new ad-hoc signature, so macOS may ask again for notification or Keychain permission after an upgrade.

### Coming from a script install

The Homebrew install removes `~/Applications/AgentSwitch.app` and `~/.local/bin/agent-switch` for you. If the old app is running, the install stops and asks you to quit it from its menu. Run the command again after that. Your saved accounts stay in the vault.

### Coming from the Homebrew formula

An earlier release came as a Homebrew formula. Remove it first, then install the cask:

```sh
brew uninstall --formula agent-switch
brew install zeemrx0/tap/agent-switch
```

### Build from a checkout

To build and install from a checkout without Homebrew:

```sh
scripts/install.sh
```

This needs no certificate. It needs a Rust toolchain (`rustup`) and the Xcode command line tools. A local install refuses while the Homebrew cask is installed. It builds in release mode, copies `agent-switch` to `~/.local/bin` (make sure that folder is on `PATH`) and installs an ad-hoc signed `Agent Switch.app` to `~/Applications`. It also removes a legacy `AgentSwitch.app`.

Open the app with `open "$HOME/Applications/Agent Switch.app"` and answer the notification prompt. Quit it from its menu before you reinstall.

### Shell setup

Add to `~/.zshrc` so a new terminal points the Codex CLI at its own login:

```sh
eval "$(agent-switch shell-init)"
```

It prints nothing when the Codex CLI shares `~/.codex`. The export applies only to terminal shells, so the ChatGPT app, which reads your shell environment, keeps `~/.codex`. Shells that are already open keep their old environment.

## Use

```sh
agent-switch status                 # each surface: binding, active account, live identity
agent-switch list                   # every saved account with 5-hour and weekly usage
agent-switch doctor                 # config, vault, Keychain backend, saved logins, app versions
```

### Add an account

`add` saves a login in the vault (Keychain service `agent-switch`). Every `--from-*` source is idempotent, accepts `--dry-run`, and never deletes its source.

```sh
# Claude Code: log in with the browser (runs `claude` /login for you)
agent-switch add claude-cli --id work-claude --label Work

# Claude Code: save a login that already sits in a Keychain item
agent-switch add claude-cli --id team-claude --from-keychain-item "Claude Code-credentials-<hash>" --config-dir <the CLAUDE_CONFIG_DIR it was made with>

# Codex / ChatGPT: save one or more auth.json copies; the newest last_refresh wins and is named
agent-switch add openai --id team-openai --from-file ~/.codex/auth.json [--from-file <another copy>]

# Claude Desktop: sign in on a new profile (waits up to 10 minutes) or register an existing profile folder
agent-switch add claude-desktop team-claude
agent-switch add claude-desktop team-claude --from-profile <profile dir>

# Antigravity CLI: sign in with `agy` first, then save that login
agent-switch add antigravity-cli --id work-agy [--label Work]
```

Claude Desktop's own folder is registered in place and protected from removal. Any other `--from-profile` folder is cloned (APFS copy-on-write) into the managed folder. MCP `env` values in the Desktop config are copied as is; the command lists the key names.

### Bind and switch

```sh
agent-switch bind claude-cli personal-claude      # set the binding and activate it
agent-switch bind codex-cli unmanaged             # stop managing a surface
agent-switch switch claude-cli team-claude        # one-off activation, binding unchanged
agent-switch switch claude-desktop team-claude --restart   # --restart consents to quitting the app
agent-switch remove <account>                     # refused while the account is bound or active
```

Desktop switches never quit an app without `--restart` (or a click on the menu bar notification).

### Claude Desktop sessions follow you

Desktop keeps the Code-tab session list per profile, so a new account used to open on an empty list. On every Claude Desktop switch agent-switch now copies the session records (`claude-code-sessions/<account>/<org>/local_*.json`) between your profiles, so each account lists every session with the newest content. Transcripts are already shared in `~/.claude/projects`. A switch prints a line such as `sessions: 2 mirrored, 0 backed up`.

- **Busy guard.** If a session is mid-reply or waiting for your input, the switch refuses before quitting anything and names the session. Add `--force` (CLI) or choose "Switch anyway" in the menu bar (offered for 10 minutes after the refusal) to quit now; that reply is lost. "Switch anyway" ends only the sessions it listed: if a different one is busy when you click, the switch refuses again. Quitting also waits for (and, with `--force`, signals) the Claude Code agents Claude started from its profile folders; a `claude` you started in a terminal is never touched. `--force` also ends the reply by signalling Claude to quit when its own dialog would ask.
- **Changes in several accounts.** The copy with more turns wins and the other copy is saved under `~/Library/Application Support/agent-switch/backups/sessions/`. Deleting a session in one account moves the other copies to that folder; nothing is unlinked.
- **Untested Desktop version.** Mirroring runs only for the Claude Desktop version listed in `docs/architecture.md`. On any other version the switch still happens, with a warning and mirroring off. `doctor` reports the same.
- **Turn it off or on.** `agent-switch desktop-sessions off [--dry-run]` gives each profile back its own sessions (newest content in the profile each came from), moves the other copies to the backup folder and keeps mirroring off. `agent-switch desktop-sessions on` resumes it at the next switch. Both need Claude closed.
- **Not covered.** Cowork sessions, claude.ai web chats and sessions that Desktop runs in its own managed worktrees (the worktree state is per profile and is not copied, so such a session lists everywhere but may not reopen its worktree).

### Auto mode

Auto-switching runs inside the menu bar app (one watcher at a time). Each provider section of the menu has a `CLI: <account>` and a `Desktop: <account>` submenu (Antigravity has only the CLI one). In each one you pick the account (or "Leave unmanaged"), turn auto-switch on or off for that surface, and set the order in which auto-switch uses the accounts ("Priority", with "Move up" and "Move down"). The account rows above them only show which surfaces use each account and its usage. An "Add account…" item at the end of each provider section opens Terminal and runs the matching `agent-switch add` command (for Antigravity, sign in with `agy` first; the window asks for an account id).

The same settings live in `~/.config/agent-switch/config.toml`:

```toml
[auto]
enabled = false          # auto-switch for every surface not listed in [auto.surfaces]
threshold = 90           # percent, applied to both the 5-hour and weekly windows
hysteresis = 5           # a candidate must be at least this far below the threshold
cooldown_minutes = 5     # minimum time between automatic switches of one surface
refresh_inactive = false # opt in: refresh saved logins in the background (see Caveats)
codex_cli_mode = "new-sessions"  # new-sessions | notify | force

[auto.surfaces]          # optional; written by the menu's per-surface "Auto-switch" toggle
claude-cli = true
claude-desktop = false
codex-cli = true
chatgpt-desktop = true
antigravity-cli = true

[priority]               # optional; per surface, account ids, highest first
claude-cli = ["team-claude", "personal-claude"]
```

- A surface without a `[priority]` list puts its bound account first, then the other accounts in file order, so an older config file never makes a surface move away from its binding. Accounts missing from a list come after the listed ones, in file order, and unknown ids are ignored. Removing an account removes it from every list.
- Choosing an account by hand makes it the surface's first priority. This covers picking it in the menu, `agent-switch bind` and `agent-switch switch`. A Desktop pick that is refused (the binding goes back) changes nothing; one that switched the app keeps the binding and the priority even if the app then fails to come up.
- Without `[auto.surfaces]`, `enabled` applies to every surface, so older config files keep working as before. The first per-surface toggle in the menu writes a value for each surface; a surface missing from the table follows `enabled`.
- When the current account reaches the threshold, the surface moves to the first account in its priority order that has room.
- When the current account is below the threshold, the cooldown has passed and no switch-needed event is waiting, the surface moves back to a higher-priority account. That account needs a usage reading from the last 30 minutes with both windows below `threshold - hysteresis`. It also must not be dead, live on another surface that holds its login, or the target of another surface's waiting switch-needed event. A Codex CLI that shares `~/.codex` with the ChatGPT app never splits off to return; the app's event moves both.
- A return needs that fresh reading of the higher-priority account. With `refresh_inactive = false`, an idle account whose saved login has expired may not get a new reading, so a return can be delayed until you switch to it or turn `refresh_inactive` on.
- CLI surfaces (`claude-cli`, `codex-cli`, `antigravity-cli`) switch automatically. Desktop surfaces raise a notification with a "Relaunch" button, both to move on and to move back; nothing quits until you click.
- `codex_cli_mode`: `new-sessions` switches the login now and running Codex sessions keep the old account until restarted; `notify` never switches automatically and offers a button; `force` switches now even if a running session then hits a sign-in error.
- Accounts that are exhausted or have a dead login are skipped. By default a saved login is refreshed only just before you switch to it.

### Repair and Dock launches

```sh
agent-switch repair codex [--dry-run]   # re-link the Codex shallow home to ~/.codex; quit Codex and the ChatGPT app first
agent-switch default-link enable        # make the Claude Desktop default folder a link to the active profile, so Dock launches open the bound account
agent-switch default-link disable       # put the real folder back
```

`doctor` reports drifted shallow-home entries and tells you when `repair codex` is needed.

## Caveats

- **Prompt cache.** The first message after a Claude switch misses the prompt cache and costs a full-context read. This is expected.
- **Server-side chats do not move.** Conversations that live on a provider's servers (claude.ai, ChatGPT web history) belong to the account that created them. Local Claude Code projects and Codex sessions stay shared.
- **Never run `/logout`** in Claude Code or `codex logout` on a managed surface. That revokes the login that the vault also holds, and the saved copy stops working. Use `agent-switch switch` or `bind` instead.
- **Terms of service.** Refreshing OAuth logins outside the official clients is an area that Anthropic's policy for third-party use of subscription OAuth addresses. `refresh_inactive` is off by default for that reason: by default agent-switch refreshes a saved login only just before switching to it, and never the active login (the official client owns it). Read the current terms of your plan and decide.
- **Undocumented endpoints.** Usage comes from `api/oauth/usage` (Claude), `backend-api/wham/usage` (ChatGPT) and `v1internal:fetchAvailableModels` (Antigravity, per-model quota mapped to the two windows). If they change, `list` shows the last-known values with their age and auto mode pauses.
- **Claude login size.** The vault splits large logins into parts. Claude Code's own `Claude Code-credentials` item is always written through `/usr/bin/security`, which accepts about 4 KB. The live Claude login with many MCP logins can exceed that; switches then refuse (fail closed) until you remove unused MCP logins.
- **Time Machine and APFS snapshots keep deleted plaintext.** Removing an old `auth.json` or backup does not remove it from earlier backups or local snapshots. Exclude `~/.agent-switch` from Time Machine (`tmutil addexclusion ~/.agent-switch`), and consider logging in to each account once after migration so the old copies are invalid.
- **Stored secrets.** Logins live in the Keychain. Live auth files (`~/.codex/auth.json`, the shallow home's `auth.json`) are mode `0600`. No token appears in command arguments or logs; emails are masked in output.

## Release

A maintainer pushes a stable tag `v<version>` that matches the version in `Cargo.toml`. The `Release` workflow tests, builds the ZIP and DMG, publishes the GitHub release and updates `Casks/agent-switch.rb` in `ZeeMRx0/homebrew-tap` from `packaging/homebrew/agent-switch.rb`. The repository needs a `HOMEBREW_TAP_TOKEN` secret: a fine-grained token with Contents read/write on `ZeeMRx0/homebrew-tap`.

## Migrating a hand-made setup

Use the general `add` sources above, `--dry-run` first. Before deleting any plaintext copy, confirm that each account passes a live check (`agent-switch list` shows usage read "just now" for it) and has been switched to and back once. A matching identity alone is not enough: a spent refresh token still carries a valid `id_token`. Then back up and replace any hard-coded `CODEX_HOME` line in `~/.zshrc` with the `shell-init` line.

## Development

```sh
cargo build --workspace
cargo test --workspace            # real-Keychain tests are ignored by default: cargo test -p agent-switch-core --test real_keychain -- --ignored --test-threads=1
cargo clippy --workspace --all-targets
agent-switch-menubar --print-menu # render the menu as text without a GUI
```

Workspace crates: `core` (vault, adapters, policy, watcher), `cli`, and `menubar`.

## Credits and license

agent-switch is MIT licensed (see `Cargo.toml`). Ported mechanics and ideas, each from an MIT-licensed project:

- [claude-swap](https://github.com/realiti4/claude-swap) by Onur Cetinkol: directory-lock port, Keychain access through `security`, OAuth profile/usage/refresh contracts, auto-switch policy and polling limits.
- [caam / coding_agent_account_manager](https://github.com/Dicklesworthstone/coding_agent_account_manager) by Jeffrey Emanuel: atomic auth-file writes with forced `0600`, and the ChatGPT workspace usage request. Its license is MIT with a rider that grants no rights to OpenAI, Anthropic, their affiliates, or anyone acting for them; the rider travels with any code derived from it, so read `references/Dicklesworthstone_coding_agent_account_manager/LICENSE` before redistributing.
- [codex-switcher (cxs)](https://github.com/mincia1110/codex-switcher) by Min Chul Kim: the shallow `CODEX_HOME` and per-home socket approach.
- [claude-code-account-switcher](https://github.com/Nemo-Illusionist/claude-code-account-switcher) by Nemo-Illusionist: the Claude Desktop per-profile model and the runtime-folder allowlist for cloned profiles.

Also read for design (no code ported): codex-auth-snap, codex-switch, teamclaude, CLIProxyAPI, hoangvu12/claude-switch and Symbioose/claude-account-switcher. Copies of the references sit in the git-ignored `references/` folder with their licenses.
