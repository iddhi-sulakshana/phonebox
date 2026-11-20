//! Port of `AdbReverse.cs`: finds adb and keeps the USB tunnel to the phone (the loop comes with Task 12).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What the USB side knows right now (for /api/info and the settings page).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsbStatus {
    pub connected: bool,
    pub all_devices: Vec<String>,
    pub missing_app: Vec<String>,
    pub usb_problem: Option<String>,
}

pub struct AdbReverse {
    status: Mutex<UsbStatus>,
}

impl AdbReverse {
    /// No phone known yet.
    pub fn idle() -> Arc<AdbReverse> {
        Arc::new(AdbReverse {
            status: Mutex::new(UsbStatus::default()),
        })
    }
    pub fn status(&self) -> UsbStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn connected(&self) -> bool {
        self.status().connected
    }
}

/// The adb to use: the configured path when it exists, else the first one found on PATH or in the usual places.
pub fn find_adb(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Some(c);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for dir in std::env::split_paths(&path) {
            let dir = PathBuf::from(dir.to_string_lossy().trim().trim_matches('"')); // PATH entries may be quoted
            candidates.push(dir.join("adb.exe"));
        }
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default());
        candidates.push(
            local
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join("adb.exe"),
        );
        if let Ok(entries) =
            std::fs::read_dir(local.join("Microsoft").join("WinGet").join("Packages"))
        {
            for e in entries.flatten() {
                if e.file_name()
                    .to_string_lossy()
                    .starts_with("Google.PlatformTools")
                {
                    candidates.push(e.path().join("platform-tools").join("adb.exe"));
                }
            }
        }
    } else {
        let home = crate::app_paths::home();
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join("adb")),
        );
        candidates.extend([
            home.join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join("adb"),
            home.join("Library")
                .join("Android")
                .join("sdk")
                .join("platform-tools")
                .join("adb"),
            PathBuf::from("/opt/homebrew/bin/adb"),
            PathBuf::from("/usr/local/bin/adb"),
            PathBuf::from("/usr/bin/adb"),
            PathBuf::from("/usr/lib/android-sdk/platform-tools/adb"),
        ]);
    }
    candidates.into_iter().find(|p| p.is_file())
}
