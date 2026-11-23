//! A fake growatt-server for the client's and the service's tests. The password "sun" signs `admin` in for the token
//! "t1", then "t2", …; the data routes answer 401 for a token in `refused`, or for every token once `refuse_all` is set.
//! Every request is recorded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::testutil::{FakeHttp, Reply, Req};

pub const STATUS: &str = r#"{"status":200,"message":"Device status data retrieved successfully","data":{
  "ppv1":"604","ppv2":"660","panelPower":"1264","vPv1":"185.5","vPv2":"198","iPv1":"3.4","iPv2":"3.4","iTotal":"6.8",
  "batPower":"-43","vBat":"54.38","capacity":"75","gridPower":"0","vAcInput":"232.2","fAcInput":"50.08",
  "loadPower":"1270","rateVA":"1389","loadPrecent":"23.1","vAcOutput":"230.1","fAcOutput":"50.13",
  "status":"12","invStatus":"","deviceType":"5","source":"grobro","updatedAt":"2026-11-23T10:00:00.000Z"}}"#;

pub const TOTALS: &str = r#"{"status":200,"message":"Device total data retrieved successfully","data":{"deviceType":"5",
  "epvToday":"5.2","epvTotal":"1234.5","eDischargeToday":"3.1","eDischargeTotal":"800","chargeToday":"2.4",
  "chargeTotal":"760.2","eToUserToday":"1.5","eToUserTotal":"300","useEnergyToday":"7.9","useEnergyTotal":"2100.7",
  "eToGridToday":"0","eToGridTotal":"0"}}"#;

const DENIED: &str = r#"{"status":401,"message":"Access denied: Invalid token"}"#;
const SETTINGS: &str = "/api/v1/growatt/inverter-settings/";

pub struct Server {
    pub fake: FakeHttp,
    pub seen: Arc<Mutex<Vec<Req>>>,
    pub refused: Arc<Mutex<Vec<String>>>,
    pub refuse_all: Arc<AtomicBool>,
    /// The inverter's settings by route name: ac-output-source 1, charge-source 0, max-charge-current 30.
    pub settings: Arc<Mutex<HashMap<String, i64>>>,
    /// When set, writing a setting answers 500 with this message.
    pub refuse_settings: Arc<Mutex<Option<String>>>,
    /// When set, device-status answers 500 "Growatt not connected".
    pub fail_status: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Switches {
    seen: Arc<Mutex<Vec<Req>>>,
    refused: Arc<Mutex<Vec<String>>>,
    refuse_all: Arc<AtomicBool>,
    settings: Arc<Mutex<HashMap<String, i64>>>,
    refuse_settings: Arc<Mutex<Option<String>>>,
    fail_status: Arc<AtomicBool>,
}

fn ok(message: &str, data: Value) -> Reply {
    Reply::json(
        200,
        json!({"status": 200, "message": message, "data": data}).to_string(),
    )
}

fn answer(s: &Switches, req: Req) -> Reply {
    let logins = {
        let mut seen = s.seen.lock().unwrap();
        seen.push(req.clone());
        seen.iter()
            .filter(|q| q.path == "/api/v1/auth/login")
            .count()
    };
    if (req.method.as_str(), req.path.as_str()) == ("POST", "/api/v1/auth/login") {
        let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
        return if body == json!({"username": "admin", "password": "sun"}) {
            ok(
                "Login successful",
                json!({"token": format!("t{logins}"), "username": "admin"}),
            )
        } else {
            Reply::json(
                401,
                r#"{"status":401,"message":"Invalid username or password"}"#,
            )
        };
    }
    if !req.path.starts_with("/api/v1/growatt/") {
        return Reply::json(404, r#"{"status":404,"message":"Not found"}"#);
    }
    let token = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "));
    let allowed = token.is_some_and(|t| {
        t.starts_with('t')
            && !s.refuse_all.load(Ordering::SeqCst)
            && !s.refused.lock().unwrap().iter().any(|x| x == t)
    });
    if !allowed {
        return Reply::json(401, DENIED);
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/v1/growatt/device-status") if s.fail_status.load(Ordering::SeqCst) => {
            Reply::json(500, r#"{"status":500,"message":"Growatt not connected"}"#)
        }
        ("GET", "/api/v1/growatt/device-status") => Reply::json(200, STATUS),
        ("GET", "/api/v1/growatt/total-data") => Reply::json(200, TOTALS),
        ("POST", "/api/v1/growatt/relogin") => ok(
            "Re-login successful",
            json!({"connected": true, "plantId": "1", "device": "storage-AB12"}),
        ),
        ("GET", path) if path.starts_with(SETTINGS) => {
            match s.settings.lock().unwrap().get(&path[SETTINGS.len()..]) {
                Some(v) => ok("Setting retrieved", json!({ "value": v })),
                None => Reply::json(404, r#"{"status":404,"message":"Not found"}"#),
            }
        }
        ("POST", path) if path.starts_with(SETTINGS) => {
            if let Some(m) = s.refuse_settings.lock().unwrap().clone() {
                return Reply::json(500, json!({"status": 500, "message": m}).to_string());
            }
            let value = serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|b| b.get("value").and_then(Value::as_i64));
            match value {
                Some(v) => {
                    s.settings
                        .lock()
                        .unwrap()
                        .insert(path[SETTINGS.len()..].to_string(), v);
                    ok(
                        "Setting applied",
                        json!({"success": true, "message": "Setting applied"}),
                    )
                }
                None => Reply::json(400, r#"{"status":400,"message":"Validation error"}"#),
            }
        }
        _ => Reply::json(404, r#"{"status":404,"message":"Not found"}"#),
    }
}

impl Server {
    pub async fn start() -> Server {
        let s = Switches {
            seen: Arc::default(),
            refused: Arc::default(),
            refuse_all: Arc::default(),
            settings: Arc::new(Mutex::new(HashMap::from([
                ("ac-output-source".to_string(), 1),
                ("charge-source".to_string(), 0),
                ("max-charge-current".to_string(), 30),
            ]))),
            refuse_settings: Arc::default(),
            fail_status: Arc::default(),
        };
        let switches = s.clone();
        let fake = FakeHttp::start(move |req: Req| {
            let s = switches.clone();
            async move { answer(&s, req) }
        })
        .await;
        Server {
            fake,
            seen: s.seen,
            refused: s.refused,
            refuse_all: s.refuse_all,
            settings: s.settings,
            refuse_settings: s.refuse_settings,
            fail_status: s.fail_status,
        }
    }

    pub fn client(&self, password: &str) -> super::client::GrowattClient {
        super::client::GrowattClient::new(&self.fake.url, "admin", password).unwrap()
    }

    pub fn seen(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }

    pub fn logins(&self) -> usize {
        self.seen()
            .iter()
            .filter(|q| q.path == "/api/v1/auth/login")
            .count()
    }

    /// Every request to `/growatt/…`.
    pub fn reads(&self) -> Vec<Req> {
        self.seen()
            .into_iter()
            .filter(|q| q.path.starts_with("/api/v1/growatt/"))
            .collect()
    }

    pub fn count(&self, method: &str, path: &str) -> usize {
        self.seen()
            .iter()
            .filter(|q| q.method == method && q.path == path)
            .count()
    }
}
