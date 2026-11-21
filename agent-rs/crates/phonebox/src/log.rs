//! The agent's log: `agent.log` in the C# format (see [`crate::file_logger`]) plus a console line.

use std::sync::{Arc, OnceLock};

use crate::file_logger::{FileLogger, Level};

static LOGGER: OnceLock<Arc<FileLogger>> = OnceLock::new();

/// Starts writing to `path`; before this (and in tests) lines go to the console only.
pub fn init(path: &std::path::Path) -> std::io::Result<()> {
    let logger = FileLogger::new(path)?;
    let _ = LOGGER.set(logger);
    Ok(())
}

pub fn log(level: Level, category: &str, message: &str) {
    if !FileLogger::is_enabled(level, category) {
        return;
    }
    eprintln!(
        "{} {}: {message}",
        chrono::Local::now().format("%H:%M:%S"),
        crate::file_logger::short_category(category)
    );
    if let Some(l) = LOGGER.get() {
        l.log(level, category, message);
    }
}

pub fn debug(category: &str, message: &str) {
    log(Level::Debug, category, message);
}

pub fn info(category: &str, message: &str) {
    log(Level::Information, category, message);
}

pub fn warn(category: &str, message: &str) {
    log(Level::Warning, category, message);
}

/// Stops writing (the agent is exiting).
pub fn dispose() {
    if let Some(l) = LOGGER.get() {
        l.dispose();
    }
}
