//! TCL: the owner's TCL air conditioners through TCL's cloud and AWS IoT, as the tcl-server dashboard reaches them.

pub mod actions;
pub mod client;
pub mod sigv4;

#[cfg(test)]
pub(crate) mod fake;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::config::TclConfig;
use crate::config::store::ConfigStore;
use client::{TclClient, TclDevice, TclEndpoints};

pub const NOT_CONFIGURED: &str = "Set up TCL in Settings › General";
pub const NO_WIDGET: &str = "Add a TCL widget to a page";
pub const NOT_ALLOWED: &str = "TCL actions are off in the widget's settings";
const CATEGORY: &str = "Tcl";
const LIST_FOR: Duration = Duration::from_secs(30);
const STATES_FOR: Duration = Duration::from_secs(4);

/// A unit for the widget; the state fields are None when the unit is offline or does not report them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TclUnit {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub power: Option<bool>,
    /// 0 auto, 1 cool, 2 dry, 3 fan, 4 heat.
    pub mode: Option<i64>,
    /// 0 auto … 7 turbo.
    pub fan: Option<i64>,
    /// 0 off, 1–3 the generator limits.
    pub generator: Option<i64>,
    pub target: Option<f64>,
    pub inside: Option<f64>,
    pub outside: Option<f64>,
}

impl TclUnit {
    fn from(device: &TclDevice, reported: Option<&Value>) -> TclUnit {
        let mut unit = TclUnit {
            id: device.id.clone(),
            name: device.name.clone(),
            online: device.online,
            ..TclUnit::default()
        };
        let Some(r) = reported.filter(|_| device.online) else {
            return unit;
        };
        let int = |k: &str| r.get(k).and_then(Value::as_f64).map(|n| n as i64);
        let num = |k: &str| r.get(k).and_then(Value::as_f64);
        unit.power = int("powerSwitch").map(|p| p == 1);
        unit.mode = int("workMode");
        unit.fan = int("windSpeed7Gear");
        unit.generator = int("generatorMode");
        unit.target = num("targetTemperature");
        unit.inside = num("currentTemperature");
        unit.outside = num("externalUnitTemperature");
        unit
    }
}

/// `GET /api/tcl`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TclView {
    pub configured: bool,
    pub error: Option<String>,
    /// A widget on an enabled page allows the actions.
    pub control: bool,
    pub units: Vec<TclUnit>,
}

#[derive(Default)]
struct Cache {
    /// The settings (`username\npassword`) these came with.
    key: String,
    devices: Vec<TclDevice>,
    devices_until: Option<Instant>,
    list_error: Option<String>,
    states: HashMap<String, Value>,
    states_until: Option<Instant>,
    states_error: Option<String>,
}

pub struct Tcl {
    cfg: Arc<ConfigStore>,
    endpoints: TclEndpoints,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The client for the current account (it keeps the session), by `username\npassword`.
    client: std::sync::Mutex<Option<(String, Arc<TclClient>)>>,
    /// Held across the reads, so requests that come together share them.
    cache: tokio::sync::Mutex<Cache>,
}

fn usable(t: Option<&TclConfig>) -> Option<&TclConfig> {
    t.filter(|t| !t.username.trim().is_empty() && !t.password.is_empty())
}

fn key_of(t: &TclConfig) -> String {
    format!("{}\n{}", t.username.trim(), t.password)
}

fn due(until: Option<Instant>) -> bool {
    until.is_none_or(|u| Instant::now() >= u)
}

impl Tcl {
    pub fn new(cfg: Arc<ConfigStore>) -> Tcl {
        Tcl::with_endpoints(
            cfg,
            TclEndpoints::tcl(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
    }

    /// With other hosts and clock (the tests' fake cloud).
    pub fn with_endpoints(
        cfg: Arc<ConfigStore>,
        endpoints: TclEndpoints,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Tcl {
        Tcl {
            cfg,
            endpoints,
            now_ms,
            client: std::sync::Mutex::new(None),
            cache: tokio::sync::Mutex::new(Cache::default()),
        }
    }

    fn client_for(&self, t: &TclConfig) -> Arc<TclClient> {
        let key = key_of(t);
        let mut slot = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, c)) = slot.as_ref()
            && *k == key
        {
            return c.clone();
        }
        let c = Arc::new(TclClient::new(
            &t.username,
            &t.password,
            self.endpoints.clone(),
            self.now_ms.clone(),
        ));
        *slot = Some((key, c.clone()));
        c
    }

    /// The device list from its cache or TCL (a failure keeps the last list).
    async fn refresh_list(&self, cache: &mut Cache, client: &TclClient) {
        if !due(cache.devices_until) {
            return;
        }
        match client.devices().await {
            Ok(list) => {
                cache.devices = list;
                cache.list_error = None;
                cache.devices_until = Some(Instant::now() + LIST_FOR);
            }
            Err(error) => {
                crate::log::debug(CATEGORY, &format!("tcl list: {error}"));
                cache.list_error = Some(error);
                cache.devices_until = Some(Instant::now() + STATES_FOR);
            }
        }
    }

    /// The account's units with their states, from the caches or TCL; an error keeps the last ones.
    pub async fn view(&self) -> TclView {
        let cfg = self.cfg.get();
        let Some(t) = usable(cfg.tcl.as_ref()) else {
            return TclView {
                error: Some(NOT_CONFIGURED.into()),
                ..TclView::default()
            };
        };
        let mut view = TclView {
            configured: true,
            control: cfg.widget_control("tcl"),
            ..TclView::default()
        };
        if !cfg.has_widget("tcl") {
            view.error = Some(NO_WIDGET.into());
            return view;
        }
        let client = self.client_for(t);
        let key = key_of(t);
        let mut cache = self.cache.lock().await;
        if cache.key != key {
            *cache = Cache {
                key,
                ..Cache::default()
            };
        }
        self.refresh_list(&mut cache, &client).await;
        if due(cache.states_until) {
            let online: Vec<String> = cache
                .devices
                .iter()
                .filter(|d| d.online)
                .map(|d| d.id.clone())
                .collect();
            let reads =
                futures_util::future::join_all(online.iter().map(|id| client.reported(id))).await;
            cache.states_error = None;
            for (id, read) in online.into_iter().zip(reads) {
                match read {
                    Ok(r) => {
                        cache.states.insert(id, r);
                    }
                    Err(error) => {
                        crate::log::debug(CATEGORY, &format!("tcl state of {id}: {error}"));
                        cache.states_error.get_or_insert(error);
                    }
                }
            }
            cache.states_until = Some(Instant::now() + STATES_FOR);
        }
        view.units = cache
            .devices
            .iter()
            .map(|d| TclUnit::from(d, cache.states.get(&d.id)))
            .collect();
        view.error = cache
            .list_error
            .clone()
            .or_else(|| cache.states_error.clone());
        view
    }

    /// Sends `action` with `value` to the unit `unit`, or to every online unit for `all`: how many publishes went
    /// out. Needs a widget that allows the actions.
    pub async fn act(&self, unit: &str, action: &str, value: &str) -> Result<usize, String> {
        let cfg = self.cfg.get();
        let t = usable(cfg.tcl.as_ref()).ok_or(NOT_CONFIGURED)?;
        if !cfg.widget_control("tcl") {
            return Err(NOT_ALLOWED.into());
        }
        let all = unit == "all";
        let properties = actions::properties(action, value, all)?;
        let client = self.client_for(t);
        let targets: Vec<TclDevice> = {
            let key = key_of(t);
            let mut cache = self.cache.lock().await;
            if cache.key != key {
                *cache = Cache {
                    key,
                    ..Cache::default()
                };
            }
            if cache.devices.is_empty() {
                self.refresh_list(&mut cache, &client).await;
            }
            if cache.devices.is_empty()
                && let Some(error) = &cache.list_error
            {
                return Err(error.clone());
            }
            if all {
                cache.devices.iter().filter(|d| d.online).cloned().collect()
            } else {
                let d = cache
                    .devices
                    .iter()
                    .find(|d| d.id == unit)
                    .ok_or("There is no such unit")?;
                if !d.online {
                    return Err(format!("{} is offline", d.name));
                }
                vec![d.clone()]
            }
        };
        if targets.is_empty() {
            return Err("No unit is online".into());
        }
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|d| client.publish(&d.id, properties.clone())),
        )
        .await;
        self.cache.lock().await.states_until = None;
        let sent = results.iter().filter(|r| r.is_ok()).count();
        crate::log::info(
            CATEGORY,
            &format!(
                "tcl: {action} {value} -> {sent} of {} unit(s)",
                targets.len()
            ),
        );
        match results.into_iter().find_map(Result::err) {
            None => Ok(sent),
            Some(error) if sent == 0 => Err(error),
            Some(error) => Err(format!(
                "{sent} of {} units took it; the others: {error}",
                targets.len()
            )),
        }
    }

    /// Settings › Test with the (maybe unsaved) account: "Connected · N units", or why not.
    pub async fn test(&self, username: &str, password: &str) -> String {
        if username.trim().is_empty() || password.is_empty() {
            return "Enter the TCL account and password".into();
        }
        let client = TclClient::new(
            username,
            password,
            self.endpoints.clone(),
            self.now_ms.clone(),
        );
        match client.devices().await {
            Ok(list) if list.len() == 1 => "Connected · 1 unit".into(),
            Ok(list) => format!("Connected · {} units", list.len()),
            Err(error) => error,
        }
    }

    #[cfg(test)]
    async fn expire(&self) {
        let mut cache = self.cache.lock().await;
        cache.devices_until = None;
        cache.states_until = None;
    }

    #[cfg(test)]
    async fn expire_states(&self) {
        self.cache.lock().await.states_until = None;
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
