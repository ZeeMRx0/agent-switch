//! Keychain access through `/usr/bin/security`, with the secret on stdin (`security -i`), never argv.
//!
//! Ported from claude-swap `macos_keychain.py` (stdin piping, exit code 44 = not found, 5 s timeout,
//! absolute binary path). Unlike claude-swap there is NO argv fallback. Measured on this Mac (Darwin 27):
//!
//! * `security -i` reads one command per line through a 4096-byte buffer. A longer line is silently split:
//!   the first ~4095 bytes still run as a command (writing a truncated value) and the rest is parsed as a
//!   new command, with the exit code 1 and an error that echoes part of the secret. So we refuse long lines.
//! * `-X` hex doubles the size (about 1990 bytes of payload), so we pick the shortest of single-quoted,
//!   double-quoted and hex. A compact JSON login with no `'` fits about 4030 bytes single-quoted.
//! * Backslash line continuation and quoting do not extend the line limit.
//! * `-w` with no value reads a password prompt; piped into a non-tty it silently truncates at 128 bytes.
//!   There is no argv-free path through `/usr/bin/security` for larger values.
//!
//! Values must be printable ASCII: `find-generic-password -w` prints non-printable data hex-encoded, which
//! would break the round trip.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::keychain::{validate_name, Keychain};
use crate::proc;

pub const SECURITY_BIN: &str = "/usr/bin/security";
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// Longest command line (including the newline) we send. The tool's buffer is 4096 bytes; keep headroom.
pub const STDIN_LINE_LIMIT: usize = 4090;
const NOT_FOUND: i32 = 44;

pub struct SecurityCli {
    binary: PathBuf,
}

impl Default for SecurityCli {
    fn default() -> Self {
        SecurityCli { binary: PathBuf::from(SECURITY_BIN) }
    }
}

fn single_quote(s: &str) -> String {
    format!("'{s}'")
}

fn encode_value(value: &str) -> String {
    let mut best: Option<String> = None;
    let mut consider = |form: String| {
        if best.as_ref().is_none_or(|b| form.len() < b.len()) {
            best = Some(form);
        }
    };
    if !value.contains('\'') {
        consider(format!("-w '{value}'"));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    consider(format!("-w \"{escaped}\""));
    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    consider(format!("-X {hex}"));
    best.expect("at least one form")
}

/// Build the `add-generic-password` line for `security -i`. Fails closed when it would not fit.
pub fn encode_add_line(service: &str, account: &str, value: &str) -> Result<String> {
    validate_name("service", service)?;
    validate_name("account", account)?;
    if value.is_empty() || !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(Error::Keychain("value must be non-empty printable ASCII (escape non-ASCII JSON as \\uXXXX)".into()));
    }
    let line = format!(
        "add-generic-password -U -s {} -a {} {}\n",
        single_quote(service),
        single_quote(account),
        encode_value(value)
    );
    if line.len() > STDIN_LINE_LIMIT {
        return Err(Error::TooLarge { len: line.len(), limit: STDIN_LINE_LIMIT });
    }
    Ok(line)
}

impl SecurityCli {
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        SecurityCli { binary: binary.into() }
    }

    fn first_line(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).lines().next().unwrap_or("").chars().take(200).collect()
    }
}

impl Keychain for SecurityCli {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        validate_name("service", service)?;
        validate_name("account", account)?;
        let out = proc::run(&self.binary, &["find-generic-password", "-s", service, "-a", account, "-w"], None, TIMEOUT)?;
        match out.code {
            0 => {
                let mut text = String::from_utf8(out.stdout).map_err(|_| Error::Keychain("item is not UTF-8 text".into()))?;
                if text.ends_with('\n') {
                    text.pop();
                }
                Ok(Some(text))
            }
            NOT_FOUND => Ok(None),
            c => Err(Error::Keychain(format!("security find-generic-password exited {c}: {}", Self::first_line(&out.stderr)))),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        let line = encode_add_line(service, account, value)?;
        let out = proc::run(&self.binary, &["-i"], Some(line.as_bytes()), TIMEOUT)?;
        // Never include stderr here: on a malformed line it echoes part of the command, i.e. the secret.
        if out.code != 0 {
            return Err(Error::Keychain(format!("security add-generic-password exited {}", out.code)));
        }
        match self.get(service, account)? {
            Some(back) if back == value => Ok(()),
            _ => Err(Error::Keychain("read-back after write did not match".into())),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool> {
        validate_name("service", service)?;
        validate_name("account", account)?;
        let out = proc::run(&self.binary, &["delete-generic-password", "-s", service, "-a", account], None, TIMEOUT)?;
        match out.code {
            0 => Ok(true),
            NOT_FOUND => Ok(false),
            c => Err(Error::Keychain(format!("security delete-generic-password exited {c}: {}", Self::first_line(&out.stderr)))),
        }
    }

    fn name(&self) -> &'static str {
        "security-cli"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"b"}}"#;

    #[test]
    fn prefers_single_quotes_when_possible() {
        let line = encode_add_line("agent-switch", "acme:claude-oauth", JSON).unwrap();
        assert_eq!(
            line,
            format!("add-generic-password -U -s 'agent-switch' -a 'acme:claude-oauth' -w '{JSON}'\n")
        );
    }

    #[test]
    fn falls_back_to_double_quotes_for_values_with_a_single_quote() {
        let v = r#"{"n":"it's"}"#;
        let line = encode_add_line("s", "a", v).unwrap();
        assert!(line.contains(r#"-w "{\"n\":\"it's\"}""#), "{line}");
    }

    #[test]
    fn falls_back_to_hex_when_shorter() {
        let v = r"\\\\'"; // 4 backslashes and a quote: double-quoted needs 11 chars, hex 10
        let line = encode_add_line("s", "a", v).unwrap();
        assert!(line.contains(" -X "), "{line}");
    }

    #[test]
    fn refuses_values_that_overflow_the_stdin_line() {
        let big = format!("{{\"k\":\"{}\"}}", "A".repeat(4100));
        assert!(matches!(encode_add_line("s", "a", &big), Err(Error::TooLarge { .. })));
        let edge_ok = format!("{{\"k\":\"{}\"}}", "A".repeat(3900));
        assert!(encode_add_line("agent-switch", "personal:claude-oauth", &edge_ok).is_ok());
    }

    #[test]
    fn rejects_non_ascii_control_and_empty_values() {
        assert!(encode_add_line("s", "a", "caf\u{e9}").is_err());
        assert!(encode_add_line("s", "a", "a\nb").is_err());
        assert!(encode_add_line("s", "a", "").is_err());
    }

    #[test]
    fn rejects_names_that_could_break_the_line() {
        assert!(encode_add_line("s'x", "a", "v").is_err());
        assert!(encode_add_line("s", "a\nb", "v").is_err());
    }
}
