//! PhoneBox desktop agent: everything except the process entry point (`phonebox-agent`).
//!
//! Each module ports the C# file of the same name from `agent/PhoneBox.Agent` at upstream commit 43800f9, which is
//! the specification (see docs/superpowers/specs/2026-11-20-rust-agent-design.md).

pub mod adb;
pub mod agent_endpoint;
pub mod app_catalog;
pub mod app_paths;
pub mod app_restart;
pub mod audio_control;
pub mod battery_monitor;
pub mod browser_guard;
pub mod catalog;
pub mod config;
pub mod der;
pub mod dictation;
pub mod drm_usage;
pub mod file_logger;
pub mod gpus;
pub mod icon_text;
pub mod key_sender;
pub mod lan;
pub mod lan_cert;
pub mod listening_ports;
pub mod log;
pub mod media_artwork_cache;
pub mod media_monitor;
pub mod net_adapters;
pub mod panel_widgets;
pub mod phone_registry;
pub mod ping_monitor;
pub mod pkcs12;
pub mod png;
pub mod process_monitor;
pub mod push_store;
pub mod request_timings;
pub mod server;
pub mod shortcut_icon_uploads;
pub mod shortcut_icons;
pub mod shortcuts;
pub mod single_instance;
pub mod spectrum_analyzer;
pub mod stack_widgets;
pub mod state;
pub mod state_frame_cache;
pub mod state_signal;
pub mod system_monitor;
pub mod trackpad_handler;
pub mod unix;
pub mod usb_doctor;
pub mod webview_doctor;
pub mod widget_feed_proxy;
pub mod win_icon;
pub mod windows_gpu;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_version_is_alpha() {
        assert!(env!("CARGO_PKG_VERSION").starts_with("1.0.0-alpha."));
    }
}
