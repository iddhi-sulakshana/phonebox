//! Port of `SessionLockMonitor.cs`: while display.followComputerLock is on (macOS), the phones' screens follow the
//! computer's lock: asleep while it is locked, awake with the panel after.

use std::sync::Arc;
use std::time::Duration;

use crate::adb::AdbReverse;
use crate::config::store::ConfigStore;
use crate::screen_sync::SyncError;
use crate::server::Stop;

const CATEGORY: &str = "PhoneBox.Agent.SessionLockMonitor";

/// `MacSessionLock.IsLocked`: whether the macOS console session is locked (or not on the console); None elsewhere and
/// when the agent is not in the GUI session (started over ssh, or as a daemon). A missing lock key counts as unlocked.
pub fn is_locked() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        mac::is_locked()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Every second until `stop`: follow the lock while the option is on (macOS), else forget the phones' states.
pub fn start(
    cfg: Arc<ConfigStore>,
    adb: Arc<AdbReverse>,
    stop: Stop,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut warned = false;
        while !stop.is_stopped() {
            let pass = if cfg.get().display.follow_computer_lock && cfg!(target_os = "macos") {
                adb.sync_session(&stop, false).await.map(|available| {
                    if !available && !warned {
                        crate::log::warn(
                            CATEGORY,
                            "Lock sync is unavailable; start the agent in the macOS GUI session",
                        );
                    }
                    warned = !available;
                })
            } else {
                adb.reset_session(&stop).await.map(|()| warned = false)
            };
            let pause = match pass {
                Ok(()) => Duration::from_secs(1),
                Err(SyncError::Cancelled) if stop.is_stopped() => return,
                Err(e) => {
                    crate::log::warn(
                        CATEGORY,
                        &format!("Could not follow the macOS session lock: {e}"),
                    );
                    Duration::from_secs(5)
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(pause) => {}
                _ = stop.stopped() => return,
            }
        }
    })
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    type CFTypeRef = *const c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            text: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CFTypeRef) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    const UTF8: u32 = 0x0800_0100;

    pub fn is_locked() -> Option<bool> {
        // SAFETY: CoreGraphics returns an owned dictionary (or null), released below
        let session = unsafe { CGSessionCopyCurrentDictionary() };
        if session.is_null() {
            return None; // not in the GUI session
        }
        let on_console = boolean(session, c"kCGSSessionOnConsoleKey");
        let locked = boolean(session, c"CGSSessionScreenIsLocked");
        unsafe { CFRelease(session) };
        Some(on_console == Some(false) || locked == Some(true))
    }

    fn boolean(dictionary: CFTypeRef, name: &CStr) -> Option<bool> {
        // SAFETY: the key is created and released here; the value belongs to the dictionary
        unsafe {
            let key = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8);
            if key.is_null() {
                return None;
            }
            let value = CFDictionaryGetValue(dictionary, key);
            let result = (!value.is_null() && CFGetTypeID(value) == CFBooleanGetTypeID())
                .then(|| CFBooleanGetValue(value) != 0);
            CFRelease(key);
            result
        }
    }
}
