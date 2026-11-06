# agent-switch

A Rust CLI and a macOS menu bar app that switch Claude and ChatGPT accounts per surface, keep your history and settings in place, and can switch by itself when an account nears its usage limit.

| Surface | What a switch does |
|---|---|
| `claude-cli` (Claude Code) | Swaps the login in the Keychain item `Claude Code-credentials` and `oauthAccount` in `~/.claude.json`. A running session follows within about 30 seconds. |
| `claude-desktop` | One Claude Desktop profile folder per account. A switch quits the app, then reopens it on the target profile. |
| `chatgpt-desktop` (ChatGPT app) | Quits the app, saves the login back, swaps `~/.codex/auth.json`, reopens. |
| `codex-cli` | Its own `CODEX_HOME` (a "shallow home": links to `~/.codex` plus its own `auth.json`), or the same swap as the app when bound with it. |

Each surface is bound to an account or left `unmanaged`. A switch touches only the surfaces you bound. macOS only. Design, mechanics and test evidence are in [docs/architecture.md](docs/architecture.md).

## Install

Requirements: macOS, a Rust toolchain (`rustup`), Xcode command line tools.

### One-time: create the signing certificate

The menu bar app and the Keychain helper are signed with a stable local identity so that Keychain trust and notification permission survive rebuilds.

1. Open Keychain Access, then Certificate Assistant, then Create a Certificate.
2. Name: `agent-switch Code Signing`. Identity Type: Self Signed Root. Certificate Type: Code Signing. Create it.
3. Check it with `security find-identity -p codesigning`. The first signing run may ask to use the private key: choose Always Allow.

Create the certificate **before** you add accounts. The signed helper opens a different vault than `/usr/bin/security` does, so logins saved with one are invisible to the other. `scripts/install.sh` refuses a signed install over an existing vault unless you pass `--allow-vault-backend-change` (then add your accounts again).

### Build and install

```sh
scripts/install.sh            # signed: needs the certificate
scripts/install.sh --adhoc    # no certificate: CLI plus an ad-hoc signed app
```

This builds in release mode, copies `agent-switch` to `~/.local/bin` (make sure that folder is on `PATH`) and installs `AgentSwitch.app` to `~/Applications`. A signed install also puts `agent-switch-keychain` beside the CLI. With `--adhoc` the vault uses `/usr/bin/security`, which refuses logins larger than about 4 KB in one item (the vault splits larger ones into parts), and the app's Keychain trust and notification permission reset on every reinstall.

Open the app with `open ~/Applications/AgentSwitch.app` and answer the notification prompt. Quit it from its menu before you reinstall.

### Shell setup

Add to `~/.zshrc` so a new terminal points the Codex CLI at its own login:

```sh
eval "$(agent-switch shell-init)"
```

It prints nothing when the Codex CLI shares `~/.codex`. Shells that are already open keep their old environment.

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
```

Claude Desktop's own folder is registered in place and protected from removal. Any other `--from-profile` folder is cloned (APFS copy-on-write) into the managed folder. MCP `env` values in the Desktop config are copied as is; the command lists the key names.

### Bind, switch, undo

```sh
agent-switch bind claude-cli personal-claude      # set the binding and activate it
agent-switch bind codex-cli unmanaged             # stop managing a surface
agent-switch switch claude-cli team-claude        # one-off activation, binding unchanged
agent-switch switch claude-desktop team-claude --restart   # --restart consents to quitting the app
agent-switch undo [surface]                       # re-activate the previous account from its current saved login
agent-switch remove <account>                     # refused while the account is bound or active
```

Desktop switches never quit an app without `--restart` (or a click on the menu bar notification).

### Auto mode

Auto-switching runs inside the menu bar app (one watcher at a time). Configure it in `~/.config/agent-switch/config.toml`:

```toml
[auto]
enabled = false          # also a menu bar toggle
threshold = 90           # percent, applied to both the 5-hour and weekly windows
hysteresis = 5           # a candidate must be at least this far below the threshold
cooldown_minutes = 5     # minimum time between automatic switches of one surface
refresh_inactive = false # opt in: refresh saved logins in the background (see Caveats)
codex_cli_mode = "new-sessions"  # new-sessions | notify | force
```

- CLI surfaces (`claude-cli`, `codex-cli`) switch automatically. Desktop surfaces raise a notification with a "Relaunch" button; nothing quits until you click.
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
- **Undocumented endpoints.** Usage comes from `api/oauth/usage` (Claude) and `backend-api/wham/usage` (ChatGPT). If they change, `list` shows the last-known values with their age and auto mode pauses.
- **Claude login size.** `/usr/bin/security` accepts about 4 KB. The live Claude login with many MCP logins can exceed that; switches then refuse (fail closed) until you remove unused MCP logins. The signed helper has no such limit.
- **Time Machine and APFS snapshots keep deleted plaintext.** Removing an old `auth.json` or backup does not remove it from earlier backups or local snapshots. Exclude `~/.agent-switch` from Time Machine (`tmutil addexclusion ~/.agent-switch`), and consider logging in to each account once after migration so the old copies are invalid.
- **Stored secrets.** Logins live in the Keychain. Live auth files (`~/.codex/auth.json`, the shallow home's `auth.json`) are mode `0600`. No token appears in command arguments or logs; emails are masked in output.

## Migrating a hand-made setup

Use the general `add` sources above, `--dry-run` first. Before deleting any plaintext copy, confirm that each account passes a live check (`agent-switch list` shows usage read "just now" for it) and has been switched to and back once. A matching identity alone is not enough: a spent refresh token still carries a valid `id_token`. Then back up and replace any hard-coded `CODEX_HOME` line in `~/.zshrc` with the `shell-init` line.

## Development

```sh
cargo build --workspace
cargo test --workspace            # real-Keychain tests are ignored by default: cargo test -p agent-switch-core --test real_keychain -- --ignored --test-threads=1
cargo clippy --workspace --all-targets
agent-switch-menubar --print-menu # render the menu as text without a GUI
```

Workspace crates: `core` (vault, adapters, policy, watcher), `cli`, `menubar`, `keychain-helper`.

## Credits and license

agent-switch is MIT licensed (see `Cargo.toml`). Ported mechanics and ideas, each from an MIT-licensed project:

- [claude-swap](https://github.com/realiti4/claude-swap) by Onur Cetinkol: directory-lock port, Keychain access through `security`, OAuth profile/usage/refresh contracts, auto-switch policy and polling limits.
- [caam / coding_agent_account_manager](https://github.com/Dicklesworthstone/coding_agent_account_manager) by Jeffrey Emanuel: atomic auth-file writes with forced `0600`, and the ChatGPT workspace usage request. Its license is MIT with a rider that grants no rights to OpenAI, Anthropic, their affiliates, or anyone acting for them; the rider travels with any code derived from it, so read `references/Dicklesworthstone_coding_agent_account_manager/LICENSE` before redistributing.
- [codex-switcher (cxs)](https://github.com/mincia1110/codex-switcher) by Min Chul Kim: the shallow `CODEX_HOME` and per-home socket approach.
- [claude-code-account-switcher](https://github.com/Nemo-Illusionist/claude-code-account-switcher) by Nemo-Illusionist: the Claude Desktop per-profile model and the runtime-folder allowlist for cloned profiles.

Also read for design (no code ported): codex-auth-snap, codex-switch, teamclaude, CLIProxyAPI, hoangvu12/claude-switch and Symbioose/claude-account-switcher. Copies of the references sit in the git-ignored `references/` folder with their licenses.
