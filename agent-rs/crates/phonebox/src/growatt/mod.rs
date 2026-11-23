//! Growatt: the owner's growatt-server (the shop inverter) for the `growatt` widget. No loop: every read comes from a
//! request (the phone's widget, Settings › Test), only while an enabled page has the widget, with caches (the live
//! status 2 s, the energy totals 5 minutes, the inverter settings 10 minutes). Actions need a widget that allows them.

pub mod client;

#[cfg(test)]
pub(crate) mod fake;

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::GrowattConfig;
use crate::config::store::ConfigStore;
use client::{DeviceStatus, GrowattClient, TotalData};

pub const NOT_CONFIGURED: &str = "Set up Growatt in Settings › General";
pub const NO_WIDGET: &str = "Add a Growatt widget to a page";
pub const NOT_ALLOWED: &str = "Growatt actions are off in the widget's settings";
const CATEGORY: &str = "Growatt";
const DEFAULT_USERNAME: &str = "admin";
const STATUS_FOR: Duration = Duration::from_secs(2);
const TOTALS_FOR: Duration = Duration::from_secs(300);
const SETTINGS_FOR: Duration = Duration::from_secs(600);
/// A failed read of the totals or the settings is tried again after this.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// The settings the widget offers, in the order they are read.
const SETTINGS: [&str; 3] = ["ac-output-source", "charge-source", "max-charge-current"];

/// `GET /api/growatt`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowattView {
    pub configured: bool,
    pub error: Option<String>,
    /// A widget on an enabled page allows the actions.
    pub control: bool,
    /// The server's host (and port), for the widget's footer.
    pub host: String,
    pub status: Option<DeviceStatus>,
    pub totals: Option<TotalData>,
}

/// `GET /api/growatt/settings`: the inverter's settings, None when not read (yet).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowattSettings {
    pub error: Option<String>,
    pub ac_output_source: Option<i64>,
    pub charge_source: Option<i64>,
    pub max_charge_current: Option<i64>,
}

/// The values the server accepts for a setting, by its route name.
pub fn setting_range(name: &str) -> Option<RangeInclusive<i64>> {
    match name {
        "ac-output-source" => Some(0..=3),
        "charge-source" => Some(0..=2),
        "max-charge-current" => Some(0..=100),
        _ => None,
    }
}

#[derive(Default)]
struct Live {
    /// The settings (`url\nusername\npassword`) these readings came with.
    key: String,
    status: Option<DeviceStatus>,
    status_until: Option<Instant>,
    status_error: Option<String>,
    totals: Option<TotalData>,
    totals_until: Option<Instant>,
    totals_error: Option<String>,
}

#[derive(Default)]
struct Settings {
    key: String,
    values: HashMap<&'static str, i64>,
    until: Option<Instant>,
    error: Option<String>,
}

pub struct Growatt {
    cfg: Arc<ConfigStore>,
    /// The client for the current settings (it keeps its token), by `url\nusername\npassword`.
    client: std::sync::Mutex<Option<(String, Arc<GrowattClient>)>>,
    /// Held across the reads, so requests that come together share one.
    live: tokio::sync::Mutex<Live>,
    /// Apart from `live`: the settings come through Growatt's cloud, slowly.
    settings: tokio::sync::Mutex<Settings>,
}

/// The settings the service may use: an address and a password.
fn usable(g: Option<&GrowattConfig>) -> Option<&GrowattConfig> {
    g.filter(|g| !g.url.trim().is_empty() && !g.password.is_empty())
}

fn username(g: &GrowattConfig) -> &str {
    match g.username.trim() {
        "" => DEFAULT_USERNAME,
        name => name,
    }
}

fn key_of(g: &GrowattConfig) -> String {
    format!("{}\n{}\n{}", g.url, username(g), g.password)
}

/// `growatt.lan`, `192.168.1.157:8082`: the address without scheme or path.
fn host_of(url: &str) -> String {
    client::api_base(url)
        .and_then(|b| reqwest::Url::parse(&b).ok())
        .map(|u| match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

fn due(until: Option<Instant>) -> bool {
    until.is_none_or(|u| Instant::now() >= u)
}

impl Growatt {
    pub fn new(cfg: Arc<ConfigStore>) -> Growatt {
        Growatt {
            cfg,
            client: std::sync::Mutex::new(None),
            live: tokio::sync::Mutex::new(Live::default()),
            settings: tokio::sync::Mutex::new(Settings::default()),
        }
    }

    /// The client for `g`, made again when the address, username or password changed (a token is never sent to
    /// another server or kept for another password).
    fn client_for(&self, g: &GrowattConfig) -> Result<Arc<GrowattClient>, String> {
        let key = key_of(g);
        let mut slot = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, c)) = slot.as_ref()
            && *k == key
        {
            return Ok(c.clone());
        }
        let c = Arc::new(GrowattClient::new(&g.url, username(g), &g.password)?);
        *slot = Some((key, c.clone()));
        Ok(c)
    }

    /// The live readings and the totals, each from its cache or the server; an error keeps the last ones.
    pub async fn view(&self) -> GrowattView {
        let cfg = self.cfg.get();
        let Some(g) = usable(cfg.growatt.as_ref()) else {
            return GrowattView {
                error: Some(NOT_CONFIGURED.into()),
                ..GrowattView::default()
            };
        };
        let mut view = GrowattView {
            configured: true,
            control: cfg.widget_control("growatt"),
            host: host_of(&g.url),
            ..GrowattView::default()
        };
        if !cfg.has_widget("growatt") {
            view.error = Some(NO_WIDGET.into());
            return view;
        }
        let client = match self.client_for(g) {
            Ok(c) => c,
            Err(error) => {
                view.error = Some(error);
                return view;
            }
        };
        let key = key_of(g);
        let mut live = self.live.lock().await;
        if live.key != key {
            *live = Live {
                key,
                ..Live::default()
            };
        }
        if due(live.status_until) {
            match client.device_status().await {
                Ok(s) => {
                    live.status = Some(s);
                    live.status_error = None;
                }
                Err(error) => {
                    crate::log::debug(CATEGORY, &format!("growatt status: {error}"));
                    live.status_error = Some(error);
                }
            }
            live.status_until = Some(Instant::now() + STATUS_FOR);
        }
        if due(live.totals_until) {
            let (totals, wait) = match client.total_data().await {
                Ok(t) => (Ok(t), TOTALS_FOR),
                Err(error) => (Err(error), RETRY_AFTER),
            };
            match totals {
                Ok(t) => {
                    live.totals = Some(t);
                    live.totals_error = None;
                }
                Err(error) => {
                    crate::log::debug(CATEGORY, &format!("growatt totals: {error}"));
                    live.totals_error = Some(error);
                }
            }
            live.totals_until = Some(Instant::now() + wait);
        }
        view.status = live.status.clone();
        view.totals = live.totals.clone();
        view.error = live
            .status_error
            .clone()
            .or_else(|| live.totals_error.clone());
        view
    }

    /// The inverter's settings, read once and kept 10 minutes (a write updates them).
    pub async fn settings(&self) -> GrowattSettings {
        let cfg = self.cfg.get();
        let Some(g) = usable(cfg.growatt.as_ref()) else {
            return GrowattSettings {
                error: Some(NOT_CONFIGURED.into()),
                ..GrowattSettings::default()
            };
        };
        if !cfg.has_widget("growatt") {
            return GrowattSettings {
                error: Some(NO_WIDGET.into()),
                ..GrowattSettings::default()
            };
        }
        let client = match self.client_for(g) {
            Ok(c) => c,
            Err(error) => {
                return GrowattSettings {
                    error: Some(error),
                    ..GrowattSettings::default()
                };
            }
        };
        let key = key_of(g);
        let mut st = self.settings.lock().await;
        if st.key != key {
            *st = Settings {
                key,
                ..Settings::default()
            };
        }
        if due(st.until) {
            st.error = None;
            for name in SETTINGS {
                match client.setting(name).await {
                    Ok(v) => {
                        st.values.insert(name, v);
                    }
                    Err(error) => {
                        crate::log::debug(CATEGORY, &format!("growatt {name}: {error}"));
                        st.error = Some(error);
                        break;
                    }
                }
            }
            st.until = Some(
                Instant::now()
                    + if st.error.is_some() {
                        RETRY_AFTER
                    } else {
                        SETTINGS_FOR
                    },
            );
        }
        GrowattSettings {
            error: st.error.clone(),
            ac_output_source: st.values.get("ac-output-source").copied(),
            charge_source: st.values.get("charge-source").copied(),
            max_charge_current: st.values.get("max-charge-current").copied(),
        }
    }

    /// The settings and a client when a widget on an enabled page allows the actions.
    fn for_action(&self) -> Result<(GrowattConfig, Arc<GrowattClient>), String> {
        let cfg = self.cfg.get();
        let g = usable(cfg.growatt.as_ref()).ok_or(NOT_CONFIGURED)?;
        if !cfg.widget_control("growatt") {
            return Err(NOT_ALLOWED.into());
        }
        Ok((g.clone(), self.client_for(g)?))
    }

    /// Changes an inverter setting: `name` one of the three, `value` a whole number in its range.
    pub async fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let (g, client) = self.for_action()?;
        let (name, range) = SETTINGS
            .iter()
            .find(|n| **n == name)
            .and_then(|n| Some((*n, setting_range(n)?)))
            .ok_or("There is no such Growatt setting")?;
        let value: i64 = value
            .trim()
            .parse()
            .map_err(|_| "The value must be a whole number".to_string())?;
        if !range.contains(&value) {
            return Err(format!(
                "The value must be {} to {}",
                range.start(),
                range.end()
            ));
        }
        client.set_setting(name, value).await?;
        crate::log::info(CATEGORY, &format!("growatt: {name} set to {value}"));
        let mut st = self.settings.lock().await;
        if st.key == key_of(&g) {
            st.values.insert(name, value);
        }
        Ok(())
    }

    /// Asks the server to sign in to Growatt's cloud again; the next view reads everything again.
    pub async fn relogin(&self) -> Result<bool, String> {
        let (_, client) = self.for_action()?;
        let connected = client.relogin().await?;
        crate::log::info(
            CATEGORY,
            &format!("growatt: relogin, connected {connected}"),
        );
        let mut live = self.live.lock().await;
        live.status_until = None;
        live.totals_until = None;
        Ok(connected)
    }

    /// Settings › Test with the (maybe unsaved) values: "Connected", or why not.
    pub async fn test(&self, url: &str, username: &str, password: &str) -> String {
        let name = match username.trim() {
            "" => DEFAULT_USERNAME,
            name => name,
        };
        let client = match GrowattClient::new(url, name, password) {
            Ok(c) => c,
            Err(error) => return error,
        };
        if let Err(error) = client.sign_in().await {
            return error;
        }
        match client.device_status().await {
            Ok(_) => "Connected".into(),
            Err(error) => format!("Signed in, but: {error}"),
        }
    }

    #[cfg(test)]
    async fn expire(&self) {
        self.expire_status().await;
        self.live.lock().await.totals_until = None;
        self.settings.lock().await.until = None;
    }

    #[cfg(test)]
    async fn expire_status(&self) {
        self.live.lock().await.status_until = None;
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
