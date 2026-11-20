//! Port of the settings model in `Config.cs` (and the config classes kept beside their features: `AppearanceConfig.cs`,
//! `ControlDeck.Models.cs`, `AppConsole.cs`, `CameraFeeds.cs`, `PhotoFrame.cs`).
//!
//! Fields are declared in the C# order so `config.json` is written in the same order. Every struct keeps the fields it
//! does not know in `extra` and writes them back (a deliberate departure: the C# agent drops them).

pub mod normalize;
pub mod read;

pub mod write;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Fields this model does not know, kept as read and written back after the known ones.
pub type Extra = Map<String, Value>;

/// The update source of this fork. The C# agent's default, `ersinkoc/phonebox`, is rewritten to it on load.
pub const FORK_REPO: &str = "iddhi-sulakshana/phonebox";
pub const UPSTREAM_REPO: &str = "ersinkoc/phonebox";

macro_rules! defaults {
    ($ty:ident { $($field:ident : $value:expr),* $(,)? }) => {
        impl Default for $ty {
            fn default() -> Self {
                $ty { $($field: $value,)* extra: Extra::new() }
            }
        }
    };
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    pub codex_bar: CodexBarConfig,
    pub controls: ControlDeckConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adb_path: Option<String>,
    pub weather: WeatherConfig,
    pub ping: PingConfig,
    pub night: NightConfig,
    pub display: DisplayConfig,
    pub lan: LanConfig,
    pub updates: UpdateConfig,
    pub claude: ClaudeConfig,
    pub cursor: CursorConfig,
    pub pages: Vec<PageConfig>,
    pub shortcuts: Vec<Shortcut>,
    pub rules: Vec<RuleConfig>,
    pub charge: ChargeConfig,
    pub phones: Vec<PhoneConfig>,
    pub calendars: Vec<CalendarSource>,
    pub feeds: Vec<FeedSource>,
    pub photos: PhotoFrameConfig,
    pub cameras: CameraConfig,
    pub home_assistant: HomeAssistantConfig,
    pub obs: ObsConfig,
    pub discord: DiscordConfig,
    pub console: ConsoleConfig,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(AgentConfig {
    codex_bar: CodexBarConfig::default(),
    controls: ControlDeckConfig::default(),
    device_serial: None,
    adb_path: None,
    weather: WeatherConfig::default(),
    ping: PingConfig::default(),
    night: NightConfig::default(),
    display: DisplayConfig::default(),
    lan: LanConfig::default(),
    updates: UpdateConfig::default(),
    claude: ClaudeConfig::default(),
    cursor: CursorConfig::default(),
    pages: vec![],
    shortcuts: vec![],
    rules: vec![],
    charge: ChargeConfig::default(),
    phones: vec![],
    calendars: vec![],
    feeds: vec![],
    photos: PhotoFrameConfig::default(),
    cameras: CameraConfig::default(),
    home_assistant: HomeAssistantConfig::default(),
    obs: ObsConfig::default(),
    discord: DiscordConfig::default(),
    console: ConsoleConfig::default(),
});

impl AgentConfig {
    /// The pages a phone shows: its own selection (Settings › Phones), or all of them.
    pub fn pages_for(&self, device_id: Option<&str>) -> Vec<&PageConfig> {
        let ids = self
            .phones
            .iter()
            .find(|p| Some(p.id.as_str()) == device_id)
            .and_then(|p| p.pages.as_ref());
        match ids {
            Some(ids) if !ids.is_empty() => {
                let chosen: Vec<&PageConfig> =
                    self.pages.iter().filter(|p| ids.contains(&p.id)).collect();
                if chosen.is_empty() {
                    self.pages.iter().collect()
                } else {
                    chosen
                }
            }
            _ => self.pages.iter().collect(),
        }
    }

    /// Every widget (of enabled pages only, if asked), with the enabled children of Smart Stacks as widgets of their own.
    pub fn widgets(&self, enabled_pages_only: bool) -> Vec<WidgetConfig> {
        let mut out = Vec::new();
        for page in self
            .pages
            .iter()
            .filter(|p| !enabled_pages_only || p.enabled)
        {
            for w in page.widgets.iter().flatten() {
                out.push(w.clone());
                for child in crate::stack_widgets::children(w)
                    .into_iter()
                    .filter(|c| c.enabled)
                {
                    out.push(WidgetConfig {
                        r#type: child.r#type,
                        options: child.options,
                        ..WidgetConfig::default()
                    });
                }
            }
        }
        out
    }

    /// Ending processes is allowed when an enabled page has a processes widget that allows it.
    pub fn allow_kill(&self) -> bool {
        self.widgets(true)
            .iter()
            .any(|w| w.r#type == "processes" && w.bool("allowKill") != Some(false))
    }

    /// True when an enabled page (including enabled Smart Stack children) shows the quota cost overview card.
    pub fn show_quota_overview(&self) -> bool {
        self.widgets(true).iter().any(|w| {
            (w.r#type == "codexbar" || w.r#type == "quotas") && w.bool("showOverview") == Some(true)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CodexBarConfig {
    pub providers: Vec<String>,
    pub rotate: bool,
    pub page_size: i32,
    pub rotate_interval: i32,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(CodexBarConfig {
    providers: vec!["claude".into(), "codex".into(), "antigravity".into()],
    rotate: true,
    page_size: 2,
    rotate_interval: 8,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ControlDeckConfig {
    pub allow_listening: bool,
    pub allow_power: bool,
    pub allow_online_radio_logos: bool,
    pub actions: Vec<DeckAction>,
    pub profiles: Vec<DeckProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mixer_executable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mixer_library: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_refresh_token: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ControlDeckConfig {
    allow_listening: false,
    allow_power: false,
    allow_online_radio_logos: false,
    actions: vec![],
    profiles: vec![],
    spotify_token: None,
    spotify_device: None,
    mixer_executable: None,
    mixer_library: None,
    spotify_client_id: None,
    spotify_refresh_token: None,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeckAction {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
    pub value: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(DeckAction {
    id: String::new(),
    label: String::new(),
    kind: "shortcut".into(),
    target: String::new(),
    argument: None,
    value: 0,
    device: None,
    icon: None,
    enabled: true,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeckProfile {
    pub id: String,
    pub label: String,
    pub levels: Vec<DeckLevel>,
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub defaults: IndexMap<String, String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(DeckProfile {
    id: String::new(),
    label: String::new(),
    levels: vec![],
    actions: vec![],
    output: None,
    input: None,
    defaults: IndexMap::new(),
});

/// `record DeckLevel(string Kind, string Id, int Volume, bool Muted)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeckLevel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub volume: i32,
    pub muted: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(DeckLevel {
    kind: None,
    id: None,
    volume: 0,
    muted: false
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WeatherConfig {
    pub city: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(
        deserialize_with = "de_weather_unit",
        serialize_with = "ser_weather_unit"
    )]
    pub unit: String,
    #[serde(
        deserialize_with = "de_weather_model",
        serialize_with = "ser_weather_model"
    )]
    pub model: String,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(WeatherConfig {
    city: "Tallinn".into(),
    lat: 59.437,
    lon: 24.745,
    unit: "celsius".into(),
    model: String::new()
});

/// The Open-Meteo models the settings offer (`WeatherModelJsonConverter.Models`).
pub const WEATHER_MODELS: [&str; 8] = [
    "gfs_seamless",
    "ecmwf_ifs025",
    "icon_seamless",
    "ukmo_seamless",
    "gem_seamless",
    "jma_seamless",
    "meteofrance_seamless",
    "cma_grapes_global",
];

/// weather.unit: "fahrenheit" in any case or padding, anything else "celsius".
pub fn normalize_weather_unit(value: Option<&str>) -> String {
    if value.is_some_and(|v| v.trim().eq_ignore_ascii_case("fahrenheit")) {
        "fahrenheit"
    } else {
        "celsius"
    }
    .to_string()
}

/// weather.model: one of [`WEATHER_MODELS`] (any case, padding), anything else "" (Open-Meteo's best match).
pub fn normalize_weather_model(value: Option<&str>) -> String {
    let v = value.map(str::trim).unwrap_or_default();
    WEATHER_MODELS
        .iter()
        .find(|m| m.eq_ignore_ascii_case(v))
        .map(|m| m.to_string())
        .unwrap_or_default()
}

/// display.hour12: "true" (also "12h"), "auto", anything else "false".
pub fn normalize_hour12(value: Option<&str>) -> String {
    match value.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("true") | Some("12h") => "true",
        Some("auto") => "auto",
        _ => "false",
    }
    .to_string()
}

fn de_weather_unit<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    // a non-string token is skipped, not an error: a hand-edited number must not make the whole file unreadable
    Ok(normalize_weather_unit(Value::deserialize(d)?.as_str()))
}
fn ser_weather_unit<S: Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&normalize_weather_unit(Some(v)))
}
fn de_weather_model<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(normalize_weather_model(Value::deserialize(d)?.as_str()))
}
fn ser_weather_model<S: Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&normalize_weather_model(Some(v)))
}
fn de_hour12<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(true) => "true".into(),
        Value::String(s) => normalize_hour12(Some(&s)),
        _ => "false".into(),
    })
}
fn ser_hour12<S: Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
    match v {
        "true" => s.serialize_bool(true),
        "auto" => s.serialize_str("auto"),
        _ => s.serialize_bool(false),
    }
}
/// `IconJsonConverter.Read`: a string, or null for a JSON null.
fn de_icon<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub adapters: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
/// `PingMonitor.DefaultHost`.
pub const DEFAULT_PING_HOST: &str = "1.1.1.1";
defaults!(PingConfig {
    host: Some(DEFAULT_PING_HOST.into()),
    adapters: vec![]
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct NightConfig {
    pub enabled: bool,
    pub start: String,
    pub end: String,
    pub brightness: f64,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(NightConfig {
    enabled: true,
    start: "00:30".into(),
    end: "07:30".into(),
    brightness: 0.02
});

/// The codes the panel and the settings page are translated into (i18n.js).
pub const LANGUAGES: [&str; 14] = [
    "auto", "en", "tr", "de", "fr", "es", "it", "pt", "nl", "pl", "ru", "el", "vi", "zh",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DisplayConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appearance: Option<AppearanceConfig>,
    pub language: String,
    pub show_seconds: bool,
    #[serde(deserialize_with = "de_hour12", serialize_with = "ser_hour12")]
    pub hour12: String,
    pub idle_return_sec: i32,
    pub vibrate: bool,
    pub finish_toast: bool,
    pub volume_keys: bool,
    pub orientation: String,
    pub theme: String,
    pub accent: String,
    pub art_tint: bool,
    pub auto_brightness: bool,
    pub night_when_dark: bool,
    pub proximity_wake: bool,
    pub pixel_shift: bool,
    pub controls_top: bool,
    pub idle_dim_min: i32,
    pub pin: bool,
    pub home_screen: bool,
    pub keep_awake: bool,
    pub follow_computer_lock: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_page: Option<String>,
    pub switch_countdown_sec: i32,
    pub touch_grace_sec: i32,
    pub min_dwell_sec: i32,
    pub hold_max_min: i32,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(DisplayConfig {
    appearance: None,
    language: "auto".into(),
    show_seconds: true,
    hour12: "false".into(),
    idle_return_sec: 90,
    vibrate: true,
    finish_toast: true,
    volume_keys: true,
    orientation: "landscape".into(),
    theme: "default".into(),
    accent: "#2dd4bf".into(),
    art_tint: true,
    auto_brightness: false,
    night_when_dark: false,
    proximity_wake: true,
    pixel_shift: false,
    controls_top: false,
    idle_dim_min: 0,
    pin: false,
    home_screen: false,
    keep_awake: true,
    follow_computer_lock: false,
    home_page: None,
    switch_countdown_sec: 3,
    touch_grace_sec: 20,
    min_dwell_sec: 15,
    hold_max_min: 30,
});

/// Optional appearance overrides. Null means inherit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AppearanceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_scale: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_padding: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(AppearanceConfig {
    style: None,
    font: None,
    font_scale: None,
    radius: None,
    gap: None,
    padding: None,
    page_padding: None,
    border: None,
    shadow: None,
});

impl AppearanceConfig {
    /// `AppearanceConfig.Normalize`.
    pub fn normalize(&mut self, widget: bool) {
        let bound = |v: Option<i32>, max: i32| v.map(|n| n.clamp(0, max));
        if !matches!(
            self.style.as_deref(),
            Some("default" | "cards" | "flat" | "borderless" | "seamless")
        ) {
            self.style = None;
        }
        if !matches!(
            self.font.as_deref(),
            Some("default" | "sans" | "rounded" | "condensed" | "serif" | "mono")
        ) {
            self.font = None;
        }
        self.font_scale = self.font_scale.map(|s| s.clamp(50, 200));
        self.radius = bound(self.radius, 32);
        self.gap = if widget { None } else { bound(self.gap, 24) };
        self.padding = bound(self.padding, 24);
        self.page_padding = if widget {
            None
        } else {
            bound(self.page_padding, 32)
        };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LanConfig {
    pub enabled: bool,
    pub port: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_token: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(LanConfig {
    enabled: false,
    port: 8766,
    token: None,
    webhook_token: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateConfig {
    pub auto: bool,
    pub repo: String,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(UpdateConfig {
    auto: true,
    repo: FORK_REPO.into()
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeConfig {
    pub phone_approvals: bool,
    pub timeout_sec: i32,
    pub share_conversation: bool,
    pub only_when_away: bool,
    pub share_session_titles: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ClaudeConfig {
    phone_approvals: true,
    timeout_sec: 90,
    share_conversation: false,
    only_when_away: false,
    share_session_titles: false,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CursorConfig {
    pub sessions: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(CursorConfig { sessions: false });

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PageConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appearance: Option<AppearanceConfig>,
    pub id: String,
    pub title: String,
    #[serde(deserialize_with = "de_icon", serialize_with = "write::ser_icon")]
    pub icon: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_sec: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub widgets: Option<Vec<WidgetConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(PageConfig {
    appearance: None,
    id: String::new(),
    title: String::new(),
    icon: String::new(),
    enabled: true,
    idle_sec: None,
    widgets: None,
    template: None,
    r#type: None,
    options: None,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WidgetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appearance: Option<AppearanceConfig>,
    pub id: String,
    pub r#type: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(WidgetConfig {
    appearance: None,
    id: String::new(),
    r#type: String::new(),
    x: 0,
    y: 0,
    w: 1,
    h: 1,
    options: None
});

impl WidgetConfig {
    pub fn opt(&self, key: &str) -> Option<&Value> {
        self.options.as_ref().and_then(|o| o.get(key))
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.opt(key).and_then(Value::as_str)
    }
    /// A number option as an int (`(int)double`), or None.
    pub fn int(&self, key: &str) -> Option<i32> {
        self.opt(key).and_then(Value::as_f64).map(|d| d as i32)
    }
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.opt(key).and_then(Value::as_bool)
    }
    pub fn set(&mut self, key: &str, value: Value) {
        self.options
            .get_or_insert_with(Map::new)
            .insert(key.to_string(), value);
    }
}

/// An automation rule: when something happens on the PC, do something on the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct RuleConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub when: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#match: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    pub then: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_value: Option<String>,
    pub while_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    pub priority: i32,
    pub for_sec: i32,
    pub cooldown_min: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub during: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countdown_sec: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(RuleConfig {
    id: String::new(),
    name: String::new(),
    enabled: true,
    when: "session-waiting".into(),
    r#match: None,
    value: None,
    then: "goto".into(),
    page: None,
    text: None,
    level: None,
    key: None,
    push_value: None,
    while_active: true,
    shortcut: None,
    priority: 5,
    for_sec: 0,
    cooldown_min: 0,
    days: None,
    during: None,
    countdown_sec: None,
    op: None,
    arg: None,
});

pub const RULE_TRIGGERS: [&str; 19] = [
    "session-waiting",
    "session-busy",
    "media-playing",
    "process-running",
    "app-in-front",
    "mic-in-use",
    "camera-in-use",
    "obs-live",
    "calendar-event",
    "cpu-above",
    "idle",
    "time",
    "push",
    "quota-above",
    "review-requested",
    "ci-failed",
    "battery-low",
    "ping-high",
    "always",
];
pub const RULE_ACTIONS: [&str; 4] = ["goto", "notify", "push", "shortcut"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ChargeConfig {
    pub enabled: bool,
    pub min: i32,
    pub max: i32,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_shortcut: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_shortcut: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ChargeConfig {
    enabled: false,
    min: 50,
    max: 80,
    method: "ha".into(),
    entity: None,
    on_shortcut: None,
    off_shortcut: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ObsConfig {
    pub enabled: bool,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ObsConfig {
    enabled: false,
    url: "ws://127.0.0.1:4455".into(),
    password: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DiscordConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(DiscordConfig {
    enabled: false,
    client_id: None,
    client_secret: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct HomeAssistantConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(HomeAssistantConfig {
    url: None,
    token: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PhoneConfig {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(PhoneConfig {
    id: String::new(),
    name: String::new(),
    pages: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct FeedSource {
    pub name: String,
    pub url: String,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(FeedSource {
    name: String::new(),
    url: String::new()
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CalendarSource {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(CalendarSource {
    name: String::new(),
    url: String::new(),
    color: None
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Shortcut {
    pub id: String,
    pub label: String,
    #[serde(deserialize_with = "de_icon", serialize_with = "write::ser_icon")]
    pub icon: String,
    pub color: String,
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
    pub toggle: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_args: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(Shortcut {
    id: String::new(),
    label: String::new(),
    icon: String::new(),
    color: "#6b7280".into(),
    cmd: String::new(),
    args: None,
    cwd: None,
    steps: None,
    toggle: false,
    state_key: None,
    off_cmd: None,
    off_args: None,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PhotoFrameConfig {
    pub folder: String,
    pub subfolders: bool,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(PhotoFrameConfig {
    folder: String::new(),
    subfolders: true
});

/// `CameraFeeds.MaxCameras`, `DefaultFps`, `MaxFps`.
pub const MAX_CAMERAS: usize = 8;
pub const CAMERA_DEFAULT_FPS: i32 = 5;
pub const CAMERA_MAX_FPS: i32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CameraConfig {
    pub ffmpeg: String,
    pub sources: Vec<CameraSource>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(CameraConfig {
    ffmpeg: String::new(),
    sources: vec![]
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CameraSource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub transport: String,
    pub fps: i32,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(CameraSource {
    id: String::new(),
    name: String::new(),
    url: String::new(),
    transport: "tcp".into(),
    fps: CAMERA_DEFAULT_FPS
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsoleConfig {
    pub profiles: Vec<ConsoleProfile>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ConsoleConfig { profiles: vec![] });

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsoleProfile {
    pub id: String,
    pub label: String,
    pub apps: String,
    pub keys: Vec<ConsoleKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_press: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ConsoleProfile {
    id: String::new(),
    label: String::new(),
    apps: String::new(),
    keys: vec![],
    dial_left: None,
    dial_right: None,
    dial_press: None,
});

impl ConsoleProfile {
    pub const MAX_KEYS: usize = 12;

    /// The app names, without a trailing `.exe`.
    pub fn app_names(&self) -> Vec<String> {
        self.apps
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| {
                if a.len() >= 4 && a[a.len() - 4..].eq_ignore_ascii_case(".exe") {
                    &a[..a.len() - 4]
                } else {
                    a
                }
            })
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsoleKey {
    pub label: String,
    pub keys: String,
    #[serde(flatten)]
    pub extra: Extra,
}
defaults!(ConsoleKey {
    label: String::new(),
    keys: String::new()
});

/// Every property name of the model, for reading keys in any case (`PropertyNameCaseInsensitive`).
pub(crate) const KNOWN_FIELDS: &[&str] = &[
    "codexBar",
    "controls",
    "deviceSerial",
    "adbPath",
    "weather",
    "ping",
    "night",
    "display",
    "lan",
    "updates",
    "claude",
    "cursor",
    "pages",
    "shortcuts",
    "rules",
    "charge",
    "phones",
    "calendars",
    "feeds",
    "photos",
    "cameras",
    "homeAssistant",
    "obs",
    "discord",
    "console",
    "providers",
    "rotate",
    "pageSize",
    "rotateInterval",
    "allowListening",
    "allowPower",
    "allowOnlineRadioLogos",
    "actions",
    "profiles",
    "spotifyToken",
    "spotifyDevice",
    "mixerExecutable",
    "mixerLibrary",
    "spotifyClientId",
    "spotifyRefreshToken",
    "id",
    "label",
    "kind",
    "target",
    "argument",
    "value",
    "device",
    "icon",
    "enabled",
    "levels",
    "output",
    "input",
    "defaults",
    "volume",
    "muted",
    "city",
    "lat",
    "lon",
    "unit",
    "model",
    "host",
    "adapters",
    "start",
    "end",
    "brightness",
    "appearance",
    "language",
    "showSeconds",
    "hour12",
    "idleReturnSec",
    "vibrate",
    "finishToast",
    "volumeKeys",
    "orientation",
    "theme",
    "accent",
    "artTint",
    "autoBrightness",
    "nightWhenDark",
    "proximityWake",
    "pixelShift",
    "controlsTop",
    "idleDimMin",
    "pin",
    "homeScreen",
    "keepAwake",
    "followComputerLock",
    "homePage",
    "switchCountdownSec",
    "touchGraceSec",
    "minDwellSec",
    "holdMaxMin",
    "style",
    "font",
    "fontScale",
    "radius",
    "gap",
    "padding",
    "pagePadding",
    "border",
    "shadow",
    "port",
    "token",
    "webhookToken",
    "auto",
    "repo",
    "phoneApprovals",
    "timeoutSec",
    "shareConversation",
    "onlyWhenAway",
    "shareSessionTitles",
    "sessions",
    "title",
    "idleSec",
    "widgets",
    "template",
    "type",
    "options",
    "x",
    "y",
    "w",
    "h",
    "name",
    "when",
    "match",
    "then",
    "page",
    "text",
    "level",
    "key",
    "pushValue",
    "whileActive",
    "shortcut",
    "priority",
    "forSec",
    "cooldownMin",
    "days",
    "during",
    "countdownSec",
    "op",
    "arg",
    "min",
    "max",
    "method",
    "entity",
    "onShortcut",
    "offShortcut",
    "url",
    "password",
    "clientId",
    "clientSecret",
    "color",
    "cmd",
    "args",
    "cwd",
    "steps",
    "toggle",
    "stateKey",
    "offCmd",
    "offArgs",
    "folder",
    "subfolders",
    "ffmpeg",
    "sources",
    "transport",
    "fps",
    "apps",
    "keys",
    "dialLeft",
    "dialRight",
    "dialPress",
    "desc",
];

/// Free-form dictionaries: their keys are data, read as written.
pub(crate) const OPAQUE_FIELDS: &[&str] = &["options", "defaults"];

#[cfg(test)]
mod tests {
    use super::normalize::normalize;
    use super::read::parse;
    use super::*;

    /// `ConfigStore.Get` on a file holding `json`: read, then normalized.
    fn load(json: &str) -> AgentConfig {
        normalize(parse(json).unwrap(), None)
    }

    fn page_ids(c: &AgentConfig) -> Vec<&str> {
        c.pages.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn keeps_unknown_fields() {
        let cfg = parse(r#"{ "futureFlag": 7, "weather": { "city": "Colombo", "newKnob": "x" } }"#)
            .unwrap();
        let out = serde_json::to_value(&cfg).unwrap();
        assert_eq!(out["futureFlag"], 7);
        assert_eq!(out["weather"]["newKnob"], "x");
    }

    #[test]
    fn reads_comments_trailing_commas_and_any_case() {
        let cfg = parse("{ // c\n \"Weather\": { \"CITY\": \"Colombo\", }, }").unwrap();
        assert_eq!(cfg.weather.city, "Colombo");
        assert!(cfg.extra.is_empty());
    }

    #[test]
    fn upstream_update_repo_moves_to_fork() {
        let cfg = normalize(
            parse(r#"{ "updates": { "repo": "ersinkoc/phonebox" } }"#).unwrap(),
            None,
        );
        assert_eq!(cfg.updates.repo, "iddhi-sulakshana/phonebox");
        let other = normalize(
            parse(r#"{ "updates": { "repo": "someone/else" } }"#).unwrap(),
            None,
        );
        assert_eq!(other.updates.repo, "someone/else");
        assert_eq!(load("{}").updates.repo, "iddhi-sulakshana/phonebox");
    }

    #[test]
    fn empty_config_gets_the_default_pages_and_fresh_tokens() {
        let c = load("{}");
        assert!(!c.pages.is_empty());
        assert!(
            c.pages
                .iter()
                .all(|p| !p.widgets.as_ref().unwrap().is_empty())
        );
        let (t, w) = (
            c.lan.token.clone().unwrap(),
            c.lan.webhook_token.clone().unwrap(),
        );
        assert!(!t.is_empty() && !w.is_empty());
        assert_ne!(t, w);
    }

    #[test]
    fn weather_unit_defaults_to_celsius_and_reaches_the_phone() {
        for (json, expected) in [
            (
                r#"{"weather":{"city":"Tallinn","lat":59.437,"lon":24.745}}"#,
                "celsius",
            ),
            (r#"{"weather":{"unit":"fahrenheit"}}"#, "fahrenheit"),
            (r#"{"weather":{"unit":" Fahrenheit "}}"#, "fahrenheit"),
            (r#"{"weather":{"unit":"kelvin"}}"#, "celsius"),
            (r#"{"weather":{"unit":null}}"#, "celsius"),
            (r#"{"weather":{"city":"Oslo","unit":123}}"#, "celsius"),
            (r#"{"weather":{"city":"Oslo","unit":true}}"#, "celsius"),
            (
                r#"{"weather":{"city":"Oslo","unit":{"deep":[1]}}}"#,
                "celsius",
            ),
        ] {
            let c = load(json);
            assert_eq!(c.weather.unit, expected, "{json}");
            if json.contains("Oslo") {
                assert_eq!(c.weather.city, "Oslo");
            }
            assert_eq!(serde_json::to_value(&c.weather).unwrap()["unit"], expected);
        }
    }

    #[test]
    fn weather_model_is_one_of_the_offered_models_or_best_match() {
        for (json, expected) in [
            (
                r#"{"weather":{"city":"Tallinn","lat":59.437,"lon":24.745}}"#,
                "",
            ),
            (r#"{"weather":{"model":"gfs_seamless"}}"#, "gfs_seamless"),
            (r#"{"weather":{"model":" GFS_Seamless "}}"#, "gfs_seamless"),
            (r#"{"weather":{"model":"ecmwf_ifs025"}}"#, "ecmwf_ifs025"),
            (r#"{"weather":{"model":"gfs_seamless&daily=x"}}"#, ""),
            (r#"{"weather":{"model":"accuweather"}}"#, ""),
            (r#"{"weather":{"model":null}}"#, ""),
            (r#"{"weather":{"city":"Oslo","model":42}}"#, ""),
            (
                r#"{"weather":{"city":"Oslo","model":["gfs_seamless"]}}"#,
                "",
            ),
        ] {
            let c = load(json);
            assert_eq!(c.weather.model, expected, "{json}");
            if json.contains("Oslo") {
                assert_eq!(c.weather.city, "Oslo");
            }
            assert_eq!(serde_json::to_value(&c.weather).unwrap()["model"], expected);
        }
    }

    #[test]
    fn burn_in_protection_is_off_by_default_and_the_dim_time_is_clamped() {
        let defaults = load("{}").display;
        assert!(!defaults.pixel_shift);
        assert_eq!(defaults.idle_dim_min, 0);
        assert_eq!(
            load(r#"{ "display": { "pixelShift": true, "idleDimMin": 99999 } }"#)
                .display
                .idle_dim_min,
            1440
        );
        assert_eq!(
            load(r#"{ "display": { "idleDimMin": -5 } }"#)
                .display
                .idle_dim_min,
            0
        );
        assert!(
            load(r#"{ "display": { "pixelShift": true } }"#)
                .display
                .pixel_shift
        );
    }

    #[test]
    fn templates_expand_into_widgets() {
        let c = load(r#"{ "pages": [ { "template": "music" }, { "template": "music" } ] }"#);
        assert_eq!(page_ids(&c), ["music", "music-2"]);
        assert!(
            c.pages[0]
                .widgets
                .as_ref()
                .unwrap()
                .iter()
                .any(|w| w.r#type == "media")
        );
        assert!(c.pages[0].template.is_none());
    }

    #[test]
    fn a_new_template_cannot_take_an_existing_pages_id() {
        let c = load(
            r#"{ "pages": [ { "template": "music" }, { "id": "music", "title": "Existing music page" } ] }"#,
        );
        assert_eq!(page_ids(&c), ["music-2", "music"]);
    }

    #[test]
    fn a_new_widget_cannot_take_an_existing_widgets_persistence_key() {
        let c = load(
            r#"{ "pages": [{ "id": "desk", "widgets": [ { "type": "timer" }, { "type": "timer", "id": "timer" } ] }] }"#,
        );
        let ids: Vec<&str> = c.pages[0]
            .widgets
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, ["timer-2", "timer"]);
    }

    #[test]
    fn turkish_capital_dotted_i_keeps_its_letter_in_ids() {
        let c = load(
            r#"{ "pages": [ { "title": "İzmir", "icon": "x", "widgets": [ { "type": "clock" } ] },
                            { "title": "İş Akışı", "icon": "x", "widgets": [ { "type": "clock" } ] } ],
                 "shortcuts": [ { "label": "İndir", "cmd": "notepad" } ],
                 "rules": [ { "name": "İkindi", "when": "always", "then": "notify" } ] }"#,
        );
        assert_eq!(page_ids(&c), ["izmir", "is-akisi"]);
        assert_eq!(c.shortcuts[0].id, "indir");
        assert_eq!(c.rules[0].id, "ikindi");
    }

    #[test]
    fn legacy_home_page_keeps_its_tiles_and_options() {
        let c = load(
            r#"{ "pages": [ { "type": "home", "title": "Start", "options": { "tiles": ["cpu", "ram"], "media": false, "weather": false } } ] }"#,
        );
        let p = &c.pages[0];
        assert_eq!(c.pages.len(), 1);
        assert_eq!(p.title, "Start");
        let ws = p.widgets.as_ref().unwrap();
        assert!(!ws.iter().any(|w| w.r#type == "media"));
        let stats: Vec<&WidgetConfig> = ws.iter().filter(|w| w.r#type == "stat").collect();
        assert_eq!(
            stats
                .iter()
                .map(|w| w.str("metric").unwrap())
                .collect::<Vec<_>>(),
            ["cpu", "ram"]
        );
        assert_eq!(stats.iter().map(|w| w.w).sum::<i32>(), 12);
        let clock = ws.iter().find(|w| w.r#type == "clock").unwrap();
        assert_eq!(clock.bool("weather"), Some(false));
        assert_eq!(clock.w, 12);
    }

    #[test]
    fn legacy_process_page_keeps_the_kill_setting() {
        let c = load(r#"{ "pages": [ { "type": "procs", "options": { "allowKill": false } } ] }"#);
        let w = c.pages[0]
            .widgets
            .as_ref()
            .unwrap()
            .iter()
            .find(|w| w.r#type == "processes")
            .unwrap()
            .clone();
        assert_eq!(w.bool("allowKill"), Some(false));
    }

    #[test]
    fn widgets_stay_on_the_grid_and_get_unique_ids() {
        let c = load(
            r#"{ "pages": [ { "id": "p", "title": "P", "widgets": [
                { "type": "clock", "x": 10, "y": 5, "w": 20, "h": 9 },
                { "type": "note", "id": "n", "x": -3, "y": 0, "w": 2, "h": 2 },
                { "type": "note", "id": "n", "x": 0, "y": 0, "w": 2, "h": 2 },
                { "type": "no-such-widget", "x": 0, "y": 0, "w": 2, "h": 2 } ] } ] }"#,
        );
        let ws = c.pages[0].widgets.as_ref().unwrap();
        assert_eq!(ws.len(), 3);
        assert!(ws[0].x + ws[0].w <= 12 && ws[0].y + ws[0].h <= 6);
        assert_eq!(ws[1].x, 0);
        assert_eq!(
            ws.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["clock", "n", "n-2"]
        );
    }

    #[test]
    fn at_least_one_page_stays_enabled() {
        let c = load(
            r#"{ "pages": [ { "template": "clock", "enabled": false }, { "template": "music", "enabled": false } ] }"#,
        );
        assert!(c.pages[0].enabled);
    }

    #[test]
    fn rules_are_cleaned_up() {
        let c = load(
            r#"{ "rules": [ { "when": "explode", "then": "goto" }, { "name": "Away", "when": "idle", "value": 99999, "then": "notify", "level": "weird" }, { "name": "Away", "when": "idle", "then": "goto" } ] }"#,
        );
        assert_eq!(
            c.rules.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["away", "away-2"]
        );
        assert_eq!(c.rules[0].value, Some(1440.0));
        assert_eq!(c.rules[0].level.as_deref(), Some("info"));
    }

    #[test]
    fn legacy_home_keeps_string_tiles_and_skips_other_json_types() {
        for tiles in [
            r#"["cpu",42,"ram"]"#,
            r#"[false,"cpu",{},null,[],"ram"]"#,
            r#"["cpu","ram"]"#,
        ] {
            let c = load(&format!(
                r#"{{"pages":[{{"type":"home","options":{{"tiles":{tiles}}}}}]}}"#
            ));
            let metrics: Vec<&str> = c.pages[0]
                .widgets
                .as_ref()
                .unwrap()
                .iter()
                .filter(|w| w.r#type == "stat")
                .map(|w| w.str("metric").unwrap())
                .collect();
            assert_eq!(metrics, ["cpu", "ram"], "{tiles}");
        }
    }

    #[test]
    fn legacy_deck_ignores_columns_that_are_not_an_integer() {
        for columns in [
            r#""three""#,
            "null",
            "true",
            "{}",
            "[]",
            "2.5",
            "2147483648",
        ] {
            let c = load(&format!(
                r#"{{"pages":[{{"type":"deck","options":{{"columns":{columns}}}}}]}}"#
            ));
            let w = c.pages[0]
                .widgets
                .as_ref()
                .unwrap()
                .iter()
                .find(|w| w.r#type == "shortcuts")
                .unwrap()
                .clone();
            assert_eq!(w.int("columns"), Some(4), "{columns}");
        }
    }

    #[test]
    fn legacy_deck_preserves_numeric_columns() {
        let c = load(r#"{"pages":[{"type":"deck","options":{"columns":3}}]}"#);
        let w = c.pages[0]
            .widgets
            .as_ref()
            .unwrap()
            .iter()
            .find(|w| w.r#type == "shortcuts")
            .unwrap()
            .clone();
        assert_eq!(w.int("columns"), Some(3));
    }

    #[test]
    fn an_effectively_empty_page_list_restores_usable_defaults() {
        for pages in ["[]", "null", "[null]", "[null,null]"] {
            let c = load(&format!(r#"{{"pages":{pages}}}"#));
            assert_eq!(
                page_ids(&c),
                ["home", "ai", "work", "system", "procs", "deck"],
                "{pages}"
            );
            assert!(c.pages.iter().any(|p| p.enabled));
            assert!(
                c.pages
                    .iter()
                    .all(|p| !p.widgets.as_ref().unwrap().is_empty())
            );
        }
    }

    #[test]
    fn null_entries_do_not_replace_a_valid_custom_page() {
        let c = load(r#"{"pages":[null,{"id":"mine","widgets":[{"type":"clock"}]},null]}"#);
        assert_eq!(page_ids(&c), ["mine"]);
    }

    fn stack_config(children: &str) -> AgentConfig {
        parse(&r#"{"pages":[{"id":"stack","enabled":true,"widgets":[{"type":"stack","options":{"widgets":CHILDREN}}]}]}"#.replace("CHILDREN", children)).unwrap()
    }

    #[test]
    fn nested_commands_never_reach_the_phone() {
        // the ToPublic half of this C# test is ported with /api/config (server::routes_settings)
        let config = stack_config(
            r#"[{"type":"command","options":{"command":"PRIVATE SCRIPT"}},{"type":"clock"},{"type":"stack","options":{"widgets":[{"type":"command","options":{"command":"PRIVATE SCRIPT"}}]}}]"#,
        );
        let ws = config.widgets(false);
        assert_eq!(ws.iter().filter(|w| w.r#type == "clock").count(), 1);
        assert!(!ws.iter().any(|w| w.r#type == "command"));
    }

    #[test]
    fn child_permissions_follow_enabled_state_and_page_visibility() {
        assert!(
            !stack_config(r#"[{"type":"processes","enabled":false,"options":{"allowKill":true}}]"#)
                .allow_kill()
        );
        let mut config = stack_config(r#"[{"type":"processes","options":{"allowKill":true}}]"#);
        assert!(config.allow_kill());
        config.pages[0].enabled = false;
        assert!(!config.allow_kill());
    }

    #[test]
    fn codex_bar_settings_persist_and_stay_within_supported_ranges() {
        let c = load(
            r#"{"codexBar":{"providers":[],"pageSize":999,"rotateInterval":-1},"pages":[{"template":"codexbar"}]}"#,
        );
        assert!(c.codex_bar.providers.is_empty());
        assert_eq!(c.codex_bar.page_size, 6);
        assert_eq!(c.codex_bar.rotate_interval, 2);
        assert!(
            c.pages[0]
                .widgets
                .as_ref()
                .unwrap()
                .iter()
                .any(|w| w.r#type == "quotas")
        );
    }
}
