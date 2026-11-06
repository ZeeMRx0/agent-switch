//! The Keychain backend boundary. Production uses `SecurityCli` or `HelperKeychain`; tests use `MemoryKeychain`.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Result;

/// Generic-password items addressed by (service, account). Values are printable ASCII text (JSON).
pub trait Keychain: Send + Sync {
    /// `Ok(None)` means the item does not exist; any other failure is an error.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    /// Create or replace the item.
    fn set(&self, service: &str, account: &str, value: &str) -> Result<()>;
    /// `Ok(false)` means there was nothing to delete.
    fn delete(&self, service: &str, account: &str) -> Result<bool>;
    /// Short backend name for `doctor`.
    fn name(&self) -> &'static str;
}

/// Service/account names go into `security -i` lines and helper argv: keep them plain.
pub fn validate_name(what: &str, s: &str) -> Result<()> {
    if s.is_empty() || s.chars().any(|c| c.is_control() || c == '\'' || c == '"' || c == '\\') {
        return Err(crate::error::Error::Keychain(format!("invalid keychain {what} name")));
    }
    Ok(())
}

/// In-memory fake for tests.
#[derive(Default)]
pub struct MemoryKeychain {
    items: Mutex<HashMap<(String, String), String>>,
}

impl MemoryKeychain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.lock().expect("keychain mutex").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Keychain for MemoryKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        Ok(self.items.lock().expect("keychain mutex").get(&(service.into(), account.into())).cloned())
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        self.items.lock().expect("keychain mutex").insert((service.into(), account.into()), value.into());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool> {
        Ok(self.items.lock().expect("keychain mutex").remove(&(service.into(), account.into())).is_some())
    }

    fn name(&self) -> &'static str {
        "memory"
    }
}
