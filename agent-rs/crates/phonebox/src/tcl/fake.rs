//! A fake TCL cloud for the client's and the service's tests: one server playing TCL's account login
//! (`/account/login`), the cloud-URL lookup, the cloud host (`/v3/auth/refresh_tokens`), the device host
//! (`/dev/v3/user/get_things`), Cognito (`/cognito`) and AWS IoT (`/iot/…`). The account is `me@example.com` with the
//! password "pw"; tokens are numbered by how often they were issued ("base1", "saas1", "ST1"…). Every request is
//! recorded.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::client::{TclClient, TclEndpoints};
use super::sigv4::md5_hex;
use crate::testutil::{FakeHttp, Reply, Req};

/// 2026-11-23T10:00:00Z, the clock's start.
pub const T0: i64 = 1_795_428_000_000;
pub const ACCOUNT: &str = "me@example.com";
pub const PASSWORD: &str = "pw";

#[derive(Clone, Default)]
pub struct Switches {
    pub seen: Arc<Mutex<Vec<Req>>>,
    /// Base tokens the cloud-URL lookup no longer accepts.
    pub dead_bases: Arc<Mutex<Vec<String>>>,
    /// SaaS tokens the device list no longer accepts.
    pub dead_saas: Arc<Mutex<Vec<String>>>,
    /// The next this many AWS IoT calls answer 403.
    pub iot_403: Arc<AtomicUsize>,
    /// The region the cloud-URL lookup answers (default eu-central-1).
    pub region: Arc<Mutex<Option<String>>>,
    /// The units' reported state by id; ac3 is offline in the list.
    pub reported: Arc<Mutex<Map<String, Value>>>,
    /// When set, the device list answers code 1 for every token.
    pub list_fails: Arc<AtomicBool>,
}

pub struct Cloud {
    pub fake: FakeHttp,
    pub s: Switches,
    /// The client's clock (Unix ms).
    pub clock: Arc<AtomicI64>,
}

fn count(seen: &[Req], path: &str) -> usize {
    seen.iter().filter(|q| q.path == path).count()
}

fn body_of(req: &Req) -> Value {
    serde_json::from_str(&req.body).unwrap_or(Value::Null)
}

fn answer(s: &Switches, url: &str, req: Req) -> Reply {
    let seen = {
        let mut seen = s.seen.lock().unwrap();
        seen.push(req.clone());
        seen.clone()
    };
    let body = body_of(&req);
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/account/login") => {
            if body["username"] == ACCOUNT && body["password"] == md5_hex(PASSWORD).as_str() {
                let n = count(&seen, "/account/login");
                Reply::json(
                    200,
                    json!({"status": 1, "token": format!("base{n}"), "refreshToken": "r",
                        "user": {"username": "sso-user", "countryAbbr": "LK"}})
                    .to_string(),
                )
            } else {
                Reply::json(200, r#"{"status":6,"msg":"password error"}"#)
            }
        }
        ("POST", "/v3/global/cloud_url_get") => {
            let token = body["ssoToken"].as_str().unwrap_or("");
            if !token.starts_with("base") || s.dead_bases.lock().unwrap().iter().any(|d| d == token)
            {
                return Reply::json(200, r#"{"code":1,"message":"token expired"}"#);
            }
            let region = s
                .region
                .lock()
                .unwrap()
                .clone()
                .unwrap_or("eu-central-1".into());
            Reply::json(
                200,
                json!({"code": 0, "message": "ok", "data": {"cloud_url": url, "device_url": format!("{url}/dev"),
                    "cloud_region": region}})
                .to_string(),
            )
        }
        ("POST", "/v3/auth/refresh_tokens") => {
            let n = count(&seen, "/v3/auth/refresh_tokens");
            Reply::json(
                200,
                json!({"code": 0, "data": {"saasToken": format!("saas{n}"), "cognitoToken": format!("cog{n}"),
                    "cognitoId": "eu-central-1:identity"}})
                .to_string(),
            )
        }
        ("POST", "/cognito") => {
            let n = count(&seen, "/cognito");
            Reply::json(
                200,
                json!({"IdentityId": "eu-central-1:identity", "Credentials": {"AccessKeyId": format!("AKID{n}"),
                    "SecretKey": "SK", "SessionToken": format!("ST{n}"), "Expiration": T0 / 1000 + 3600}})
                .to_string(),
            )
        }
        ("POST", "/dev/v3/user/get_things") => {
            let saas = req.header("accesstoken").unwrap_or("");
            if s.list_fails.load(Ordering::SeqCst)
                || !saas.starts_with("saas")
                || s.dead_saas.lock().unwrap().iter().any(|d| d == saas)
            {
                return Reply::json(200, r#"{"code":1,"message":"token invalid"}"#);
            }
            Reply::json(
                200,
                json!({"code": 0, "message": "ok", "data": [
                    {"deviceId": "ac1", "nickName": "Shop front", "deviceName": "TCL AC", "isOnline": "1"},
                    {"deviceId": "ac2", "nickName": "", "deviceName": "Back room", "isOnline": "1"},
                    {"deviceId": "ac3", "isOnline": "0"},
                ]})
                .to_string(),
            )
        }
        (_, path) if path.starts_with("/iot/") => {
            let refuse = s
                .iot_403
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            let signed = req
                .header("authorization")
                .is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 Credential=AKID"))
                && req
                    .header("x-amz-security-token")
                    .is_some_and(|t| t.starts_with("ST"));
            if refuse || !signed {
                return Reply::json(403, r#"{"message":"Forbidden"}"#);
            }
            if req.method == "GET" {
                let id = path
                    .trim_start_matches("/iot/things/")
                    .trim_end_matches("/shadow");
                match s.reported.lock().unwrap().get(id) {
                    Some(r) => Reply::json(
                        200,
                        json!({"state": {"reported": r}, "metadata": {}, "version": 7, "timestamp": T0 / 1000})
                            .to_string(),
                    ),
                    None => Reply::json(404, r#"{"message":"No shadow exists with name"}"#),
                }
            } else {
                Reply::json(200, r#"{"message":"OK","traceId":"t"}"#)
            }
        }
        _ => Reply::json(404, r#"{"message":"not found"}"#),
    }
}

impl Cloud {
    pub async fn start() -> Cloud {
        let s = Switches::default();
        s.reported.lock().unwrap().extend([
            (
                "ac1".to_string(),
                json!({"powerSwitch": 1, "workMode": 1, "targetTemperature": 26, "currentTemperature": 29.5,
                    "externalUnitTemperature": 34, "windSpeed7Gear": 2, "generatorMode": 0}),
            ),
            // a unit that reports only some fields
            ("ac2".to_string(), json!({"powerSwitch": 0, "currentTemperature": 31.2})),
        ]);
        let switches = s.clone();
        let url: Arc<Mutex<String>> = Arc::default();
        let base = url.clone();
        let fake = FakeHttp::start(move |req: Req| {
            let (s, base) = (switches.clone(), base.clone());
            async move {
                let url = base.lock().unwrap().clone();
                answer(&s, &url, req)
            }
        })
        .await;
        *url.lock().unwrap() = fake.url.clone();
        Cloud {
            fake,
            s,
            clock: Arc::new(AtomicI64::new(T0)),
        }
    }

    pub fn endpoints(&self) -> TclEndpoints {
        TclEndpoints {
            login: format!("{}/account/login?clientId=54148614", self.fake.url),
            cloud_urls: format!("{}/v3/global/cloud_url_get", self.fake.url),
            cognito: Some(format!("{}/cognito", self.fake.url)),
            iot: Some(format!("{}/iot", self.fake.url)),
        }
    }

    pub fn client(&self, password: &str) -> TclClient {
        let clock = self.clock.clone();
        TclClient::new(
            ACCOUNT,
            password,
            self.endpoints(),
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
    }

    pub fn seen(&self) -> Vec<Req> {
        self.s.seen.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|q| q.path).collect()
    }

    pub fn count(&self, path: &str) -> usize {
        count(&self.seen(), path)
    }

    /// AWS IoT calls (shadow reads and publishes).
    pub fn iot(&self) -> Vec<Req> {
        self.seen()
            .into_iter()
            .filter(|q| q.path.starts_with("/iot/"))
            .collect()
    }

    pub fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}
