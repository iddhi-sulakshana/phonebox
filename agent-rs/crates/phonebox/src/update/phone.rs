//! The phone app's updates and installs over adb (`UpdateService.cs:182-257, 420-458`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;

/// How long one adb install may take.
const INSTALL_LIMIT: Duration = Duration::from_secs(180);

/// Runs adb for the updater; a fake in tests.
pub trait PhoneInstaller: Send + Sync {
    /// Where adb is (`AdbReverse.FindAdb`).
    fn locate(&self, configured: Option<&str>) -> Option<PathBuf>;
    /// `Run`: adb with these arguments (an argument list: the APK's path may hold spaces), its output and error text
    /// together (adb reports `Failure [...]` on stderr); an error when it could not run or passed 3 minutes.
    fn run<'a>(&'a self, adb: &'a Path, args: Vec<String>)
    -> BoxFuture<'a, Result<String, String>>;
}

/// The real adb.
pub struct AdbInstaller;

impl PhoneInstaller for AdbInstaller {
    fn locate(&self, configured: Option<&str>) -> Option<PathBuf> {
        crate::adb::find_adb(configured)
    }

    fn run<'a>(
        &'a self,
        adb: &'a Path,
        args: Vec<String>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let command = crate::tool_process::ToolCommand::new(adb).args(args);
            crate::tool_process::run(&command, INSTALL_LIMIT, std::future::pending())
                .await
                .map(|o| o.stdout + &o.stderr)
                .map_err(|e| e.to_string())
        })
    }
}

/// `PhoneTargets`: every connected phone, or, when the app the phones report is already current, only those whose
/// update failed before (the reported version comes from whichever phone spoke last).
pub fn phone_targets(connected: &[String], app_current: bool, retry: &[String]) -> Vec<String> {
    connected
        .iter()
        .filter(|s| !app_current || retry.contains(s))
        .cloned()
        .collect()
}

/// `NextPhoneRetry`: earlier failures not tried now, then the ones that failed now, each once.
pub fn next_phone_retry(retry: &[String], attempted: &[String], failed: &[String]) -> Vec<String> {
    let mut next: Vec<String> = Vec::new();
    for s in retry
        .iter()
        .filter(|s| !attempted.contains(s))
        .chain(failed)
    {
        if !next.contains(s) {
            next.push(s.clone());
        }
    }
    next
}

/// The last line of adb's output (`output.Trim().Split('\n').LastOrDefault()`).
pub fn last_line(output: &str) -> &str {
    output.trim().split('\n').next_back().unwrap_or("")
}

/// Whether a phone failure is new enough to log: the same refusal every minute (a phone still on upstream's app,
/// which the fork's cannot update) is logged once (a departure: C# logged it on every round).
pub fn log_once(last: &mut Option<String>, message: &str) -> bool {
    if last.as_deref() == Some(message) {
        return false;
    }
    *last = Some(message.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn when_the_phones_report_the_current_app_only_the_ones_that_failed_are_tried_again() {
        assert_eq!(phone_targets(&v(&["A", "B"]), true, &v(&["A"])), v(&["A"]));
        assert!(phone_targets(&v(&["B"]), true, &v(&["A"])).is_empty());
        assert_eq!(phone_targets(&v(&["A", "B"]), false, &[]), v(&["A", "B"]));
    }

    #[test]
    fn a_phone_stays_pending_until_its_own_update_works() {
        assert_eq!(
            next_phone_retry(&[], &v(&["A", "B"]), &v(&["A"])),
            v(&["A"])
        );
        assert_eq!(next_phone_retry(&v(&["A"]), &[], &[]), v(&["A"]));
        assert!(next_phone_retry(&v(&["A"]), &v(&["A"]), &[]).is_empty());
        assert_eq!(
            next_phone_retry(&v(&["A", "C"]), &v(&["A", "B"]), &v(&["B"])),
            v(&["C", "B"])
        );
    }

    #[test]
    fn a_refused_phone_install_is_logged_once() {
        let mut last = None;
        let refused = "phone update failed on SER1: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE]";
        assert!(log_once(&mut last, refused));
        assert!(!log_once(&mut last, refused));
        assert!(log_once(
            &mut last,
            "phone update failed on SER1: Failure [LOCKED]"
        ));
        assert_eq!(
            last_line("Performing Streamed Install\nFailure [LOCKED]\n"),
            "Failure [LOCKED]"
        );
    }
}
