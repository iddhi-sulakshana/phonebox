//! Helpers the deck's tests share.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::config::store::ConfigStore;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A config file with this JSON, in `dir`.
pub(crate) fn store(dir: &tempfile::TempDir, json: &str) -> Arc<ConfigStore> {
    let path = dir.path().join("config.json");
    std::fs::write(&path, json).unwrap();
    Arc::new(ConfigStore::new(path))
}

/// A set-once signal a test thread can wait on (`ManualResetEventSlim`).
#[derive(Default)]
pub(crate) struct Event {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Event {
    pub(crate) fn set(&self) {
        *lock(&self.set) = true;
        self.changed.notify_all();
    }
    pub(crate) fn is_set(&self) -> bool {
        *lock(&self.set)
    }
    pub(crate) fn wait(&self, limit: Duration) -> bool {
        let guard = lock(&self.set);
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, limit, |set| !*set)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

/// Waits for `event` off the async workers.
pub(crate) async fn wait_for(event: &Arc<Event>, limit: Duration) -> bool {
    let event = event.clone();
    tokio::task::spawn_blocking(move || event.wait(limit))
        .await
        .unwrap()
}
