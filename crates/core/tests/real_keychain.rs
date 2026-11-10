//! Real-Keychain tests. Ignored by default because they touch the login keychain (throwaway items only,
//! service `agent-switch-test-<pid>-<tag>`, always cleaned up). Run with:
//!
//!   cargo test -p agent-switch-core --test real_keychain -- --ignored
//!
//! While each write runs, the process list is sampled to prove the secret never appears in any argv.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use agent_switch_core::keychain::Keychain;
use agent_switch_core::security_cli::SecurityCli;
use agent_switch_core::Error;

const MARKER: &str = "TOPSECRETMARKER9f3a";

struct Throwaway<'a> {
    kc: &'a dyn Keychain,
    service: String,
    accounts: Vec<String>,
}

impl<'a> Throwaway<'a> {
    fn new(kc: &'a dyn Keychain, tag: &str) -> Self {
        Throwaway { kc, service: format!("agent-switch-test-{}-{tag}", std::process::id()), accounts: vec![] }
    }
    fn account(&mut self, name: &str) -> String {
        self.accounts.push(name.to_string());
        name.to_string()
    }
}

impl Drop for Throwaway<'_> {
    fn drop(&mut self) {
        for a in &self.accounts {
            let _ = self.kc.delete(&self.service, a);
        }
    }
}

/// Samples `ps` until finished; reports (samples taken, samples that showed the marker).
struct PsSampler {
    stop: Arc<AtomicBool>,
    samples: Arc<AtomicUsize>,
    leaks: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PsSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(AtomicUsize::new(0));
        let leaks = Arc::new(AtomicUsize::new(0));
        let (s, n, l) = (stop.clone(), samples.clone(), leaks.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let out = Command::new("/bin/ps").args(["-axww", "-o", "command="]).output().expect("ps");
                n.fetch_add(1, Ordering::SeqCst);
                if String::from_utf8_lossy(&out.stdout).contains(MARKER) {
                    l.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        PsSampler { stop, samples, leaks, handle: Some(handle) }
    }

    fn finish(mut self) -> (usize, usize) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
        (self.samples.load(Ordering::SeqCst), self.leaks.load(Ordering::SeqCst))
    }
}

/// A Claude-style login blob of exactly `len` bytes of compact JSON, with the marker inside the token.
fn claude_blob(len: usize) -> String {
    let head = format!(r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-{MARKER}-"#);
    let tail = r#"","scopes":["user:inference"]},"mcpOAuth":{}}"#;
    let pad = len - head.len() - tail.len();
    format!("{head}{}{tail}", "A".repeat(pad))
}

fn roundtrip_with_ps_sampling(kc: &dyn Keychain, tag: &str, len: usize) {
    let mut t = Throwaway::new(kc, tag);
    let acct = t.account("test:claude-oauth");
    let blob = claude_blob(len);
    assert_eq!(blob.len(), len);

    // Writes are fast and `ps` is slow, so repeat the write+read until enough samples overlap them.
    let sampler = PsSampler::start();
    let mut rounds = 0;
    while sampler.samples.load(Ordering::SeqCst) < 5 && rounds < 200 {
        kc.set(&t.service, &acct, &blob).expect("write");
        let back = kc.get(&t.service, &acct).expect("read");
        assert_eq!(back.as_deref(), Some(blob.as_str()), "round trip must be byte-identical");
        rounds += 1;
    }
    let (samples, leaks) = sampler.finish();

    assert!(samples >= 5, "sampler barely ran ({samples})");
    assert_eq!(leaks, 0, "secret appeared in the process list ({leaks}/{samples} samples)");
    println!("{tag}: {len} bytes, {rounds} write+read rounds, {samples} ps samples, 0 leaks");
}

#[test]
#[ignore = "touches the login keychain"]
fn security_cli_carries_a_live_sized_blob_without_argv() {
    roundtrip_with_ps_sampling(&SecurityCli::default(), "cli-3k9", 3900);
}

#[test]
#[ignore = "touches the login keychain"]
fn security_cli_refuses_6kb_and_leaves_no_truncated_item() {
    let kc = SecurityCli::default();
    let mut t = Throwaway::new(&kc, "cli-6k");
    let acct = t.account("test:claude-oauth");
    let err = kc.set(&t.service, &acct, &claude_blob(6144)).unwrap_err();
    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
    assert!(!err.to_string().contains(MARKER));
    assert_eq!(kc.get(&t.service, &acct).unwrap(), None, "a refused write must not leave a truncated item");
}

#[test]
#[ignore = "touches the login keychain"]
fn security_cli_round_trips_quotes_backslashes_and_dollars() {
    let kc = SecurityCli::default();
    let mut t = Throwaway::new(&kc, "cli-chars");
    for (i, v) in [r#"{"a":"it's \"q\" \\ $HOME `x` ;"}"#, r#"{"k":"v w"}"#, "plain value  "].iter().enumerate() {
        let acct = t.account(&format!("c{i}"));
        kc.set(&t.service, &acct, v).unwrap();
        assert_eq!(kc.get(&t.service, &acct).unwrap().as_deref(), Some(*v));
    }
}

#[test]
#[ignore = "touches the login keychain"]
fn security_cli_replaces_an_existing_item_and_reports_missing() {
    let kc = SecurityCli::default();
    let mut t = Throwaway::new(&kc, "cli-upd");
    let acct = t.account("u");
    assert_eq!(kc.get(&t.service, &acct).unwrap(), None);
    kc.set(&t.service, &acct, "one").unwrap();
    kc.set(&t.service, &acct, "two").unwrap();
    assert_eq!(kc.get(&t.service, &acct).unwrap().as_deref(), Some("two"));
    assert!(kc.delete(&t.service, &acct).unwrap());
    assert!(!kc.delete(&t.service, &acct).unwrap());
}
