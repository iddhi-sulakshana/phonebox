//! `AgentConfig.ToPublic`: the part of the settings sent to the phone. Commands (shortcut cmd/args/cwd, command widget
//! scripts), feed and calendar addresses, camera URLs and integration keys never leave this computer.

use serde_json::{Value, json};

use super::{AgentConfig, Shortcut, with_public};
use crate::stack_widgets::public_options;

/// A shortcut's picture: its revision (None when it has none), and whether the user uploaded it.
pub type IconLookup<'a> = &'a dyn Fn(&Shortcut) -> (Option<i64>, bool);

pub fn to_public(
    c: &AgentConfig,
    device_id: Option<&str>,
    shortcut_icon: Option<IconLookup>,
) -> Value {
    with_public(|| {
        let to = |v: &dyn erased::Ser| v.value();
        json!({
            "weather": to(&c.weather),
            "codexBar": to(&c.codex_bar),
            "night": to(&c.night),
            "display": to(&c.display),
            "pages": c.pages_for(device_id).iter().map(|p| json!({
                "id": p.id, "title": p.title, "icon": p.icon, "enabled": p.enabled, "idleSec": p.idle_sec,
                "appearance": p.appearance.as_ref().map(|a| to(a)),
                "widgets": p.widgets.iter().flatten().map(|w| json!({
                    "id": w.id, "type": w.r#type, "x": w.x, "y": w.y, "w": w.w, "h": w.h,
                    "appearance": w.appearance.as_ref().map(|a| to(a)),
                    // command widgets: only what the panel shows; the scripts stay on this computer
                    "options": public_options(w),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            // cameras: only how often the Camera widget may ask for a picture
            "cameras": c.cameras.sources.iter().map(|s| json!({ "id": s.id, "fps": s.fps })).collect::<Vec<_>>(),
            "shortcuts": c.shortcuts.iter().map(|s| {
                let (rev, upload) = shortcut_icon.map(|f| f(s)).unwrap_or((None, false));
                json!({ "id": s.id, "label": s.label, "icon": s.icon, "color": s.color, "toggle": s.toggle || s.cmd == "builtin:mute",
                        "iconRev": rev, "iconUpload": upload })
            }).collect::<Vec<_>>(),
            // names only (for the Rules widget); what a rule matches on stays here
            "rules": c.rules.iter().map(|r| json!({ "id": r.id, "name": r.name, "enabled": r.enabled, "when": r.when })).collect::<Vec<_>>(),
            "charge": { "enabled": c.charge.enabled, "min": c.charge.min, "max": c.charge.max },
        })
    })
}

mod erased {
    /// Any settings section, serialized to a JSON value under the phone's rules.
    pub trait Ser {
        fn value(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn value(&self) -> serde_json::Value {
            serde_json::to_value(self).expect("settings serialize")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{normalize::normalize, read::parse};

    fn load(json: &str) -> AgentConfig {
        normalize(parse(json).unwrap(), None)
    }

    #[test]
    fn the_phone_never_sees_commands() {
        let c = load(
            r#"{ "pages": [ { "id": "p", "title": "P", "widgets": [ { "type": "command", "x": 0, "y": 0, "w": 3, "h": 2, "options": { "command": "secret-script.ps1", "commandUnix": "secret-unix.sh", "future": "secret-x", "title": "T" } } ] } ],
                 "shortcuts": [ { "id": "s", "label": "S", "cmd": "secret.exe", "args": "--token abc" } ] }"#,
        );
        let text = to_public(&c, None, None).to_string();
        assert!(!text.contains("secret"));
        assert!(!text.contains("--token"));
        assert!(text.contains("\"T\""));
    }

    #[test]
    fn the_phone_learns_which_shortcut_pictures_are_uploads() {
        let c = load(
            r#"{ "shortcuts": [ { "id": "up", "label": "Up", "cmd": "a.exe" }, { "id": "own", "label": "Own", "cmd": "b.exe" } ] }"#,
        );
        let lookup = |s: &Shortcut| (Some(7), s.id == "up");
        let public = to_public(&c, None, Some(&lookup));
        let by_id = |id: &str| {
            public["shortcuts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id("up")["iconUpload"], true);
        assert_eq!(by_id("own")["iconUpload"], false);
        assert_eq!(by_id("own")["iconRev"], 7);
        assert!(
            to_public(&c, None, None)["shortcuts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["iconUpload"] == false)
        );
    }

    #[test]
    fn nulls_are_written_and_unknown_fields_stay_home() {
        let c = load(r#"{ "weather": { "secretKnob": "x" } }"#);
        let public = to_public(&c, None, None);
        assert!(
            public["display"]
                .get("homePage")
                .is_some_and(Value::is_null)
        );
        assert!(public["weather"].get("secretKnob").is_none());
        assert!(
            public["pages"][0]
                .get("idleSec")
                .is_some_and(Value::is_null)
        );
    }
}
