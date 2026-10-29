//! Real OS power backend. Compiled only with `--release --features real-power`.
//!
//! All variants ask the OS politely: applications may veto a shutdown, nothing
//! is force-closed and no reboot is ever requested.

use super::{PowerAction, PowerBackend, PowerError, PowerManager, PowerRecord};
use crate::time::now_millis;

pub struct OsPowerManager;

impl PowerManager for OsPowerManager {
    fn backend(&self) -> PowerBackend {
        PowerBackend::Real
    }

    fn execute(&self, action: PowerAction) -> Result<PowerRecord, PowerError> {
        tracing::warn!(action = action.id(), "requesting real power action from the OS");
        platform::execute(action)?;
        Ok(PowerRecord { at: now_millis(), action, backend: PowerBackend::Real })
    }
}

#[cfg(windows)]
mod platform {
    use super::{PowerAction, PowerError};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Power::SetSuspendState;
    use windows_sys::Win32::System::Shutdown::{
        EWX_POWEROFF, EWX_SHUTDOWN, ExitWindowsEx, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
        SHTDN_REASON_MINOR_OTHER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn execute(action: PowerAction) -> Result<(), PowerError> {
        enable_shutdown_privilege()?;
        match action {
            PowerAction::Sleep => {
                // hibernate = false, force = false, wake events stay enabled.
                let ok = unsafe { SetSuspendState(false, false, false) };
                if !ok {
                    return Err(last_error("SetSuspendState"));
                }
            }
            PowerAction::Shutdown => {
                // No EWX_FORCE / EWX_FORCEIFHUNG: apps may refuse, nothing is killed.
                let reason = SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED;
                let ok = unsafe { ExitWindowsEx(EWX_SHUTDOWN | EWX_POWEROFF, reason) };
                if ok == 0 {
                    return Err(last_error("ExitWindowsEx"));
                }
            }
        }
        Ok(())
    }

    fn enable_shutdown_privilege() -> Result<(), PowerError> {
        let name: Vec<u16> = "SeShutdownPrivilege".encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
                return Err(last_error("OpenProcessToken"));
            }
            let mut luid = LUID { LowPart: 0, HighPart: 0 };
            if LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) == 0 {
                CloseHandle(token);
                return Err(last_error("LookupPrivilegeValueW"));
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            let ok = AdjustTokenPrivileges(token, 0, &tp, 0, null_mut(), null_mut());
            CloseHandle(token);
            if ok == 0 {
                return Err(last_error("AdjustTokenPrivileges"));
            }
        }
        Ok(())
    }

    fn last_error(what: &str) -> PowerError {
        let code = unsafe { GetLastError() };
        PowerError::Failed(format!("{what} failed (Win32 error {code})"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{PowerAction, PowerError};
    use std::process::Command;

    pub fn execute(action: PowerAction) -> Result<(), PowerError> {
        let status = match action {
            PowerAction::Sleep => Command::new("/usr/bin/pmset").arg("sleepnow").status(),
            // Same as Apple menu > Shut Down: apps can cancel, nothing is forced.
            PowerAction::Shutdown => Command::new("/usr/bin/osascript")
                .args(["-e", "tell application \"System Events\" to shut down"])
                .status(),
        }
        .map_err(|e| PowerError::Failed(e.to_string()))?;
        if status.success() { Ok(()) } else { Err(PowerError::Failed(format!("exit status {status}"))) }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::{PowerAction, PowerError};
    use std::process::Command;

    pub fn execute(action: PowerAction) -> Result<(), PowerError> {
        // systemd-logind honours inhibitor locks held by other applications.
        let verb = match action {
            PowerAction::Sleep => "suspend",
            PowerAction::Shutdown => "poweroff",
        };
        let status = Command::new("systemctl")
            .arg(verb)
            .status()
            .map_err(|e| PowerError::Failed(e.to_string()))?;
        if status.success() { Ok(()) } else { Err(PowerError::Failed(format!("exit status {status}"))) }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::{PowerAction, PowerError};

    pub fn execute(_action: PowerAction) -> Result<(), PowerError> {
        Err(PowerError::Unsupported)
    }
}
