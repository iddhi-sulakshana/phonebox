//! PhoneBox desktop agent: everything except the process entry point (`phonebox-agent`).
//!
//! Each module ports the C# file of the same name from `agent/PhoneBox.Agent` at upstream commit 43800f9, which is
//! the specification (see docs/superpowers/specs/2026-11-20-rust-agent-design.md).

pub mod adb;
pub mod agent_endpoint;
pub mod app_catalog;
pub mod app_paths;
pub mod app_restart;
pub mod approval_store;
pub mod audio_control;
pub mod battery_monitor;
pub mod browser_guard;
pub mod calendar;
pub mod camera_feeds;
pub mod catalog;
pub mod claude_hook;
pub mod claude_transcript;
pub mod codex_bar;
pub mod command_runner;
pub mod config;
pub mod control_deck;
pub mod der;
pub mod dictation;
pub mod discord_monitor;
pub mod docker_monitor;
pub mod dotnet_text;
pub mod drm_usage;
pub mod feeds;
pub mod file_logger;
pub mod github_monitor;
pub mod gpus;
pub mod home_assistant;
pub mod icon_text;
pub mod idle;
pub mod json_text;
pub mod key_sender;
pub mod lan;
pub mod lan_cert;
pub mod listening_ports;
pub mod log;
pub mod media_artwork_cache;
pub mod media_monitor;
pub mod net;
pub mod net_adapters;
pub mod note_edit;
pub mod obs_monitor;
pub mod pairing;
pub mod panel_widgets;
pub mod phone_registry;
pub mod photo_frame;
pub mod ping_monitor;
pub mod pkcs12;
pub mod png;
pub mod privacy_monitor;
pub mod process_monitor;
pub mod push_store;
pub mod quota;
pub mod request_timings;
pub mod screen_sync;
pub mod server;
pub mod session_lock_monitor;
pub mod session_monitor;
pub mod settings_hooks;
pub mod shortcut_icon_uploads;
pub mod shortcut_icons;
pub mod shortcuts;
pub mod single_instance;
pub mod spectrum_analyzer;
pub mod spotify_control;
pub mod stack_widgets;
pub mod state;
pub mod state_frame_cache;
pub mod state_signal;
pub mod system_monitor;
pub mod tool_process;
pub mod trackpad_handler;
pub mod unix;
pub mod usb_doctor;
pub mod wake;
pub mod webview_doctor;
pub mod widget_feed_proxy;
pub mod win_icon;
pub mod windows_gpu;
pub mod work_buddy;

#[cfg(test)]
mod core_audit_tests;
#[cfg(test)]
pub(crate) mod testutil;
#[cfg(test)]
mod tests {
    #[test]
    fn crate_version_is_alpha() {
        assert!(env!("CARGO_PKG_VERSION").starts_with("1.0.0-alpha."));
    }
}
