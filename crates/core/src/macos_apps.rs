//! Quit an app, wait until every one of its processes is gone, and open it again. Shared by the ChatGPT
//! adapter and the Claude Desktop adapter.
//!
//! Everything that touches the machine sits behind `System`, so tests drive the quit-and-wait logic with a
//! fake. A quit request is only a request: an app can refuse it (an unsaved-work dialog), so success means
//! "every process by PID is gone", never "the AppleScript call returned".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::proc;

const PS: &str = "/bin/ps";
const OSASCRIPT: &str = "/usr/bin/osascript";
const OPEN: &str = "/usr/bin/open";
const KILL: &str = "/bin/kill";
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// A macOS app identified by bundle id and install path. Both are program constants, never user input, so
/// they can go into an AppleScript line without escaping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppTarget {
    pub name: &'static str,
    pub bundle_id: &'static str,
    /// For example `/Applications/ChatGPT.app`. Processes whose executable lives inside belong to the app.
    pub bundle_path: &'static str,
}

pub const CHATGPT: AppTarget =
    AppTarget { name: "ChatGPT", bundle_id: "com.openai.codex", bundle_path: "/Applications/ChatGPT.app" };

pub const CLAUDE: AppTarget =
    AppTarget { name: "Claude", bundle_id: "com.anthropic.claudefordesktop", bundle_path: "/Applications/Claude.app" };

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    /// The full command line as `ps` prints it.
    pub command: String,
}

impl ProcInfo {
    /// The first word of the command line. An executable path with spaces is cut short, which is fine for
    /// the basename checks used here (they only look for `codex`).
    pub fn program(&self) -> &str {
        self.command.split_whitespace().next().unwrap_or("")
    }

    pub fn basename(&self) -> &str {
        Path::new(self.program()).file_name().and_then(|n| n.to_str()).unwrap_or("")
    }

    /// Arguments after the program, as plain words.
    pub fn args(&self) -> Vec<&str> {
        self.command.split_whitespace().skip(1).collect()
    }
}

/// Everything the quit-and-wait logic needs from the machine.
pub trait System {
    fn processes(&self) -> Result<Vec<ProcInfo>>;
    /// The value of environment variable `key` in process `pid`, when the OS shows it to us.
    fn env_var(&self, pid: u32, key: &str) -> Result<Option<String>>;
    /// Ask the app to quit (AppleScript). Does nothing when the app is not running.
    fn request_quit(&self, target: &AppTarget) -> Result<()>;
    fn open(&self, target: &AppTarget) -> Result<()>;
    /// Start a new instance of the app (`open -n`) with command-line arguments after `--args`.
    fn open_new_with_args(&self, target: &AppTarget, args: &[String]) -> Result<()>;
    fn terminate(&self, pid: u32) -> Result<()>;
}

/// Processes whose executable is inside the app bundle: the main process, helpers and bundled children
/// such as the app's own `codex app-server`.
pub fn bundle_pids(procs: &[ProcInfo], target: &AppTarget) -> Vec<u32> {
    let prefix = format!("{}/Contents/", target.bundle_path);
    procs.iter().filter(|p| p.command.starts_with(&prefix)).map(|p| p.pid).collect()
}

/// A running copy of an app: its main process and the profile folder it was started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub pid: u32,
    /// The `--user-data-dir` argument; `None` means the app's own default profile.
    pub profile: Option<PathBuf>,
}

/// The value of `--user-data-dir=` in a command line. `ps` joins arguments with single spaces, so the
/// value runs up to the next ` --` word (a profile path under `Application Support` holds a space).
pub fn user_data_dir_arg(command: &str) -> Option<PathBuf> {
    const FLAG: &str = "--user-data-dir=";
    let start = command.find(FLAG)? + FLAG.len();
    let rest = &command[start..];
    let end = rest.find(" --").unwrap_or(rest.len());
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// Main processes of `target` (not helpers), each with the profile it runs on. The app takes no
/// single-instance lock, so several can run at once.
pub fn instances(procs: &[ProcInfo], target: &AppTarget) -> Vec<Instance> {
    let prefix = format!("{}/Contents/MacOS/", target.bundle_path);
    procs
        .iter()
        .filter(|p| p.command.starts_with(&prefix))
        .map(|p| Instance { pid: p.pid, profile: user_data_dir_arg(&p.command) })
        .collect()
}

pub struct QuitOpts {
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for QuitOpts {
    fn default() -> Self {
        QuitOpts { timeout: Duration::from_secs(30), poll: Duration::from_millis(250) }
    }
}

/// Quit `target` and wait until it and every process that `also_wait` names have exited.
/// `also_wait` is called on every poll, so children started late are still caught.
///
/// Returns `Ok(true)` when something was running and is now gone, `Ok(false)` when nothing was running.
/// Fails with a message naming the PIDs that are still alive when the app refuses to quit in time.
pub fn quit_and_wait(
    sys: &dyn System,
    target: &AppTarget,
    also_wait: &dyn Fn(&[ProcInfo]) -> Vec<u32>,
    opts: &QuitOpts,
) -> Result<bool> {
    let alive = |sys: &dyn System| -> Result<Vec<u32>> {
        let procs = sys.processes()?;
        let mut pids = bundle_pids(&procs, target);
        pids.extend(also_wait(&procs));
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    };

    if alive(sys)?.is_empty() {
        return Ok(false);
    }
    sys.request_quit(target)?;
    let deadline = Instant::now() + opts.timeout;
    loop {
        let pids = alive(sys)?;
        if pids.is_empty() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "{} did not quit within {}s (still running: {}). It may be showing a dialog that needs an answer; \\
                 answer it or quit the app yourself, then run the command again",
                target.name,
                opts.timeout.as_secs(),
                join_pids(&pids)
            )));
        }
        std::thread::sleep(opts.poll);
    }
}

/// Quit every instance of `target` and wait until no process of it is left. A scripted quit reaches one
/// instance per request, so the request is repeated every `reask` while anything is alive. Returns
/// `Ok(true)` when something was running and is now gone. Fails naming the PIDs still alive after the timeout.
pub fn quit_all_instances(sys: &dyn System, target: &AppTarget, opts: &QuitOpts, reask: Duration) -> Result<bool> {
    let alive = || -> Result<Vec<u32>> { Ok(bundle_pids(&sys.processes()?, target)) };
    if alive()?.is_empty() {
        return Ok(false);
    }
    let deadline = Instant::now() + opts.timeout;
    let mut next_request = Instant::now();
    loop {
        let pids = alive()?;
        if pids.is_empty() {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Invalid(format!(
                "{} did not quit within {}s (still running: {}). It may be showing a dialog that needs an answer; \
                 answer it or quit the app yourself, then run the command again",
                target.name,
                opts.timeout.as_secs(),
                join_pids(&pids)
            )));
        }
        if now >= next_request {
            sys.request_quit(target)?;
            next_request = now + reask;
        }
        std::thread::sleep(opts.poll);
    }
}

/// `CFBundleShortVersionString` of an installed app, or `None` when it is not installed.
pub fn app_version(bundle_path: &str) -> Option<String> {
    let plist = format!("{bundle_path}/Contents/Info.plist");
    let out = run_tool("/usr/bin/plutil", &["-extract", "CFBundleShortVersionString", "raw", "-o", "-", &plist]).ok()?;
    (out.code == 0).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn join_pids(pids: &[u32]) -> String {
    pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
}

/// The real machine.
pub struct SystemApps;

fn run_tool(program: &str, args: &[&str]) -> Result<proc::Output> {
    proc::run(Path::new(program), args, None, TOOL_TIMEOUT)
}

/// Split `ps -o pid=,command=` output into processes. Lines that do not start with a pid are skipped.
pub fn parse_ps(text: &str) -> Vec<ProcInfo> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            Some(ProcInfo { pid: pid.parse().ok()?, command: command.trim().to_string() })
        })
        .collect()
}

/// Pull `KEY=value` out of the environment tail that `ps -E` appends to the command line. The value ends at
/// the next ` NAME=` word (names are upper-case with digits and underscores), so a path with spaces works.
pub fn env_from_ps_line(line: &str, key: &str) -> Option<String> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let mut end = rest.len();
    for (i, _) in rest.match_indices(' ') {
        let word = rest[i + 1..].split(['=', ' ']).next().unwrap_or("");
        let is_name = !word.is_empty() && word.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if is_name && rest[i + 1..].starts_with(&format!("{word}=")) {
            end = i;
            break;
        }
    }
    Some(rest[..end].trim_end().to_string())
}

impl System for SystemApps {
    fn processes(&self) -> Result<Vec<ProcInfo>> {
        let out = run_tool(PS, &["-axww", "-o", "pid=,command="])?;
        if out.code != 0 {
            return Err(Error::Invalid("could not list processes".into()));
        }
        Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
    }

    fn env_var(&self, pid: u32, key: &str) -> Result<Option<String>> {
        let out = run_tool(PS, &["-E", "-ww", "-p", &pid.to_string(), "-o", "command="])?;
        if out.code != 0 {
            return Ok(None); // the process ended, or the OS hides it
        }
        Ok(env_from_ps_line(&String::from_utf8_lossy(&out.stdout), key))
    }

    fn request_quit(&self, target: &AppTarget) -> Result<()> {
        let id = target.bundle_id;
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Err(Error::Invalid(format!("unsafe bundle id `{id}`")));
        }
        let script = format!("if application id \"{id}\" is running then tell application id \"{id}\" to quit");
        let out = run_tool(OSASCRIPT, &["-e", &script]).map_err(|e| match e {
            Error::Timeout(_) => Error::Invalid(format!(
                "asking {} to quit timed out; macOS may be waiting for you to allow this terminal to control it \
                 (System Settings > Privacy & Security > Automation)",
                target.name
            )),
            other => other,
        })?;
        if out.code != 0 {
            // `osascript` errors never contain secrets; its first line is the useful part.
            let msg = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Invalid(format!("asking {} to quit failed: {}", target.name, msg.lines().next().unwrap_or("").trim())));
        }
        Ok(())
    }

    fn open(&self, target: &AppTarget) -> Result<()> {
        let out = run_tool(OPEN, &["-b", target.bundle_id])?;
        if out.code != 0 {
            return Err(Error::Invalid(format!("could not open {}", target.name)));
        }
        Ok(())
    }

    fn open_new_with_args(&self, target: &AppTarget, args: &[String]) -> Result<()> {
        let mut argv = vec!["-n", "-b", target.bundle_id, "--args"];
        argv.extend(args.iter().map(String::as_str));
        let out = run_tool(OPEN, &argv)?;
        if out.code != 0 {
            return Err(Error::Invalid(format!("could not open {}", target.name)));
        }
        Ok(())
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        let out = run_tool(KILL, &["-TERM", &pid.to_string()])?;
        if out.code != 0 {
            return Err(Error::Invalid(format!("could not signal process {pid}")));
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted machine for tests.
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct FakeSystem {
        pub procs: RefCell<Vec<ProcInfo>>,
        pub env: RefCell<HashMap<(u32, String), String>>,
        /// When true, `request_quit` removes the app's processes. When false the app refuses.
        pub quit_works: bool,
        pub quit_calls: RefCell<u32>,
        pub opened: RefCell<Vec<&'static str>>,
        /// Arguments of every `open_new_with_args` call.
        pub opened_args: RefCell<Vec<Vec<String>>>,
        /// When true, one quit request ends only the first bundle process, like a scripted quit that
        /// reaches one instance at a time.
        pub quit_one_per_request: bool,
        /// When true, `open_new_with_args` starts a main process carrying the arguments.
        pub spawn_on_open: bool,
        pub terminated: RefCell<Vec<u32>>,
    }

    impl FakeSystem {
        pub fn new(quit_works: bool) -> Self {
            FakeSystem { quit_works, ..Default::default() }
        }

        pub fn add(&self, pid: u32, command: &str) {
            self.procs.borrow_mut().push(ProcInfo { pid, command: command.to_string() });
        }
    }

    /// Lets a test keep a handle on the fake while the adapter owns a `Box<dyn System>`.
    impl System for std::rc::Rc<FakeSystem> {
        fn processes(&self) -> Result<Vec<ProcInfo>> {
            self.as_ref().processes()
        }
        fn env_var(&self, pid: u32, key: &str) -> Result<Option<String>> {
            self.as_ref().env_var(pid, key)
        }
        fn request_quit(&self, target: &AppTarget) -> Result<()> {
            self.as_ref().request_quit(target)
        }
        fn open(&self, target: &AppTarget) -> Result<()> {
            self.as_ref().open(target)
        }
        fn open_new_with_args(&self, target: &AppTarget, args: &[String]) -> Result<()> {
            self.as_ref().open_new_with_args(target, args)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.as_ref().terminate(pid)
        }
    }

    impl System for FakeSystem {
        fn processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(self.procs.borrow().clone())
        }
        fn env_var(&self, pid: u32, key: &str) -> Result<Option<String>> {
            Ok(self.env.borrow().get(&(pid, key.to_string())).cloned())
        }
        fn request_quit(&self, target: &AppTarget) -> Result<()> {
            *self.quit_calls.borrow_mut() += 1;
            if self.quit_works {
                let prefix = format!("{}/Contents/", target.bundle_path);
                if self.quit_one_per_request {
                    let mut procs = self.procs.borrow_mut();
                    if let Some(at) = procs.iter().position(|p| p.command.starts_with(&prefix)) {
                        procs.remove(at);
                    }
                } else {
                    self.procs.borrow_mut().retain(|p| !p.command.starts_with(&prefix));
                }
            }
            Ok(())
        }
        fn open(&self, target: &AppTarget) -> Result<()> {
            self.opened.borrow_mut().push(target.name);
            Ok(())
        }
        fn open_new_with_args(&self, target: &AppTarget, args: &[String]) -> Result<()> {
            self.opened.borrow_mut().push(target.name);
            self.opened_args.borrow_mut().push(args.to_vec());
            if self.spawn_on_open {
                let pid = 9_000 + self.opened_args.borrow().len() as u32;
                self.add(pid, &format!("{}/Contents/MacOS/Claude {}", target.bundle_path, args.join(" ")));
            }
            Ok(())
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.terminated.borrow_mut().push(pid);
            self.procs.borrow_mut().retain(|p| p.pid != pid);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSystem;
    use super::*;

    const MAIN: &str = "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT";
    const APP_SERVER: &str = "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex app-server";

    fn quick() -> QuitOpts {
        QuitOpts { timeout: Duration::from_millis(150), poll: Duration::from_millis(10) }
    }

    #[test]
    fn nothing_running_means_no_quit_request() {
        let sys = FakeSystem::new(true);
        sys.add(1, "/usr/bin/other");
        assert!(!quit_and_wait(&sys, &CHATGPT, &|_| vec![], &quick()).unwrap());
        assert_eq!(*sys.quit_calls.borrow(), 0);
    }

    #[test]
    fn waits_until_every_bundle_process_is_gone() {
        let sys = FakeSystem::new(true);
        sys.add(10, MAIN);
        sys.add(11, APP_SERVER);
        sys.add(99, "/usr/bin/other");
        assert!(quit_and_wait(&sys, &CHATGPT, &|_| vec![], &quick()).unwrap());
        assert_eq!(*sys.quit_calls.borrow(), 1);
        assert_eq!(sys.processes().unwrap().len(), 1);
    }

    #[test]
    fn a_refused_quit_times_out_with_the_pids_and_a_clear_message() {
        let sys = FakeSystem::new(false);
        sys.add(10, MAIN);
        sys.add(11, APP_SERVER);
        let err = quit_and_wait(&sys, &CHATGPT, &|_| vec![], &quick()).unwrap_err().to_string();
        assert!(err.contains("ChatGPT did not quit within") && err.contains("10, 11") && err.contains("dialog"), "{err}");
    }

    #[test]
    fn extra_processes_keep_the_wait_open() {
        let sys = FakeSystem::new(true);
        sys.add(10, MAIN);
        sys.add(50, "/Users/me/.vscode/ext/codex app-server");
        let err = quit_and_wait(&sys, &CHATGPT, &|procs| procs.iter().filter(|p| p.pid == 50).map(|p| p.pid).collect(), &quick())
            .unwrap_err()
            .to_string();
        assert!(err.contains("50") && !err.contains("10,"), "{err}");
    }

    const CLAUDE_MAIN: &str = "/Applications/Claude.app/Contents/MacOS/Claude";
    const CLAUDE_HELPER: &str = "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper --type=gpu-process --user-data-dir=/Users/me/Library/Application Support/Claude --gpu-preference=x";

    fn short() -> (QuitOpts, Duration) {
        (QuitOpts { timeout: Duration::from_millis(500), poll: Duration::from_millis(5) }, Duration::from_millis(20))
    }

    #[test]
    fn finds_the_profile_of_each_running_instance_from_its_arguments() {
        let procs = vec![
            ProcInfo { pid: 1, command: CLAUDE_MAIN.into() },
            ProcInfo { pid: 2, command: format!("{CLAUDE_MAIN} --user-data-dir=/Users/me/Library/Application Support/agent-switch/claude-desktop/team") },
            ProcInfo { pid: 3, command: CLAUDE_HELPER.into() },
            ProcInfo { pid: 4, command: "/usr/bin/other --user-data-dir=/x".into() },
        ];
        let got = instances(&procs, &CLAUDE);
        assert_eq!(got.len(), 2, "helpers and other apps are not instances: {got:?}");
        assert_eq!(got[0], Instance { pid: 1, profile: None });
        assert_eq!(got[1].profile.as_deref(), Some(Path::new("/Users/me/Library/Application Support/agent-switch/claude-desktop/team")));
        // A helper's value stops at the next flag, and the space inside the path survives.
        assert_eq!(user_data_dir_arg(CLAUDE_HELPER).as_deref(), Some(Path::new("/Users/me/Library/Application Support/Claude")));
        assert_eq!(user_data_dir_arg("claude --user-data-dir="), None);
    }

    #[test]
    fn quit_all_asks_again_until_every_instance_is_gone() {
        let mut sys = FakeSystem::new(true);
        sys.quit_one_per_request = true;
        sys.add(10, CLAUDE_MAIN);
        sys.add(20, &format!("{CLAUDE_MAIN} --user-data-dir=/p/team"));
        sys.add(21, CLAUDE_HELPER);
        let (opts, reask) = short();
        assert!(quit_all_instances(&sys, &CLAUDE, &opts, reask).unwrap());
        assert!(*sys.quit_calls.borrow() >= 3, "one request per process is needed here");
        assert!(bundle_pids(&sys.processes().unwrap(), &CLAUDE).is_empty());
    }

    #[test]
    fn quit_all_names_the_pids_that_refuse_to_leave() {
        let sys = FakeSystem::new(false);
        sys.add(10, CLAUDE_MAIN);
        sys.add(20, &format!("{CLAUDE_MAIN} --user-data-dir=/p/team"));
        let (opts, reask) = short();
        let err = quit_all_instances(&sys, &CLAUDE, &opts, reask).unwrap_err().to_string();
        assert!(err.contains("Claude did not quit") && err.contains("10, 20"), "{err}");
    }

    #[test]
    fn quit_all_with_nothing_running_sends_no_request() {
        let sys = FakeSystem::new(true);
        let (opts, reask) = short();
        assert!(!quit_all_instances(&sys, &CLAUDE, &opts, reask).unwrap());
        assert_eq!(*sys.quit_calls.borrow(), 0);
    }

    #[test]
    fn parses_ps_output_and_environment_tails() {
        let procs = parse_ps("  101 /bin/zsh -l\n 202   /Applications/ChatGPT.app/Contents/MacOS/ChatGPT\nbad line\n");
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[1].pid, 202);
        assert_eq!(procs[0].args(), vec!["-l"]);
        assert_eq!(procs[1].basename(), "ChatGPT");

        let line = "/x/codex app-server HOME=/Users/me CODEX_HOME=/Users/me/Library/Application Support/c PATH=/bin";
        assert_eq!(env_from_ps_line(line, "CODEX_HOME").as_deref(), Some("/Users/me/Library/Application Support/c"));
        assert_eq!(env_from_ps_line(line, "HOME").as_deref(), Some("/Users/me"));
        assert_eq!(env_from_ps_line(line, "NOPE"), None);
        // The last variable runs to the end of the line.
        assert_eq!(env_from_ps_line("/x/codex a CODEX_HOME=/tmp/h", "CODEX_HOME").as_deref(), Some("/tmp/h"));
    }
}
