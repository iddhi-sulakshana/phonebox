//! A client for the owner's Growatt server (growatt-server, `/api/v1`): it signs in with the server's admin user for
//! a 30-day token, then reads the inverter's live status (`/growatt/device-status`), its energy totals
//! (`/growatt/total-data`) and settings (`/growatt/inverter-settings/…`), changes those settings and asks the server
//! to reconnect to Growatt's cloud (`/growatt/relogin`), with `Authorization: Bearer`. The token is kept for the client's life; a read refused with
//! 401 signs in again once. Errors never carry the address, the password or the token.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(15);
const NO_ANSWER: &str = "The Growatt server did not answer";
const BAD_LOGIN: &str = "The Growatt server refused the username or password";
const NO_TOKEN: &str = "The Growatt server's sign-in reply had no token";

/// The API root for an address as the owner types it: `http://growatt.lan`, `http://growatt.lan/api/v1/` or a
/// reverse-proxied path all give `…/api/v1` once. None unless it is an absolute http or https URL with a host.
pub fn api_base(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        return None;
    }
    Some(if url.ends_with("/api/v1") {
        url.to_string()
    } else {
        format!("{url}/api/v1")
    })
}

/// `GET /growatt/device-status`: the server sends every reading as text; a missing or unreadable one is None.
/// Watts, volts, hertz and percent. `battery_w` is positive while discharging and negative while charging.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub pv_w: Option<f64>,
    pub pv1_w: Option<f64>,
    pub pv2_w: Option<f64>,
    pub pv1_v: Option<f64>,
    pub pv2_v: Option<f64>,
    pub battery_w: Option<f64>,
    pub battery_v: Option<f64>,
    pub battery_pct: Option<f64>,
    pub grid_w: Option<f64>,
    pub grid_v: Option<f64>,
    pub grid_hz: Option<f64>,
    pub load_w: Option<f64>,
    pub load_va: Option<f64>,
    pub load_pct: Option<f64>,
    pub output_v: Option<f64>,
    pub output_hz: Option<f64>,
    /// The inverter's status code as sent ("12"), empty when absent.
    pub status: String,
    /// "grobro" (the live feed) or "cloud" (Growatt's portal), empty when absent.
    pub source: String,
    /// When the server got the reading, Unix ms; 0 when unknown.
    pub updated_at: i64,
}

impl DeviceStatus {
    /// From the reply's `data` object.
    pub fn from_data(data: &Value) -> DeviceStatus {
        let n = |key: &str| number(data.get(key));
        DeviceStatus {
            pv_w: n("panelPower"),
            pv1_w: n("ppv1"),
            pv2_w: n("ppv2"),
            pv1_v: n("vPv1"),
            pv2_v: n("vPv2"),
            battery_w: n("batPower"),
            battery_v: n("vBat"),
            battery_pct: n("capacity"),
            grid_w: n("gridPower"),
            grid_v: n("vAcInput"),
            grid_hz: n("fAcInput"),
            load_w: n("loadPower"),
            load_va: n("rateVA"),
            load_pct: n("loadPrecent"), // sic: the server's spelling
            output_v: n("vAcOutput"),
            output_hz: n("fAcOutput"),
            status: text(data.get("status")),
            source: text(data.get("source")),
            updated_at: data
                .get("updatedAt")
                .and_then(Value::as_str)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t.trim()).ok())
                .map_or(0, |t| t.timestamp_millis()),
        }
    }
}

/// kWh today and over the inverter's life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Energy {
    pub today: Option<f64>,
    pub total: Option<f64>,
}

/// `GET /growatt/total-data`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TotalData {
    pub solar: Energy,
    pub discharged: Energy,
    pub charged: Energy,
    /// From the grid.
    pub imported: Energy,
    pub load: Energy,
    /// To the grid.
    pub exported: Energy,
}

impl TotalData {
    /// From the reply's `data` object.
    pub fn from_data(data: &Value) -> TotalData {
        let pair = |prefix: &str| Energy {
            today: number(data.get(format!("{prefix}Today").as_str())),
            total: number(data.get(format!("{prefix}Total").as_str())),
        };
        TotalData {
            solar: pair("epv"),
            discharged: pair("eDischarge"),
            charged: pair("charge"),
            imported: pair("eToUser"),
            load: pair("useEnergy"),
            exported: pair("eToGrid"),
        }
    }
}

/// One Growatt server and its sign-in. No `Debug`: it holds the password and the token.
pub struct GrowattClient {
    base: String,
    username: String,
    password: String,
    http: reqwest::Client,
    /// Held while signing in, so reads that start together sign in once.
    token: tokio::sync::Mutex<Option<String>>,
}

impl GrowattClient {
    /// A client for the server at `url` (see [`api_base`]); nothing is sent until the first read.
    pub fn new(url: &str, username: &str, password: &str) -> Result<GrowattClient, String> {
        let base = api_base(url).ok_or("The Growatt address must be an http or https URL")?;
        Ok(GrowattClient {
            base,
            username: username.to_string(),
            password: password.to_string(),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .map_err(|e| e.to_string())?,
            token: tokio::sync::Mutex::new(None),
        })
    }

    /// The inverter's live readings.
    pub async fn device_status(&self) -> Result<DeviceStatus, String> {
        self.read("/growatt/device-status")
            .await
            .map(|data| DeviceStatus::from_data(&data))
    }

    /// Today's and lifetime energy.
    pub async fn total_data(&self) -> Result<TotalData, String> {
        self.read("/growatt/total-data")
            .await
            .map(|data| TotalData::from_data(&data))
    }

    /// An inverter setting's current value (`ac-output-source`, `charge-source`, `max-charge-current`), read through
    /// Growatt's cloud by the server, so slow.
    pub async fn setting(&self, name: &str) -> Result<i64, String> {
        let data = self
            .read(&format!("/growatt/inverter-settings/{name}"))
            .await?;
        data.get("value")
            .and_then(Value::as_i64)
            .ok_or_else(|| "The Growatt server's reply had no value".to_string())
    }

    /// Changes an inverter setting (the server checks the range again).
    pub async fn set_setting(&self, name: &str, value: i64) -> Result<(), String> {
        let body = json!({ "value": value }).to_string();
        let data = self
            .request(
                reqwest::Method::POST,
                &format!("/growatt/inverter-settings/{name}"),
                Some(body),
            )
            .await?;
        match data.get("success").and_then(Value::as_bool) {
            Some(false) => Err(data
                .get("message")
                .and_then(Value::as_str)
                .map(|m| format!("The Growatt server could not apply it: {m}"))
                .unwrap_or_else(|| "The Growatt server could not apply it".into())),
            _ => Ok(()),
        }
    }

    /// Asks the server to sign in to Growatt's cloud again: whether it is connected afterwards.
    pub async fn relogin(&self) -> Result<bool, String> {
        let data = self
            .request(reqwest::Method::POST, "/growatt/relogin", None)
            .await?;
        Ok(data
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// Signs in now (Settings › Test), replacing the kept token.
    pub async fn sign_in(&self) -> Result<(), String> {
        let mut slot = self.token.lock().await;
        *slot = None;
        *slot = Some(self.login().await?);
        Ok(())
    }

    async fn read(&self, path: &str) -> Result<Value, String> {
        self.request(reqwest::Method::GET, path, None).await
    }

    /// `path` with the token, and the reply's `data`. A kept token that is refused is dropped and the request sent
    /// once more with a new one (the same body); a token that was just issued is not retried.
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Value, String> {
        let (token, fresh) = self.token(None).await?;
        let mut reply = self.send(&method, path, &token, body.as_deref()).await?;
        if reply.0 == 401 && !fresh {
            let (token, _) = self.token(Some(&token)).await?;
            reply = self.send(&method, path, &token, body.as_deref()).await?;
        }
        data_of(reply)
    }

    /// The kept token, or a new one when there is none or the kept one is `refused`. True when it was just issued.
    async fn token(&self, refused: Option<&str>) -> Result<(String, bool), String> {
        let mut slot = self.token.lock().await;
        if let Some(t) = slot.as_deref()
            && Some(t) != refused
        {
            return Ok((t.to_string(), false));
        }
        *slot = None;
        let token = self.login().await?;
        *slot = Some(token.clone());
        Ok((token, true))
    }

    /// `POST /auth/login` with the username and password: the token, or why not.
    async fn login(&self) -> Result<String, String> {
        let body = json!({ "username": self.username, "password": self.password }).to_string();
        let res = self
            .http
            .post(format!("{}/auth/login", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| failure(&e))?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| failure(&e))?;
        if status == 401 {
            return Err(BAD_LOGIN.into());
        }
        let data = data_of((status, text))?;
        match data.get("token").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => Ok(t.to_string()),
            _ => Err(NO_TOKEN.into()),
        }
    }

    async fn send(
        &self,
        method: &reqwest::Method,
        path: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(body) = body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let res = req.send().await.map_err(|e| failure(&e))?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| failure(&e))?;
        Ok((status, text))
    }
}

/// The `data` of a `{status, message, data}` reply; for an error status, the server's `message` when it sent one.
fn data_of((status, text): (u16, String)) -> Result<Value, String> {
    let root: Option<Value> = serde_json::from_str(&text).ok();
    if !(200..300).contains(&status) {
        let message = root
            .as_ref()
            .and_then(|r| r.get("message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty());
        return Err(match message {
            Some(m) => format!("The Growatt server answered {status}: {m}"),
            None => {
                let reason = reqwest::StatusCode::from_u16(status)
                    .ok()
                    .and_then(|s| s.canonical_reason())
                    .unwrap_or("");
                format!("The Growatt server answered {status} {reason}")
                    .trim_end()
                    .to_string()
            }
        });
    }
    match root.and_then(|mut r| r.get_mut("data").map(Value::take)) {
        Some(data) if data.is_object() => Ok(data),
        _ => Err("The Growatt server's reply had no data".into()),
    }
}

/// A reading as the server sends it (text, sometimes a number); None when absent, blank or not a finite number.
fn number(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// A request without an answer: "did not answer" on a timeout, else what failed (never the URL).
fn failure(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        NO_ANSWER.into()
    } else {
        crate::net::error_text(e)
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
