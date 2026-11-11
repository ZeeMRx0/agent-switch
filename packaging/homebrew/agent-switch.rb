cask "agent-switch" do
  version "__VERSION__"
  sha256 "__SHA256__"

  url "https://github.com/ZeeMRx0/agent-switch/releases/download/v#{version}/Agent-Switch-#{version}-arm64.zip"
  name "Agent Switch"
  desc "Switch Claude, ChatGPT and Antigravity accounts per surface"
  homepage "https://github.com/ZeeMRx0/agent-switch"

  depends_on arch: :arm64
  depends_on macos: :ventura

  app_path = "#{Dir.home}/Applications/Agent Switch.app"

  app "Agent Switch.app", target: "~/Applications/Agent Switch.app"
  # Install steps run in a sandbox that cannot reach Launch Services, so `open` cannot run there.
  # An installer script runs outside the sandbox, but before the app is moved into place.
  # It starts a detached helper that waits for the finished app, removes the download quarantine flag (the app is
  # ad-hoc signed, so this skips the Gatekeeper prompt), then opens it (it gives up after 60 seconds).
  installer script: {
    executable:   "/bin/sh",
    args:         [
      "-c",
      'app="$1"; ( n=0; ' \
      'while { [ ! -f "$app/Contents/_CodeSignature/CodeResources" ] || ' \
      '[ ! -f "$app/Contents/MacOS/agent-switch-menubar" ]; } && [ "$n" -lt 60 ]; ' \
      "do sleep 1; n=$((n + 1)); done; " \
      '[ -f "$app/Contents/MacOS/agent-switch-menubar" ] && sleep 1 && ' \
      '/usr/bin/xattr -dr com.apple.quarantine "$app"; /usr/bin/open "$app" ) ' \
      ">/dev/null 2>&1 </dev/null &",
      "open-agent-switch",
      app_path,
    ],
    must_succeed: false,
  }
  binary "#{app_path}/Contents/MacOS/agent-switch"

  preflight_steps do
    # The old Homebrew formula links the same command name. Stop before changing anything.
    run "/bin/sh",
        args: [
          "-c",
          'case "$(readlink "$1" 2>/dev/null)" in *Cellar/agent-switch/*) ' \
          'echo "The old agent-switch formula is installed. ' \
          'Run \`brew uninstall --formula agent-switch\`, then run this command again." >&2; exit 1;; esac',
          "check-formula",
          "{{HOMEBREW_PREFIX}}/bin/agent-switch",
        ]

    # Steps run in a sandbox that cannot send an Apple Event, so they cannot quit the old app.
    # Ask the user to quit it. An upgrade quits and reopens the new app through `uninstall quit:`.
    run "/bin/sh",
        args: [
          "-c",
          'if pgrep -f "[A]gentSwitch.app/Contents/MacOS/agent-switch-menubar" >/dev/null; then ' \
          'echo "The old AgentSwitch app is running. Quit it from its menu, ' \
          'then run the command again." >&2; exit 1; fi',
        ]

    # Remove the files of a script install.
    remove "Applications/AgentSwitch.app", base: :home, recursive: true
    remove ".local/bin/agent-switch", base: :home
  end

  uninstall quit: "dev.agent-switch.menubar"
end
