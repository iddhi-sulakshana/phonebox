//! What runs after the settings page saved (`Program.cs:370-373`): the deck stops what was disabled, the photo frame
//! and the cameras drop what changed, the deck reconciles its radios. Registered at startup in that order; run once
//! per successful save, before the page gets its answer.

use std::sync::{Arc, Mutex};

pub type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct SettingsHooks {
    hooks: Mutex<Vec<Hook>>,
}

impl SettingsHooks {
    pub fn add(&self, hook: Hook) {
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(hook);
    }

    /// A hook that runs before the others (the deck stops what was disabled first).
    pub fn add_first(&self, hook: Hook) {
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(0, hook);
    }

    /// Every hook, in the order they were added (blocking: run it off the async workers).
    pub fn run(&self) {
        // a copy: a hook may take time, and nothing else should wait on this list meanwhile
        let hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for hook in hooks {
            hook();
        }
    }
}
