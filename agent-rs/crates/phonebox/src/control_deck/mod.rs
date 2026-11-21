//! The Control Deck (`ControlDeck*.cs`): audio devices and sessions, media players, internet radio and sounds, push to
//! talk, power and platform actions, tasks, MIDI and OSC — for the phone's controls page.

pub mod core;
pub mod fetch;
pub mod hls;
pub mod leases;
pub mod midi;
pub mod models;
pub mod portable;
pub mod radio_directory;
pub mod radio_logos;
pub mod tasks;
#[cfg(test)]
pub(crate) mod testing;

/// The deck's native side on this platform: the command-line players off Windows (Windows' own comes with the
/// Windows tasks; until then nothing there).
pub fn platform_native(
    audio: std::sync::Arc<crate::audio_control::AudioControl>,
    media: std::sync::Arc<dyn crate::media_monitor::SystemMedia>,
) -> std::sync::Arc<dyn models::DeckNative> {
    #[cfg(windows)]
    {
        let _ = (audio, media);
        std::sync::Arc::new(core::NoNative)
    }
    #[cfg(not(windows))]
    {
        std::sync::Arc::new(portable::PortableDeckNative::new(audio, media, None))
    }
}

/// Monotonic milliseconds (`Environment.TickCount64` in the C# agent): leases, confirmations and caches.
pub(crate) fn monotonic_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

/// `OscPacket`: an OSC message with one float argument (address and type tag padded with NULs to 4 bytes, the float
/// big-endian).
pub fn osc_packet(address: &str, value: f32) -> Result<Vec<u8>, String> {
    if !address.starts_with('/')
        || address.encode_utf16().count() > 256
        || !address.chars().all(|c| ('!'..='~').contains(&c))
    {
        return Err("Invalid OSC address".into());
    }
    fn text(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(s.as_bytes());
        // always at least one NUL, padded to 4 bytes
        let padded = (s.len() + 4) / 4 * 4;
        out.resize(out.len() + padded - s.len(), 0);
    }
    let mut packet = Vec::new();
    text(&mut packet, address);
    text(&mut packet, ",f");
    packet.extend_from_slice(&value.to_be_bytes());
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_uses_padded_strings_and_network_byte_order() {
        assert_eq!(
            osc_packet("/f", 1.0).unwrap(),
            [47, 102, 0, 0, 44, 102, 0, 0, 63, 128, 0, 0]
        );
        assert_eq!(
            osc_packet("not-an-address", 1.0).unwrap_err(),
            "Invalid OSC address"
        );
        assert_eq!(osc_packet("/a b", 1.0).unwrap_err(), "Invalid OSC address");
        assert!(osc_packet(&format!("/{}", "x".repeat(255)), 1.0).is_ok());
        assert!(osc_packet(&format!("/{}", "x".repeat(256)), 1.0).is_err());
        // a 4-byte address still gets a NUL: 8 bytes
        assert_eq!(&osc_packet("/abc", 0.0).unwrap()[..8], b"/abc\0\0\0\0");
    }

    #[test]
    fn phone_configuration_does_not_expose_control_secrets_or_commands() {
        let cfg: crate::config::AgentConfig = serde_json::from_str(
            r#"{"controls":{"spotifyToken":"private-token","actions":[{"id":"sound","target":"private-file-path"}]}}"#,
        )
        .unwrap();
        let json = crate::config::public::to_public(&cfg, None, None).to_string();
        assert!(!json.contains("private-token"));
        assert!(!json.contains("private-file-path"));
        assert!(crate::lan::allowed_over_lan("/api/controls/state"));
        assert!(!crate::lan::allowed_over_lan("/api/controls/capture"));
    }
}
