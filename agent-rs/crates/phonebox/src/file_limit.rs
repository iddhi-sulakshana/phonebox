//! The open-file limit .NET raises at start-up (`INIT_IncreaseDescriptorLimit`): the soft limit goes up to the hard
//! one, at most `OPEN_MAX` (10240) on macOS. Without it the agent keeps launchd's 256, and a few hundred sockets end
//! accepting on both ports and starting adb.

/// macOS's `OPEN_MAX`, the most `setrlimit` accepts there.
const MACOS_OPEN_MAX: u64 = 10240;

/// The soft limit to set, None when `soft` already reaches it.
pub fn target(soft: u64, hard: u64, macos: bool) -> Option<u64> {
    let wanted = if macos {
        hard.min(MACOS_OPEN_MAX)
    } else {
        hard
    };
    (soft < wanted).then_some(wanted)
}

/// Raises this process's soft open-file limit (Unix; nothing elsewhere).
pub fn raise() {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a valid rlimit the call writes.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return;
        }
        if let Some(soft) = target(limit.rlim_cur, limit.rlim_max, cfg!(target_os = "macos")) {
            limit.rlim_cur = soft;
            // SAFETY: `limit` is a valid rlimit; a refusal leaves the limit as it was.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_soft_limit_rises_to_the_hard_one() {
        assert_eq!(target(1024, 1_048_576, false), Some(1_048_576));
        assert_eq!(target(256, u64::MAX, true), Some(10240));
        assert_eq!(target(256, 4096, true), Some(4096));
        assert_eq!(target(4096, 4096, false), None);
        assert_eq!(target(20000, u64::MAX, true), None);
    }
}
