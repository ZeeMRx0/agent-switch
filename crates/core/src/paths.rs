//! Every on-disk location agent-switch owns, rooted at one home folder so tests can use a temp dir.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
}

impl Paths {
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Paths { home: home.into() }
    }

    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::Invalid("HOME is not set".into()))?;
        Ok(Self::from_home(home))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `~/.config/agent-switch/config.toml`
    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/agent-switch/config.toml")
    }

    /// `~/Library/Application Support/agent-switch`
    pub fn support_dir(&self) -> PathBuf {
        self.home.join("Library/Application Support/agent-switch")
    }

    /// `state.json` holds metadata only, never tokens.
    pub fn state_file(&self) -> PathBuf {
        self.support_dir().join("state.json")
    }

    pub fn state_lock_file(&self) -> PathBuf {
        self.locks_dir().join("state.lock")
    }

    /// Serializes read-modify-write cycles of `config.toml`.
    pub fn config_lock_file(&self) -> PathBuf {
        self.locks_dir().join("config.lock")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.support_dir().join("locks")
    }

    pub fn account_lock_file(&self, account_id: &str) -> PathBuf {
        self.locks_dir().join(format!("account-{account_id}.lock"))
    }

    /// `~/Library/Logs/agent-switch`
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("Library/Logs/agent-switch")
    }

    /// The event log; only the single watcher writes it, and it holds no secrets.
    pub fn events_file(&self) -> PathBuf {
        self.logs_dir().join("events.jsonl")
    }

    pub fn watcher_lock_file(&self) -> PathBuf {
        self.locks_dir().join("watcher.lock")
    }
}
