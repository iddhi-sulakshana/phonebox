//! Port of `FileLogger.cs`: writes `agent.log` (the agent has no console window on Windows).
//!
//! Line format: `yyyy-MM-dd HH:mm:ss <lvl4> <Category>: <message>[ | <error>]`, local time. The file starts over when it
//! is larger than 2 MB at start-up. Writes after [`FileLogger::dispose`] are dropped silently.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Start over once the log exceeds 2 MB (checked at start-up).
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Information,
    Warning,
    Error,
    Critical,
}

impl Level {
    /// `level.ToString()[..4].ToLowerInvariant()` in the C# logger.
    pub fn short(self) -> &'static str {
        match self {
            Level::Trace => "trac",
            Level::Debug => "debu",
            Level::Information => "info",
            Level::Warning => "warn",
            Level::Error => "erro",
            Level::Critical => "crit",
        }
    }
}

pub struct FileLogger {
    file: Mutex<Option<File>>,
}

impl FileLogger {
    pub fn new(path: &Path) -> std::io::Result<Arc<FileLogger>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let append = !std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES);
        let file = if append {
            OpenOptions::new().create(true).append(true).open(path)?
        } else {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)?
        };
        Ok(Arc::new(FileLogger {
            file: Mutex::new(Some(file)),
        }))
    }

    /// `IsEnabled` in the C# logger: Information and up (Warning and up for Microsoft's own categories).
    pub fn is_enabled(level: Level, category: &str) -> bool {
        level
            >= if category.starts_with("Microsoft") {
                Level::Warning
            } else {
                Level::Information
            }
    }

    /// Formats lazily (outside the lock) and appends one line, unless the logger was disposed meanwhile.
    pub fn log_with(&self, level: Level, category: &str, format: impl FnOnce() -> String) {
        if !Self::is_enabled(level, category) {
            return;
        }
        let message = format();
        let line = format!(
            "{} {} {}: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            level.short(),
            short_category(category),
            message
        );
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn log(&self, level: Level, category: &str, message: &str) {
        self.log_with(level, category, || message.to_string());
    }

    pub fn dispose(&self) {
        self.file.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// The C# category is the last segment of the logger's type name: `PhoneBox.Agent.AdbReverse` → `AdbReverse`.
pub fn short_category(category: &str) -> &str {
    let after_dot = category.rsplit('.').next().unwrap_or(category);
    after_dot.rsplit("::").next().unwrap_or(after_dot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn logging_after_disposal_does_not_throw_or_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let logger = FileLogger::new(&path).unwrap();
        logger.log(Level::Information, "PhoneBox.Fixture", "before shutdown");
        logger.dispose();
        logger.dispose();
        logger.log(Level::Information, "PhoneBox.Fixture", "after shutdown");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("before shutdown"));
        assert!(!text.contains("after shutdown"));
    }

    #[test]
    fn disposing_while_a_log_is_being_formatted_does_not_fail_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let logger = FileLogger::new(&path).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let l = logger.clone();
        let pending = std::thread::spawn(move || {
            l.log_with(Level::Information, "PhoneBox.Fixture", || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                "late".to_string()
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        logger.dispose();
        release_tx.send(()).unwrap();
        pending.join().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn lines_use_the_csharp_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let logger = FileLogger::new(&path).unwrap();
        logger.log(
            Level::Warning,
            "PhoneBox.Agent.AdbReverse",
            "USB: something",
        );
        logger.log(Level::Information, "PhoneBoxAgent", "ready");
        logger.log(Level::Debug, "PhoneBoxAgent", "below the minimum level"); // IsEnabled: Information and up
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        // 2026-11-20 13:35:18 warn AdbReverse: USB: something
        let re = |l: &str, lvl: &str, rest: &str| {
            assert_eq!(&l[4..5], "-");
            assert_eq!(&l[10..11], " ");
            assert_eq!(&l[13..14], ":");
            assert_eq!(&l[19..], format!(" {lvl} {rest}"));
        };
        re(lines[0], "warn", "AdbReverse: USB: something");
        re(lines[1], "info", "PhoneBoxAgent: ready");
    }

    #[test]
    fn a_log_over_two_megabytes_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        std::fs::write(&path, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
        FileLogger::new(&path)
            .unwrap()
            .log(Level::Information, "A", "fresh");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("20"));
        std::fs::write(&path, "kept\n").unwrap();
        FileLogger::new(&path)
            .unwrap()
            .log(Level::Information, "A", "appended");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("kept\n")
        );
    }

    #[test]
    fn short_category_is_the_last_segment() {
        assert_eq!(short_category("PhoneBox.Agent.AdbReverse"), "AdbReverse");
        assert_eq!(short_category("phonebox::adb"), "adb");
        assert_eq!(short_category("PhoneBoxAgent"), "PhoneBoxAgent");
    }
}
