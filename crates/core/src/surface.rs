//! The adapter boundary. Each surface (Claude Code CLI, Claude Desktop, Codex CLI, ChatGPT Desktop) and any
//! future provider implements `Surface` with its own paths. Usage is a separate seam: a new provider
//! implements `usage::ProviderApi` and gets a `Provider` arm in `LiveApi`. Config, vault and locks
//! stay unchanged.

use std::fmt;

use crate::config::{Account, SurfaceKind};
#[cfg(test)]
use crate::error::Error;
use crate::error::Result;
use crate::paths::Paths;
use crate::vault::{Slot, Vault};

/// Everything an adapter may need from the shared core.
pub struct Env<'a> {
    pub paths: &'a Paths,
    pub vault: &'a Vault,
}

/// Who is logged in on a surface right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// The agent-switch account whose vault slot matches the live login, if any.
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub uuid: Option<String>,
    /// Free text such as "split state" for `status` and `doctor`.
    pub note: Option<String>,
}

/// Secret bytes prepared for `apply`. Debug output never shows the payload.
#[derive(Clone)]
pub struct Staged {
    pub account_id: String,
    pub payload: Vec<u8>,
    /// Account ids whose account locks the caller (`switch::activate`) holds while `apply` runs.
    /// `apply` must not lock these again (a second `flock` on another file description would wait for the
    /// first), and must lock any other account it writes to.
    pub held: Vec<String>,
}

impl Staged {
    pub fn new(account_id: String, payload: Vec<u8>) -> Staged {
        Staged { account_id, payload, held: Vec::new() }
    }
}

impl fmt::Debug for Staged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staged").field("account_id", &self.account_id).field("payload", &"<redacted>").finish()
    }
}

pub trait Surface {
    fn kind(&self) -> SurfaceKind;

    /// Who is logged in on this surface now. `Ok(None)` means logged out or nothing readable.
    fn current_identity(&self, env: &Env) -> Result<Option<Identity>>;

    /// Read the live login into a vault slot shape. Optional: Claude Desktop has no login blob.
    fn capture(&self, _env: &Env, _account: &Account) -> Result<Option<Slot>> {
        Ok(None)
    }

    /// Prepare the target account. Writes no live store and has no side effects, so a watcher can stage
    /// without quitting anything.
    fn stage(&self, env: &Env, account: &Account) -> Result<Staged>;

    /// Write the live store. Never quits an app.
    fn apply(&self, env: &Env, staged: Staged) -> Result<()>;

    /// Whether the surface's app is running now. `None` for surfaces that are not apps.
    fn is_running(&self, _env: &Env) -> Result<Option<bool>> {
        Ok(None)
    }

    /// The only call that may quit and reopen an app. CLI surfaces have nothing to restart.
    fn restart(&self, _env: &Env) -> Result<()> {
        Ok(())
    }
}

/// A surface that supports nothing; tests that only need a `Surface` value use it.
#[cfg(test)]
pub struct StubSurface {
    kind: SurfaceKind,
}

#[cfg(test)]
impl StubSurface {
    pub fn new(kind: SurfaceKind) -> Self {
        StubSurface { kind }
    }

    fn missing<T>(&self, what: &str) -> Result<T> {
        Err(Error::Invalid(format!("{what} is not supported by the stub surface {}", self.kind)))
    }
}

#[cfg(test)]
impl Surface for StubSurface {
    fn kind(&self) -> SurfaceKind {
        self.kind
    }

    fn current_identity(&self, _env: &Env) -> Result<Option<Identity>> {
        self.missing("reading the live login")
    }

    fn stage(&self, _env: &Env, _account: &Account) -> Result<Staged> {
        self.missing("switching")
    }

    fn apply(&self, _env: &Env, _staged: Staged) -> Result<()> {
        self.missing("switching")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_debug_hides_the_payload() {
        let s = Staged::new("a".into(), b"super-secret".to_vec());
        let text = format!("{s:?}");
        assert!(!text.contains("super-secret") && text.contains("redacted"));
    }
}
