//! `TrackpadHandler`: the Trackpad widget moves the pointer, clicks and scrolls, over a WebSocket (or one POST per
//! gesture). Only while a trackpad widget with `control` on is on an enabled page; that is checked again for every
//! payload, so turning it off takes effect at once.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::AgentConfig;
use crate::config::store::ConfigStore;

const MAX_MOVEMENT_DELTA: i32 = 500;
const MAX_SCROLL_DELTA: i32 = 1000;
const CATEGORY: &str = "PhoneBox.Agent.Trackpad";

/// One gesture from the panel. Names match in any case; `type` wins over `action`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackpadPayload {
    pub r#type: Option<String>,
    pub action: Option<String>,
    pub dx: Option<f64>,
    pub dy: Option<f64>,
    pub delta: Option<f64>,
}

impl TrackpadPayload {
    /// `JsonSerializer.Deserialize<TrackpadPayload>` with the web defaults: Ok(None) for a JSON `null`, Err for anything
    /// that is not an object of the right kinds (numbers may also come as strings).
    pub fn parse(utf8: &[u8]) -> Result<Option<TrackpadPayload>, String> {
        use serde_json::Value;
        let root: Value = serde_json::from_slice(utf8).map_err(|e| e.to_string())?;
        let map = match root {
            Value::Null => return Ok(None),
            Value::Object(map) => map,
            other => return Err(format!("not an object: {other}")),
        };
        let text = |v: &Value| match v {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            other => Err(format!("not a string: {other}")),
        };
        // AllowReadingFromString: "1.5" counts as 1.5, but not "NaN", "Infinity" or a padded number
        let number = |v: &Value| match v {
            Value::Null => Ok(None),
            Value::Number(n) => n
                .as_f64()
                .map(Some)
                .ok_or_else(|| format!("not a number: {n}")),
            Value::String(s)
                if !s.is_empty()
                    && s.bytes().all(|b| {
                        b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')
                    }) =>
            {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(Some)
                    .ok_or_else(|| format!("not a number: {s}"))
            }
            other => Err(format!("not a number: {other}")),
        };
        let mut p = TrackpadPayload::default();
        for (key, value) in &map {
            match key.to_ascii_lowercase().as_str() {
                "type" => p.r#type = text(value)?,
                "action" => p.action = text(value)?,
                "dx" => p.dx = number(value)?,
                "dy" => p.dy = number(value)?,
                "delta" => p.delta = number(value)?,
                _ => {}
            }
        }
        Ok(Some(p))
    }
}

/// The left button is held (a drag); released when the socket ends.
static LEFT_BUTTON_DOWN: AtomicBool = AtomicBool::new(false);

/// One warning per process for a failed mouse action (a missing xdotool would fail every gesture).
static EXECUTION_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn is_allowed(config: &AgentConfig) -> bool {
    config
        .widgets(true)
        .iter()
        .any(|w| w.r#type == "trackpad" && w.bool("control") == Some(true))
}

/// Parses and runs one gesture, if the settings still allow it; never fails.
pub fn process_payload(utf8: &[u8], config: &ConfigStore) {
    if !is_allowed(&config.get()) {
        crate::log::debug(
            CATEGORY,
            "Trackpad payload ignored: widget not enabled with control=true",
        );
        return;
    }
    match TrackpadPayload::parse(utf8) {
        Ok(Some(msg)) => {
            if let Err(e) = execute(&msg)
                && !EXECUTION_ERROR_LOGGED.swap(true, Ordering::SeqCst)
            {
                crate::log::warn(CATEGORY, &format!("Trackpad execution failed: {e}"));
            }
        }
        Ok(None) => {}
        Err(e) => crate::log::debug(CATEGORY, &format!("Failed to parse trackpad payload: {e}")),
    }
}

/// A delta as the C# agent takes it: rounded half to even, clamped; NaN and infinities are 0.
fn delta(v: Option<f64>, max: i32) -> i32 {
    match v {
        Some(v) if v.is_finite() => (v.round_ties_even() as i32).clamp(-max, max),
        _ => 0,
    }
}

pub fn execute(msg: &TrackpadPayload) -> Result<(), String> {
    use crate::key_sender as k;
    let action = msg
        .r#type
        .as_ref()
        .or(msg.action.as_ref())
        .map(|a| a.to_lowercase());
    match action.as_deref() {
        Some("move") => {
            let dx = delta(msg.dx, MAX_MOVEMENT_DELTA);
            let dy = delta(msg.dy, MAX_MOVEMENT_DELTA);
            if dx != 0 || dy != 0 {
                k::mouse_move(dx, dy)?;
            }
        }
        Some("click") => {
            LEFT_BUTTON_DOWN.store(false, Ordering::SeqCst);
            k::mouse_click()?;
        }
        Some("mousedown" | "down") => {
            LEFT_BUTTON_DOWN.store(true, Ordering::SeqCst);
            k::mouse_down()?;
        }
        Some("mouseup" | "up") => {
            LEFT_BUTTON_DOWN.store(false, Ordering::SeqCst);
            k::mouse_up()?;
        }
        Some("rightclick") => k::mouse_right_click()?,
        Some("scroll") => {
            let vertical = delta(msg.delta.or(msg.dy), MAX_SCROLL_DELTA);
            let horizontal = delta(msg.dx, MAX_SCROLL_DELTA);
            if horizontal != 0 {
                k::mouse_horizontal_scroll(horizontal)?;
            }
            if vertical != 0 {
                k::mouse_scroll(vertical)?;
            }
        }
        Some("hscroll") => {
            let horizontal = delta(msg.delta.or(msg.dx), MAX_SCROLL_DELTA);
            if horizontal != 0 {
                k::mouse_horizontal_scroll(horizontal)?;
            }
        }
        _ => {} // unknown message types are ignored
    }
    Ok(())
}

/// Releases a held button (the socket ended in the middle of a drag).
pub fn release_held_button() {
    if LEFT_BUTTON_DOWN.swap(false, Ordering::SeqCst) {
        let _ = crate::key_sender::mouse_up();
    }
}

/// The C# agent receives into a 1 KiB buffer, so a longer message reaches it as pieces of broken JSON: ignored.
pub const MAX_PAYLOAD: usize = 1024;

/// The socket loop: each text message is one gesture, until the client closes, the connection drops or the agent
/// stops; a button still held then is released.
pub async fn handle_socket(
    mut socket: axum::extract::ws::WebSocket,
    config: std::sync::Arc<ConfigStore>,
    stop: crate::server::Stop,
) {
    use axum::extract::ws::Message;
    loop {
        let message = tokio::select! {
            m = socket.recv() => m,
            _ = stop.stopped() => break,
        };
        match message {
            Some(Ok(Message::Text(text))) if !text.is_empty() && text.len() <= MAX_PAYLOAD => {
                let config = config.clone();
                let bytes = text.as_bytes().to_vec();
                // mouse actions block (SendInput, xdotool): off the async threads, one at a time in order
                let _ = tokio::task::spawn_blocking(move || process_payload(&bytes, &config)).await;
            }
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {} // binary, ping and pong frames, empty and oversized texts
            Some(Err(e)) => {
                crate::log::debug(
                    CATEGORY,
                    &format!("Trackpad WebSocket connection closed: {e}"),
                );
                break;
            }
        }
    }
    tokio::task::spawn_blocking(release_held_button).await.ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_sender::{
        MouseCall, MouseRecorder, mouse_click, mouse_down, mouse_horizontal_scroll,
    };
    use crate::key_sender::{mouse_move, mouse_right_click, mouse_scroll, mouse_up};

    fn call(action: &str, dx: i32, dy: i32, delta: i32) -> MouseCall {
        MouseCall {
            action: action.into(),
            dx,
            dy,
            delta,
        }
    }

    fn payload(t: &str, dx: Option<f64>, dy: Option<f64>, delta: Option<f64>) -> TrackpadPayload {
        TrackpadPayload {
            r#type: Some(t.into()),
            action: None,
            dx,
            dy,
            delta,
        }
    }

    fn config(json: &str) -> AgentConfig {
        crate::config::read::parse(json).unwrap()
    }

    #[test]
    fn mouse_methods_invoke_test_seam() {
        let rec = MouseRecorder::start();
        mouse_move(5, -3).unwrap();
        mouse_click().unwrap();
        mouse_down().unwrap();
        mouse_up().unwrap();
        mouse_right_click().unwrap();
        mouse_scroll(120).unwrap();
        mouse_horizontal_scroll(-80).unwrap();
        assert_eq!(
            rec.calls(),
            [
                call("move", 5, -3, 0),
                call("click", 0, 0, 0),
                call("mouseDown", 0, 0, 0),
                call("mouseUp", 0, 0, 0),
                call("rightClick", 0, 0, 0),
                call("scroll", 0, 0, 120),
                call("hscroll", 0, 0, -80),
            ]
        );
    }

    #[test]
    fn move_is_clamped_to_sane_bounds() {
        for (dx, dy, ex, ey) in [
            (10.2, -5.6, 10, -6),
            (800.0, -900.0, 500, -500),
            (-750.0, 600.0, -500, 500),
            // half to even, as Math.Round
            (2.5, -3.5, 2, -4),
            (1e20, -1e20, 500, -500),
        ] {
            let rec = MouseRecorder::start();
            execute(&payload("move", Some(dx), Some(dy), None)).unwrap();
            assert_eq!(rec.calls(), [call("move", ex, ey, 0)], "{dx} {dy}");
        }
    }

    #[test]
    fn scroll_is_clamped_to_sane_bounds() {
        for (t, delta, expected) in [
            ("scroll", 2500.0, 1000),
            ("scroll", -2500.0, -1000),
            ("hscroll", 1500.0, 1000),
            ("hscroll", -1500.0, -1000),
        ] {
            let rec = MouseRecorder::start();
            execute(&payload(t, None, None, Some(delta))).unwrap();
            let calls = rec.calls();
            assert_eq!(calls.len(), 1, "{t} {delta}");
            assert_eq!(calls[0].delta, expected);
        }
    }

    #[test]
    fn scroll_falls_back_to_dy_and_scrolls_sideways_first() {
        let rec = MouseRecorder::start();
        execute(&payload("scroll", Some(-30.0), Some(40.0), None)).unwrap();
        assert_eq!(
            rec.calls(),
            [call("hscroll", 0, 0, -30), call("scroll", 0, 0, 40)]
        );
        drop(rec);
        let rec = MouseRecorder::start();
        execute(&payload("hscroll", Some(12.0), None, None)).unwrap();
        assert_eq!(rec.calls(), [call("hscroll", 0, 0, 12)]);
    }

    #[test]
    fn unknown_action_types_are_ignored() {
        for t in ["unknown", "drop_table", "exec"] {
            let rec = MouseRecorder::start();
            execute(&payload(t, Some(10.0), Some(10.0), Some(50.0))).unwrap();
            assert!(rec.calls().is_empty(), "{t}");
        }
    }

    #[test]
    fn the_action_table_and_type_before_action() {
        let rec = MouseRecorder::start();
        for (t, a) in [
            (Some("CLICK"), None),
            (Some("down"), None),
            (Some("MouseUp"), None),
            (None, Some("rightclick")),
            (Some("mousedown"), Some("click")),
            (Some("up"), None),
        ] {
            let p = TrackpadPayload {
                r#type: t.map(Into::into),
                action: a.map(Into::into),
                ..Default::default()
            };
            execute(&p).unwrap();
        }
        let actions: Vec<String> = rec.calls().into_iter().map(|c| c.action).collect();
        assert_eq!(
            actions,
            [
                "click",
                "mouseDown",
                "mouseUp",
                "rightClick",
                "mouseDown",
                "mouseUp"
            ]
        );
    }

    #[test]
    fn nan_and_infinity_do_not_trigger_movement() {
        let rec = MouseRecorder::start();
        execute(&payload("move", Some(f64::NAN), Some(f64::INFINITY), None)).unwrap();
        assert!(rec.calls().is_empty());
    }

    #[test]
    fn is_allowed_requires_widget_on_enabled_page_with_control_option() {
        assert!(!is_allowed(&AgentConfig::default()));
        assert!(!is_allowed(&config(
            r#"{"pages":[{"enabled":false,"widgets":[{"type":"trackpad","options":{"control":true}}]}]}"#
        )));
        assert!(!is_allowed(&config(
            r#"{"pages":[{"enabled":true,"widgets":[{"type":"trackpad","options":{"control":false}}]}]}"#
        )));
        assert!(!is_allowed(&config(
            r#"{"pages":[{"enabled":true,"widgets":[{"type":"trackpad"}]}]}"#
        )));
        assert!(is_allowed(&config(
            r#"{"pages":[{"enabled":true,"widgets":[{"type":"trackpad","options":{"control":true}}]}]}"#
        )));
    }

    #[test]
    fn process_payload_honors_config_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        let bytes = br#"{"type":"move","dx":15,"dy":-20}"#;
        std::fs::write(
            &file,
            r#"{"pages":[{"enabled":true,"widgets":[{"type":"trackpad","options":{"control":false}}]}]}"#,
        )
        .unwrap();
        let rec = MouseRecorder::start();
        process_payload(bytes, &ConfigStore::new(file.clone()));
        assert!(rec.calls().is_empty());

        std::fs::write(
            &file,
            r#"{"pages":[{"enabled":true,"widgets":[{"type":"trackpad","options":{"control":true}}]}]}"#,
        )
        .unwrap();
        process_payload(bytes, &ConfigStore::new(file));
        assert_eq!(rec.calls(), [call("move", 15, -20, 0)]);
    }

    #[test]
    fn payloads_parse_as_the_web_json_defaults_do() {
        let p = |s: &str| TrackpadPayload::parse(s.as_bytes());
        assert_eq!(
            p(r#"{"TYPE":"move","Dx":"1.5","dy":-2,"other":[1,{"x":2}]}"#),
            Ok(Some(TrackpadPayload {
                r#type: Some("move".into()),
                dx: Some(1.5),
                dy: Some(-2.0),
                ..Default::default()
            }))
        );
        assert_eq!(p("null"), Ok(None));
        assert_eq!(
            p(r#"{"type":null,"action":"up","delta":null}"#),
            Ok(Some(TrackpadPayload {
                action: Some("up".into()),
                ..Default::default()
            }))
        );
        for bad in [
            "",
            "{",
            "[]",
            "5",
            r#"{"type":5}"#,
            r#"{"dx":true}"#,
            r#"{"dx":"NaN"}"#,
            r#"{"dx":"1 "}"#,
            r#"{"dx":"Infinity"}"#,
            r#"{"type":"move"} x"#,
            "{\"type\":\"move\" /* c */}",
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_held_button_is_released_once() {
        let rec = MouseRecorder::start();
        execute(&payload("down", None, None, None)).unwrap();
        release_held_button();
        release_held_button();
        execute(&payload("click", None, None, None)).unwrap();
        release_held_button();
        let actions: Vec<String> = rec.calls().into_iter().map(|c| c.action).collect();
        assert_eq!(actions, ["mouseDown", "mouseUp", "click"]);
    }
}
