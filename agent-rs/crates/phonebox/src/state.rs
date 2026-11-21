//! Port of `State()` (`Program.cs`): what the phone reads every second, from `/api/state` or the live stream.

use serde_json::{Map, Value, json};

use crate::config::store::ConfigStore;
use crate::push_store::PushStore;

/// One part of the state (the PC monitor, media, audio…): its JSON as of now.
pub trait StateSource: Send + Sync {
    fn snapshot(&self) -> Value;
}

impl<T: StateSource + ?Sized> StateSource for std::sync::Arc<T> {
    fn snapshot(&self) -> Value {
        (**self).snapshot()
    }
}

/// A part that always answers the same JSON.
pub struct Fixed(pub Value);

impl StateSource for Fixed {
    fn snapshot(&self) -> Value {
        self.0.clone()
    }
}

/// Every part of the state that comes from a monitor, plus the live stream's "work" frame.
pub struct StateSources {
    pub update: Box<dyn StateSource>,
    pub pc: Box<dyn StateSource>,
    pub media: Box<dyn StateSource>,
    pub audio: Box<dyn StateSource>,
    pub ping: Box<dyn StateSource>,
    pub rules: Box<dyn StateSource>,
    pub rules_paused: Box<dyn StateSource>,
    pub privacy: Box<dyn StateSource>,
    pub obs: Box<dyn StateSource>,
    pub toggles: Box<dyn StateSource>,
    pub work: Box<dyn StateSource>,
}

impl StateSources {
    /// `pc` from the PC monitor.
    pub fn with_pc(mut self, pc: Box<dyn StateSource>) -> StateSources {
        self.pc = pc;
        self
    }

    /// `audio` from the volume control.
    pub fn with_audio(mut self, audio: Box<dyn StateSource>) -> StateSources {
        self.audio = audio;
        self
    }

    /// `media` from the media monitor, through the control deck.
    pub fn with_media(mut self, media: Box<dyn StateSource>) -> StateSources {
        self.media = media;
        self
    }

    /// `toggles` from the shortcuts.
    pub fn with_toggles(mut self, toggles: Box<dyn StateSource>) -> StateSources {
        self.toggles = toggles;
        self
    }

    /// `obs` from the OBS monitor.
    pub fn with_obs(mut self, obs: Box<dyn StateSource>) -> StateSources {
        self.obs = obs;
        self
    }

    /// `work` (the live stream's frame) from the session and GitHub monitors.
    pub fn with_work(mut self, work: Box<dyn StateSource>) -> StateSources {
        self.work = work;
        self
    }

    /// `rules` and `rulesPaused` from the rule engine.
    pub fn with_rules(
        mut self,
        active: Box<dyn StateSource>,
        paused: Box<dyn StateSource>,
    ) -> StateSources {
        self.rules = active;
        self.rules_paused = paused;
        self
    }

    /// `privacy` from the privacy monitor.
    pub fn with_privacy(mut self, privacy: Box<dyn StateSource>) -> StateSources {
        self.privacy = privacy;
        self
    }

    /// `ping` from the ping monitor.
    pub fn with_ping(mut self, ping: Box<dyn StateSource>) -> StateSources {
        self.ping = ping;
        self
    }

    /// Before the monitors are ported: each part answers what the C# agent answers on a headless Linux machine before
    /// its monitors have data (`state_empty.json`, recorded from the C# agent with the parity fixture).
    pub fn phase1() -> StateSources {
        let empty: Value =
            serde_json::from_str(include_str!("state_empty.json")).expect("state_empty.json");
        let part = |key: &str| -> Box<dyn StateSource> { Box::new(Fixed(empty[key].clone())) };
        StateSources {
            update: part("update"),
            pc: part("pc"),
            media: part("media"),
            audio: part("audio"),
            ping: part("ping"),
            rules: part("rules"),
            rules_paused: part("rulesPaused"),
            // macOS: the C# PrivacyMonitor cannot detect mic and camera use without extra permissions and always
            // says "unknown"; that is its whole macOS behavior, so it is final here
            privacy: if cfg!(target_os = "macos") {
                Box::new(Fixed(
                    json!({"mic": null, "camera": null, "micApps": [], "cameraApps": []}),
                ))
            } else {
                part("privacy")
            },
            obs: part("obs"),
            toggles: part("toggles"),
            work: part("work"),
        }
    }
}

/// The state in `State()`'s key order; `ev` asks for the events after that number (none without it).
pub fn build_state(
    s: &StateSources,
    cfg: &ConfigStore,
    push: &PushStore,
    ev: Option<i64>,
) -> Value {
    let events = match ev {
        Some(since) => serde_json::to_value(push.since(since)).unwrap_or_else(|_| json!([])),
        None => json!([]),
    };
    let mut m = Map::new();
    m.insert("update".into(), s.update.snapshot());
    m.insert("ts".into(), json!(crate::server::now_ms()));
    m.insert("host".into(), json!(machine_name()));
    // as text: a JavaScript number would lose the last digits of the ticks
    m.insert("cfgVersion".into(), json!(cfg.version_text()));
    m.insert("pushVersion".into(), json!(push.version()));
    m.insert("evSeq".into(), json!(push.seq()));
    m.insert("events".into(), events);
    m.insert("pc".into(), s.pc.snapshot());
    m.insert("media".into(), s.media.snapshot());
    m.insert("audio".into(), s.audio.snapshot());
    m.insert("ping".into(), s.ping.snapshot());
    m.insert("rules".into(), s.rules.snapshot());
    m.insert("rulesPaused".into(), s.rules_paused.snapshot());
    m.insert("privacy".into(), s.privacy.snapshot());
    m.insert("obs".into(), s.obs.snapshot());
    m.insert("toggles".into(), s.toggles.snapshot());
    Value::Object(m)
}

/// .NET's `Environment.MachineName`: the host name up to its first dot (Unix), the NetBIOS name (Windows).
pub fn machine_name() -> String {
    machine_name_from(&raw_host_name())
}

pub fn machine_name_from(host: &str) -> String {
    host.split('.').next().unwrap_or_default().to_string()
}

#[cfg(unix)]
fn raw_host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length; gethostname writes at most that many bytes
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(windows)]
fn raw_host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::store::ConfigStore;
    use crate::push_store::PushStore;

    #[test]
    fn keys_in_csharp_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigStore::new(dir.path().join("config.json"));
        let push = PushStore::new(&dir.path().join("data.json"));
        let state = build_state(&StateSources::phase1(), &cfg, &push, None);
        let keys: Vec<&str> = state
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "update",
                "ts",
                "host",
                "cfgVersion",
                "pushVersion",
                "evSeq",
                "events",
                "pc",
                "media",
                "audio",
                "ping",
                "rules",
                "rulesPaused",
                "privacy",
                "obs",
                "toggles"
            ]
        );
        // nulls are written, and the version goes as text (a JavaScript number would lose its last digits)
        let text = crate::config::write::to_dotnet_json_compact(&state);
        assert!(text.starts_with(r#"{"update":null,"ts":"#), "{text}");
        assert_eq!(state["cfgVersion"], cfg.version_text());
        assert_eq!(state["events"], serde_json::json!([]));
        assert_eq!(state["evSeq"], push.seq());
        assert_eq!(state["pushVersion"], push.version());
    }

    #[test]
    fn events_after_the_given_number() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigStore::new(dir.path().join("config.json"));
        let push = PushStore::new(&dir.path().join("data.json"));
        let first = push.emit("notify", Some(serde_json::json!({"title": "a"})));
        push.emit("goto", Some(serde_json::json!("p2")));
        let state = build_state(&StateSources::phase1(), &cfg, &push, Some(first.seq));
        let events = state["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "goto");
        assert_eq!(state["evSeq"], first.seq + 1);
    }

    #[test]
    fn phase1_sources_answer_like_the_csharp_agent_before_its_monitors_have_data() {
        let s = StateSources::phase1();
        assert_eq!(s.pc.snapshot(), serde_json::Value::Null);
        assert_eq!(s.rules.snapshot(), serde_json::json!([]));
        assert_eq!(
            s.work.snapshot(),
            serde_json::json!({"sessions": [], "github": null})
        );
    }

    #[test]
    fn privacy_is_unknown_on_macos_and_off_elsewhere_until_the_monitor_reads_it() {
        // macOS: the C# PrivacyMonitor cannot detect mic and camera use without extra permissions and says "unknown"
        let (mic, camera) = if cfg!(target_os = "macos") {
            (serde_json::Value::Null, serde_json::Value::Null)
        } else {
            (serde_json::json!(false), serde_json::json!(false))
        };
        assert_eq!(
            StateSources::phase1().privacy.snapshot(),
            serde_json::json!({"mic": mic, "camera": camera, "micApps": [], "cameraApps": []})
        );
    }

    #[test]
    fn host_is_the_machine_name_cut_at_the_first_dot() {
        assert_eq!(machine_name_from("iddhi-mbp.local"), "iddhi-mbp");
        assert_eq!(machine_name_from("box"), "box");
        assert!(!machine_name().contains('.'));
        assert!(!machine_name().is_empty());
    }
}
