//! PhoneBox desktop agent: everything except the process entry point (`phonebox-agent`).
//!
//! Each module ports the C# file of the same name from `agent/PhoneBox.Agent` at upstream commit 43800f9, which is
//! the specification (see docs/superpowers/specs/2026-11-20-rust-agent-design.md).

pub mod adb;
pub mod agent_endpoint;
pub mod app_paths;
pub mod app_restart;
pub mod browser_guard;
pub mod catalog;
pub mod config;
pub mod file_logger;
pub mod icon_text;
pub mod log;
pub mod phone_registry;
pub mod push_store;
pub mod request_timings;
pub mod server;
pub mod single_instance;
pub mod stack_widgets;
pub mod state;
pub mod state_frame_cache;
pub mod state_signal;
pub mod webview_doctor;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_version_is_alpha() {
        assert!(env!("CARGO_PKG_VERSION").starts_with("1.0.0-alpha."));
    }
}
