//! The stable code-signing identity that makes the keychain helper's items prompt-free across rebuilds.
//! Created once by hand in Keychain Access; `scripts/sign.sh` signs the built binaries with it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Result;
use crate::helper_keychain::HelperKeychain;
use crate::keychain::Keychain;
use crate::proc;
use crate::security_cli::{SecurityCli, SECURITY_BIN};

pub const DEFAULT_IDENTITY: &str = "agent-switch Code Signing";
pub const IDENTITY_ENV: &str = "AGENT_SWITCH_SIGN_IDENTITY";
const CODESIGN_BIN: &str = "/usr/bin/codesign";
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn identity_name() -> String {
    std::env::var(IDENTITY_ENV).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_IDENTITY.to_string())
}

/// True when the login keychain holds a code-signing identity with exactly this name.
pub fn identity_exists(name: &str) -> Result<bool> {
    let out = proc::run(Path::new(SECURITY_BIN), &["find-identity", "-p", "codesigning"], None, TIMEOUT)?;
    let needle = format!("\"{name}\"");
    Ok(String::from_utf8_lossy(&out.stdout).contains(&needle))
}

/// True when `binary` carries a signature whose authority is the named identity.
pub fn signed_by(binary: &Path, name: &str) -> Result<bool> {
    let out = proc::run(Path::new(CODESIGN_BIN), &["-dvv", &binary.display().to_string()], None, TIMEOUT)?;
    let needle = format!("Authority={name}");
    Ok(String::from_utf8_lossy(&out.stderr).lines().any(|l| l.trim() == needle))
}

/// Production backend choice: the helper when it sits beside `exe` and is signed with our identity
/// (so it will not prompt), otherwise `/usr/bin/security` with its fail-closed size limit.
pub fn detect_keychain(exe: &Path) -> Box<dyn Keychain> {
    if let Some(helper) = HelperKeychain::beside(exe) {
        if signed_by(&helper, &identity_name()).unwrap_or(false) {
            return Box::new(HelperKeychain::new(helper));
        }
    }
    Box::new(SecurityCli::default())
}

pub fn helper_path_beside(exe: &Path) -> Option<PathBuf> {
    HelperKeychain::beside(exe)
}
