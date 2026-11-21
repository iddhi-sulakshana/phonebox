//! `HomeAssistant.cs`: entity states for the `ha` widgets and switching them, through Home Assistant's REST API. No
//! loop: every read comes from a request, with a 3 s cache. Only entities listed by a widget on an enabled page are
//! read or switched, and the permission is checked again after every await.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, HomeAssistantConfig, WidgetConfig};

pub const NOT_CONFIGURED: &str = "Set up Home Assistant in Settings › General";
const NOT_ALLOWED: &str = "switching this entity is not allowed in the widget's settings";
const NO_ANSWER: &str = "Home Assistant did not answer";
const NO_STATE: &str = "Home Assistant does not report this entity's state";
const SETTINGS_CHANGED: &str = "Home Assistant settings changed; try again";
const MAX_ITEMS: usize = 200;
const CACHE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HaEntity {
    pub id: String,
    pub state: String,
    pub name: String,
    pub unit: Option<String>,
    pub device_class: Option<String>,
    pub icon: Option<String>,
    /// Unix ms; 0 when unknown.
    pub changed: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HaListing {
    pub id: String,
    pub name: String,
    pub switchable: bool,
}

/// `GET /api/ha/states`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HaStates {
    pub configured: bool,
    pub error: Option<String>,
    pub entities: Vec<HaEntity>,
}

/// `GET /api/ha/entities` (the settings page's picker).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HaEntities {
    pub configured: bool,
    pub error: Option<String>,
    pub entities: Vec<HaListing>,
}

/// `ItemsOf`: the entities a widget lists, each with whether it may be switched. A non-empty `items` array is final
/// (at most 200 usable rows); otherwise the legacy `entities` text.
pub fn items_of(w: &WidgetConfig) -> Vec<(String, bool)> {
    let widget_control = w.bool("control") == Some(true);
    if let Some(Value::Array(items)) = w.opt("items")
        && !items.is_empty()
    {
        // the cap counts usable rows, so a run of unusable ones cannot hide the valid ones after it
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for item in items {
            if seen.len() >= MAX_ITEMS {
                break;
            }
            let Some(Value::String(id)) = item.as_object().and_then(|o| o.get("id")) else {
                continue;
            };
            let id = id.trim();
            if !id.contains('.') || seen.iter().any(|s| s == id) {
                continue;
            }
            seen.push(id.to_string());
            let control = match item.get("control") {
                Some(Value::Bool(b)) => *b,
                _ => widget_control,
            };
            out.push((id.to_string(), control));
        }
        return out;
    }
    w.str("entities")
        .unwrap_or("")
        .split([',', '\n', ' ', ';'])
        .map(str::trim)
        .filter(|e| !e.is_empty() && e.contains('.'))
        .take(MAX_ITEMS)
        .map(|id| (id.to_string(), widget_control))
        .collect()
}

/// The `ha` widgets on enabled pages (Smart Stack cards included).
fn ha_widgets(cfg: &AgentConfig) -> Vec<WidgetConfig> {
    cfg.widgets(true)
        .into_iter()
        .filter(|w| w.r#type == "ha")
        .collect()
}

/// Some widget on an enabled page lets `id` be switched.
pub fn control_allowed(cfg: &AgentConfig, id: &str) -> bool {
    ha_widgets(cfg).iter().any(|w| {
        items_of(w)
            .iter()
            .any(|(item, control)| *control && item == id)
    })
}

/// Domains the picker offers as switchable.
pub fn switchable(domain: &str) -> bool {
    matches!(
        domain,
        "light"
            | "switch"
            | "fan"
            | "input_boolean"
            | "automation"
            | "siren"
            | "humidifier"
            | "group"
            | "cover"
            | "media_player"
            | "scene"
            | "script"
            | "button"
            | "input_button"
            | "lock"
            | "vacuum"
    )
}

#[derive(Default)]
struct Cache {
    states: HashMap<String, HaEntity>,
    at: Option<Instant>,
    /// Server, token and the widgets' entities the states were read for.
    key: String,
    /// `"{url}\n{token}"` the states came from.
    server: String,
    error: Option<String>,
}

pub struct HomeAssistant {
    cfg: Arc<ConfigStore>,
    http: reqwest::Client,
    /// Held across the HTTP read, as C#'s `SemaphoreSlim`.
    cache: tokio::sync::Mutex<Cache>,
    /// A submitted service call may have changed a state: the next read goes to Home Assistant.
    stale: AtomicBool,
}

impl HomeAssistant {
    pub fn new(cfg: Arc<ConfigStore>) -> HomeAssistant {
        HomeAssistant {
            cfg,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("the Home Assistant client"),
            cache: tokio::sync::Mutex::new(Cache::default()),
            stale: AtomicBool::new(false),
        }
    }

    /// The states of `ids` that a widget on an enabled page lists, in the order asked (an unknown one as "unknown").
    pub async fn states(&self, ids: &[String]) -> HaStates {
        // credentials and the visible entities come from one settings snapshot
        let cfg = self.cfg.get();
        if !configured(&cfg.home_assistant) {
            return HaStates {
                configured: false,
                error: Some(NOT_CONFIGURED.into()),
                entities: Vec::new(),
            };
        }
        let (error, states) = self.read_states(&cfg, ids, false).await;
        let entities = states
            .into_iter()
            .map(|(id, entity)| {
                entity.unwrap_or_else(|| HaEntity {
                    name: id.clone(),
                    id,
                    state: "unknown".into(),
                    unit: None,
                    device_class: None,
                    icon: None,
                    changed: 0,
                })
            })
            .collect();
        HaStates {
            configured: true,
            error,
            entities,
        }
    }

    /// The requested entities a widget in `cfg` lists, each with its state from the latest successful read for these
    /// settings (None when it did not include it), and the read error if any. The connection must be set.
    async fn read_states(
        &self,
        cfg: &AgentConfig,
        ids: &[String],
        fresh: bool,
    ) -> (Option<String>, Vec<(String, Option<HaEntity>)>) {
        let ha = &cfg.home_assistant;
        let allowed: HashSet<String> = ha_widgets(cfg)
            .iter()
            .flat_map(|w| items_of(w).into_iter().map(|(id, _)| id))
            .collect();
        let server = format!(
            "{}\n{}",
            ha.url.as_deref().unwrap_or("").trim_end_matches('/'),
            ha.token.as_deref().unwrap_or("")
        );
        let mut sorted: Vec<&str> = allowed.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        let key = format!("{server}\n{}", sorted.join(","));
        let mut cache = self.cache.lock().await;
        let stale = self.stale.swap(false, Ordering::SeqCst);
        if fresh || stale || cache.at.is_none_or(|at| at.elapsed() > CACHE) || cache.key != key {
            match self.fetch_states(ha, &allowed).await {
                Ok(states) => {
                    cache.states = states;
                    cache.server = server;
                    cache.error = None;
                }
                Err(error) => {
                    // states read from another server or token must not answer for these settings
                    if cache.server != server {
                        cache.states.clear();
                        cache.server = server;
                    }
                    crate::log::debug(CATEGORY, &format!("home assistant: {error}"));
                    cache.error = Some(error);
                }
            }
            cache.key = key;
            cache.at = Some(Instant::now()); // a failure is cached too
        }
        let list = ids
            .iter()
            .filter(|id| allowed.contains(*id))
            .map(|id| (id.clone(), cache.states.get(id).cloned()))
            .collect();
        (cache.error.clone(), list)
    }

    async fn fetch_states(
        &self,
        ha: &HomeAssistantConfig,
        allowed: &HashSet<String>,
    ) -> Result<HashMap<String, HaEntity>, String> {
        let res = self
            .request(reqwest::Method::GET, "/api/states", ha)
            .send()
            .await
            .map_err(|e| failure(&e))?;
        if !res.status().is_success() {
            return Err(answered_with_reason(res.status()));
        }
        let text = res.text().await.map_err(|e| failure(&e))?;
        let root: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut states = HashMap::new();
        for e in array(&root)? {
            let Some(id) = string(prop(e, "entity_id")?)? else {
                continue;
            };
            if !allowed.contains(id) {
                continue;
            }
            let attrs = e.get("attributes").and_then(Value::as_object);
            let attr = |n: &str| {
                attrs
                    .and_then(|a| a.get(n))
                    .and_then(Value::as_str)
                    .map(String::from)
            };
            let changed = match e.get("last_changed") {
                Some(v) => string(v)?.and_then(parse_date).unwrap_or(0),
                None => 0,
            };
            let state = string(prop(e, "state")?)?.unwrap_or("").to_string();
            states.insert(
                id.to_string(),
                HaEntity {
                    id: id.to_string(),
                    state,
                    name: attr("friendly_name").unwrap_or_else(|| id.to_string()),
                    unit: attr("unit_of_measurement"),
                    device_class: attr("device_class"),
                    icon: attr("icon"),
                    changed,
                },
            );
        }
        Ok(states)
    }

    /// `ha.Url + path` with the bearer token.
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        ha: &HomeAssistantConfig,
    ) -> reqwest::RequestBuilder {
        let url = format!(
            "{}{path}",
            ha.url.as_deref().unwrap_or("").trim_end_matches('/')
        );
        self.http
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, bearer(ha.token.as_deref()))
    }

    /// A service call for `id`; the cached states are dropped once it was sent (a lost answer may still have switched
    /// the entity). The status, or why it failed.
    async fn call(&self, ha: &HomeAssistantConfig, service: &str, id: &str) -> Result<u16, String> {
        let body =
            crate::config::write::to_dotnet_json_compact(&serde_json::json!({ "entity_id": id }));
        let sent = self
            .request(
                reqwest::Method::POST,
                &format!("/api/services/{service}"),
                ha,
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .body(body)
            .send()
            .await;
        self.stale.store(true, Ordering::SeqCst);
        sent.map(|res| res.status().as_u16())
            .map_err(|e| failure(&e))
    }

    /// Everything Home Assistant knows, for the settings page's picker (no widget filter).
    pub async fn list_entities(&self) -> HaEntities {
        let ha = self.cfg.get().home_assistant.clone();
        if !configured(&ha) {
            return HaEntities {
                configured: false,
                error: Some(NOT_CONFIGURED.into()),
                entities: Vec::new(),
            };
        }
        let (error, entities) = match self.listing(&ha).await {
            Ok(list) => (None, list),
            Err(error) => (Some(error), Vec::new()),
        };
        HaEntities {
            configured: true,
            error,
            entities,
        }
    }

    async fn listing(&self, ha: &HomeAssistantConfig) -> Result<Vec<HaListing>, String> {
        let res = self
            .request(reqwest::Method::GET, "/api/states", ha)
            .send()
            .await
            .map_err(|e| failure(&e))?;
        if !res.status().is_success() {
            return Err(answered_with_reason(res.status()));
        }
        let text = res.text().await.map_err(|e| failure(&e))?;
        let root: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut list = Vec::new();
        for e in array(&root)? {
            let Some(id) = string(prop(e, "entity_id")?)? else {
                continue;
            };
            let name = e
                .get("attributes")
                .and_then(Value::as_object)
                .and_then(|a| a.get("friendly_name"))
                .and_then(Value::as_str);
            list.push(HaListing {
                id: id.to_string(),
                name: name.unwrap_or(id).to_string(),
                switchable: switchable(id.split('.').next().unwrap_or("")),
            });
        }
        list.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(list)
    }

    /// Switches `id` with the service its domain uses; None on success, else why not.
    pub async fn toggle(&self, id: &str) -> Option<String> {
        // the permission and the connection come from one settings snapshot
        let cfg = self.cfg.get();
        if !control_allowed(&cfg, id) {
            return Some(NOT_ALLOWED.into());
        }
        let ha = &cfg.home_assistant;
        if !configured(ha) || !absolute_http(ha.url.as_deref()) {
            return Some(NOT_CONFIGURED.into());
        }
        let domain = id.split('.').next().unwrap_or("");
        let mut state = String::new();
        if matches!(domain, "lock" | "vacuum") {
            // The service depends on the current state, so it comes from a fresh successful read with the settings the
            // call goes to: a failed read or another server's cached state could unlock a door or start a vacuum.
            let (error, states) = self.read_states(&cfg, &[id.to_string()], true).await;
            if let Some(error) = error {
                return Some(error);
            }
            match states.into_iter().next().and_then(|(_, e)| e) {
                Some(e)
                    if !matches!(e.state.as_str(), "unavailable" | "unknown")
                        && !e.state.trim().is_empty() =>
                {
                    state = e.state;
                }
                _ => return Some(NO_STATE.into()),
            }
            // the read awaited Home Assistant: the widget may have been hidden or the connection replaced meanwhile
            let now = self.cfg.get();
            if !control_allowed(&now, id) {
                return Some(NOT_ALLOWED.into());
            }
            if now.home_assistant.url != ha.url || now.home_assistant.token != ha.token {
                return Some(SETTINGS_CHANGED.into());
            }
        }
        let service = match domain {
            "light" | "switch" | "fan" | "input_boolean" | "automation" | "siren"
            | "humidifier" | "group" => "homeassistant/toggle",
            "cover" => "cover/toggle",
            "media_player" => "media_player/media_play_pause",
            "scene" => "scene/turn_on",
            "script" => "script/turn_on",
            "button" => "button/press",
            "input_button" => "input_button/press",
            "lock" if state == "locked" => "lock/unlock",
            "lock" => "lock/lock",
            "vacuum" if state == "cleaning" => "vacuum/return_to_base",
            "vacuum" => "vacuum/start",
            _ => return Some(format!("{domain} entities cannot be switched")),
        };
        match self.call(ha, service, id).await {
            Ok(code) => {
                crate::log::info(
                    CATEGORY,
                    &format!(
                        "home assistant: {} {id} -> {code}",
                        service.replace('/', ".")
                    ),
                );
                (!(200..300).contains(&code)).then(|| format!("Home Assistant answered {code}"))
            }
            Err(error) => Some(error),
        }
    }

    /// `SwitchAsync` (the charge guard): turns `id` on or off with `settings`, no widget permission. Returns the error
    /// and whether the call was submitted (true as soon as it was sent, also when its answer was lost).
    pub async fn switch(
        &self,
        id: &str,
        on: bool,
        settings: &HomeAssistantConfig,
    ) -> (Option<String>, bool) {
        if !id.contains('.') {
            return (Some("not an entity id".into()), false);
        }
        if !absolute_http(settings.url.as_deref()) {
            return (
                Some("Home Assistant address must be an absolute HTTP or HTTPS URL".into()),
                false,
            );
        }
        let service = if on {
            "homeassistant/turn_on"
        } else {
            "homeassistant/turn_off"
        };
        match self.call(settings, service, id).await {
            Ok(code) => (
                (!(200..300).contains(&code)).then(|| format!("Home Assistant answered {code}")),
                true,
            ),
            Err(error) => (Some(error), true),
        }
    }

    /// Settings › Test: "Connected", "The token was rejected", "Home Assistant answered N" or why it failed.
    pub async fn test(&self, url: &str, token: &str) -> String {
        let url = format!("{}/api/", url.trim_end_matches('/'));
        match self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, bearer(Some(token)))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => "Connected".into(),
            Ok(res) if res.status() == reqwest::StatusCode::UNAUTHORIZED => {
                "The token was rejected".into()
            }
            Ok(res) => format!("Home Assistant answered {}", res.status().as_u16()),
            Err(e) if e.is_timeout() => "No answer (check the address)".into(),
            Err(e) => crate::net::error_text(&e),
        }
    }
}

const CATEGORY: &str = "HomeAssistant";

fn configured(ha: &HomeAssistantConfig) -> bool {
    let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    set(&ha.url) && set(&ha.token)
}

/// `Uri.TryCreate(url, Absolute)` with an http or https scheme.
fn absolute_http(url: Option<&str>) -> bool {
    url.and_then(|u| reqwest::Url::parse(u).ok())
        .is_some_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// `AuthenticationHeaderValue("Bearer", token)`: no token is just the scheme.
fn bearer(token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() => format!("Bearer {t}"),
        _ => "Bearer".into(),
    }
}

/// A request that did not get an answer: "Home Assistant did not answer" on a timeout, else what failed (never the
/// URL).
fn failure(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        NO_ANSWER.into()
    } else {
        crate::net::error_text(e)
    }
}

fn answered_with_reason(status: reqwest::StatusCode) -> String {
    format!(
        "Home Assistant answered {} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}

/// `JsonElement.ValueKind` as .NET names it in its messages.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

fn wrong_kind(want: &str, v: &Value) -> String {
    format!(
        "The requested operation requires an element of type '{want}', but the target element has type '{}'.",
        kind(v)
    )
}

/// `EnumerateArray`.
fn array(v: &Value) -> Result<&Vec<Value>, String> {
    v.as_array().ok_or_else(|| wrong_kind("Array", v))
}

/// `GetProperty`: a missing member fails the read, as in C#.
fn prop<'a>(v: &'a Value, name: &str) -> Result<&'a Value, String> {
    match v {
        Value::Object(o) => o
            .get(name)
            .ok_or_else(|| "The given key was not present in the dictionary.".to_string()),
        other => Err(wrong_kind("Object", other)),
    }
}

/// `GetString`: null is None, anything but a string fails the read.
fn string(v: &Value) -> Result<Option<&str>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        other => Err(wrong_kind("String", other)),
    }
}

/// `DateTimeOffset.TryParse` for Home Assistant's times: ISO 8601 with an offset, or without one as local time.
fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(t.timestamp_millis());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            use chrono::TimeZone;
            return chrono::Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|t| t.timestamp_millis());
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| {
            use chrono::TimeZone;
            chrono::Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
                .earliest()
                .map(|t| t.timestamp_millis())
        })
}

#[cfg(test)]
#[path = "home_assistant_tests.rs"]
mod tests;
