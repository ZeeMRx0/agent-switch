//! Client for the `agent-switch-keychain` helper binary (SecItem*, signed with a stable identity).
//!
//! Protocol: `agent-switch-keychain get|set|delete <service> <account>`. `set` reads the value from stdin
//! until EOF, `get` writes it to stdout. Exit 0 = ok, 44 = not found, anything else = error with a
//! secret-free message on stderr. Same 5 s timeout as `security`.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::keychain::{validate_name, Keychain};
use crate::proc;
use crate::security_cli::TIMEOUT;

pub const HELPER_NAME: &str = "agent-switch-keychain";
const NOT_FOUND: i32 = 44;

pub struct HelperKeychain {
    binary: PathBuf,
}

impl HelperKeychain {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        HelperKeychain { binary: binary.into() }
    }

    /// The helper next to the running executable, if present.
    pub fn beside(exe: &Path) -> Option<PathBuf> {
        let candidate = exe.parent()?.join(HELPER_NAME);
        candidate.is_file().then_some(candidate)
    }

    fn call(&self, verb: &str, service: &str, account: &str, stdin: Option<&[u8]>) -> Result<proc::Output> {
        validate_name("service", service)?;
        validate_name("account", account)?;
        proc::run(&self.binary, &[verb, service, account], stdin, TIMEOUT)
    }

    fn err(verb: &str, out: &proc::Output) -> Error {
        let msg: String = String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("").chars().take(200).collect();
        Error::Keychain(format!("keychain helper {verb} exited {}: {msg}", out.code))
    }
}

impl Keychain for HelperKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = self.call("get", service, account, None)?;
        match out.code {
            0 => String::from_utf8(out.stdout).map(Some).map_err(|_| Error::Keychain("item is not UTF-8 text".into())),
            NOT_FOUND => Ok(None),
            _ => Err(Self::err("get", &out)),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        if value.is_empty() {
            return Err(Error::Keychain("refusing to store an empty value".into()));
        }
        let out = self.call("set", service, account, Some(value.as_bytes()))?;
        if out.code != 0 {
            return Err(Self::err("set", &out));
        }
        match self.get(service, account)? {
            Some(back) if back == value => Ok(()),
            _ => Err(Error::Keychain("read-back after write did not match".into())),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool> {
        let out = self.call("delete", service, account, None)?;
        match out.code {
            0 => Ok(true),
            NOT_FOUND => Ok(false),
            _ => Err(Self::err("delete", &out)),
        }
    }

    fn name(&self) -> &'static str {
        "signed-helper"
    }
}
