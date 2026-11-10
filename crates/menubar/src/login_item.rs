//! The "Start at login" switch, through `SMAppService.mainApp`. It works only from the bundled app.

use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::notify::running_in_bundle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    Enabled,
    Disabled,
    /// Registered, but the user must allow it in System Settings > General > Login Items.
    NeedsApproval,
    Unavailable(String),
}

pub fn status() -> LoginStatus {
    if !running_in_bundle() {
        return LoginStatus::Unavailable("not running from the app bundle".into());
    }
    // SAFETY: `mainAppService` takes no arguments and returns a retained service object.
    let service = unsafe { SMAppService::mainAppService() };
    // SAFETY: `status` is a plain getter on that object.
    let status = unsafe { service.status() };
    if status == SMAppServiceStatus::Enabled {
        LoginStatus::Enabled
    } else if status == SMAppServiceStatus::RequiresApproval {
        LoginStatus::NeedsApproval
    } else {
        // An app that never registered reports NotFound, not NotRegistered, and `register` still works
        // from that state. A real failure shows up as a notice when the user turns the switch on.
        LoginStatus::Disabled
    }
}

/// Register or unregister the app as a login item and return the status afterwards.
pub fn set(enabled: bool) -> Result<LoginStatus, String> {
    if !running_in_bundle() {
        return Err("the login item needs the app bundle (scripts/bundle-app.sh)".into());
    }
    // SAFETY: as in `status`; the two calls below return an error object instead of throwing.
    let service = unsafe { SMAppService::mainAppService() };
    let result = unsafe {
        if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    result.map_err(|e| e.localizedDescription().to_string())?;
    Ok(status())
}
