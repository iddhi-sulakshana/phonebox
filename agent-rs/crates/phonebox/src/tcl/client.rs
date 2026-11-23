//! TCL's cloud as the tcl-server dashboard (and TCL's Android app) uses it: the account sign-in (the MD5 of the
//! password) gives a base token for about 30 days; from it the cloud-URL lookup, the SaaS token (about 30 minutes) and
//! Cognito's temporary AWS credentials (about an hour) make a session; the device list comes from TCL's device host,
//! and each unit's state is its AWS IoT shadow (read, and changed by publishing to its update topic), SigV4-signed.
//! The session is renewed from the base token before it ends, or once after a refusal; a dead base token signs in
//! with the password again. Errors never carry the password, a token or a key.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::sigv4::{AwsCredentials, SignRequest, md5_hex, sign, uri_encode};

/// The `appId` of the cloud calls (TCL's Android app's; public in tcl-server's `config.ts`).
pub const APP_ID: &str = "wx6e1af3fa84fbe523";
const LOGIN_URL: &str = "https://pa.account.tcl.com/account/login?clientId=54148614";
const CLOUD_URLS: &str = "https://prod-center.aws.tcljd.com/v3/global/cloud_url_get";
const TIMEOUT: Duration = Duration::from_secs(20);
/// A session is renewed after this (the SaaS token lasts about 30 minutes) …
const SESSION_FOR_MS: i64 = 25 * 60_000;
/// … or this long before the AWS credentials end.
const CREDS_MARGIN_MS: i64 = 5 * 60_000;
/// After TCL refuses the password, no new sign-in for this long: TCL locks accounts that keep failing.
const REFUSED_WAIT_MS: i64 = 10 * 60_000;
/// After a sign-in that got no answer (or a session that could not be made from it), this long.
const FAILED_WAIT_MS: i64 = 30_000;
const NO_ANSWER: &str = "TCL did not answer";
const BAD_ID: &str = "Not a TCL unit id";
const JSON: &str = "application/json; charset=UTF-8";

/// Where the chain's requests go: TCL's own hosts, or a test server.
#[derive(Clone)]
pub struct TclEndpoints {
    pub login: String,
    pub cloud_urls: String,
    /// None: Cognito in the account's region.
    pub cognito: Option<String>,
    /// None: AWS IoT's data endpoint in the account's region.
    pub iot: Option<String>,
}

impl TclEndpoints {
    pub fn tcl() -> TclEndpoints {
        TclEndpoints {
            login: LOGIN_URL.into(),
            cloud_urls: CLOUD_URLS.into(),
            cognito: None,
            iot: None,
        }
    }
}

/// A unit in the account.
#[derive(Debug, Clone, PartialEq)]
pub struct TclDevice {
    /// TCL's device id, the AWS IoT thing name.
    pub id: String,
    pub name: String,
    pub online: bool,
}

/// The sign-in's result, good for about 30 days.
struct Base {
    token: String,
    /// The account's TCL user name (`ssoId`).
    sso: String,
    country: String,
}

/// What the requests use until it is renewed.
struct Session {
    device_url: String,
    region: String,
    country: String,
    saas: String,
    creds: AwsCredentials,
    minted_ms: i64,
    expires_ms: i64,
}

#[derive(Default)]
struct State {
    base: Option<Base>,
    session: Option<Arc<Session>>,
    /// No password sign-in before this (Unix ms), with `wait_error` the answer meanwhile.
    wait_until: i64,
    wait_error: Option<String>,
}

/// Why a sign-in or a session failed, and whether TCL refused it (rather than not answering).
type Failure = (String, bool);

/// One TCL account. No `Debug`: it holds the password and the tokens.
pub struct TclClient {
    username: String,
    password: String,
    endpoints: TclEndpoints,
    http: reqwest::Client,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Held while a session is made, so requests that start together share one.
    state: tokio::sync::Mutex<State>,
}

/// An AWS IoT thing name as TCL's are: letters, digits, `_ . : -`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

fn valid_region(region: &str) -> bool {
    !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn absolute_http(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The reason in a TCL reply (`msg` or `message`), else the HTTP status.
fn reason(status: u16, v: &Value) -> String {
    [text(v, "msg"), text(v, "message")]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or_else(|| format!("HTTP {status}"))
}

/// 12 random base-36 characters, as the app's request nonce.
fn nonce() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("the system random source");
    bytes
        .iter()
        .map(|b| char::from_digit(u32::from(*b % 36), 36).unwrap_or('0'))
        .collect()
}

/// After a failed password sign-in: none again until the wait is over, answering `error` meanwhile.
fn hold(st: &mut State, now: i64, error: &str, refused: bool) {
    st.wait_until = now
        + if refused {
            REFUSED_WAIT_MS
        } else {
            FAILED_WAIT_MS
        };
    st.wait_error = Some(error.to_string());
}

/// A request without an answer: "did not answer" on a timeout, else what failed (never the URL).
fn failure(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        NO_ANSWER.into()
    } else {
        crate::net::error_text(e)
    }
}

impl TclClient {
    pub fn new(
        username: &str,
        password: &str,
        endpoints: TclEndpoints,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> TclClient {
        TclClient {
            username: username.trim().to_string(),
            password: password.to_string(),
            endpoints,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("the TCL client"),
            now_ms,
            state: tokio::sync::Mutex::new(State::default()),
        }
    }

    /// The units in the account.
    pub async fn devices(&self) -> Result<Vec<TclDevice>, String> {
        let session = self.session(None).await?;
        match self.things(&session).await? {
            Ok(list) => Ok(list),
            Err(_) => {
                let session = self.session(Some(&session)).await?;
                self.things(&session)
                    .await?
                    .map_err(|m| format!("TCL's device list refused the session: {m}"))
            }
        }
    }

    /// A unit's reported state (`state.reported` of its shadow).
    pub async fn reported(&self, id: &str) -> Result<Value, String> {
        if !valid_id(id) {
            return Err(BAD_ID.into());
        }
        let (status, body) = self
            .iot(
                reqwest::Method::GET,
                &format!("/things/{id}/shadow"),
                "",
                b"",
            )
            .await?;
        match status {
            404 => Err("TCL has no state for this unit yet".into()),
            200..=299 => {
                let shadow: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                Ok(match shadow.pointer("/state/reported") {
                    Some(r @ Value::Object(_)) => r.clone(),
                    _ => Value::Object(Map::new()),
                })
            }
            _ => Err(format!("AWS IoT answered {status}")),
        }
    }

    /// Asks a unit for `properties` (its shadow's desired state); a `targetTemperature` goes with
    /// `targetCelsiusDegree`, as some units need. Accepted is not applied: the unit reports when it did.
    pub async fn publish(
        &self,
        id: &str,
        mut properties: Map<String, Value>,
    ) -> Result<(), String> {
        if !valid_id(id) {
            return Err(BAD_ID.into());
        }
        if let Some(t) = properties.get("targetTemperature").cloned() {
            properties.entry("targetCelsiusDegree").or_insert(t);
        }
        let body = json!({
            "state": { "desired": properties },
            "clientToken": format!("mobile_{}", (self.now_ms)() / 1000),
        })
        .to_string();
        let topic = uri_encode(&format!("$aws/things/{id}/shadow/update"));
        let (status, _) = self
            .iot(
                reqwest::Method::POST,
                &format!("/topics/{topic}"),
                "qos=1",
                body.as_bytes(),
            )
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("AWS IoT answered {status}"))
        }
    }

    /// The current session: kept while fresh, else renewed from the base token, else a new sign-in. `refused` is
    /// a session a request was refused with: renewed even if fresh (unless another request renewed it meanwhile).
    async fn session(&self, refused: Option<&Arc<Session>>) -> Result<Arc<Session>, String> {
        let mut st = self.state.lock().await;
        let now = (self.now_ms)();
        if let Some(s) = &st.session
            && !refused.is_some_and(|r| Arc::ptr_eq(r, s))
            && now < s.minted_ms + SESSION_FOR_MS
            && now < s.expires_ms - CREDS_MARGIN_MS
        {
            return Ok(s.clone());
        }
        st.session = None;
        if let Some(base) = &st.base {
            match self.mint(base, now).await {
                Ok(s) => {
                    let s = Arc::new(s);
                    st.session = Some(s.clone());
                    return Ok(s);
                }
                // TCL no longer takes the base token: sign in with the password
                Err((error, true)) => {
                    crate::log::debug(
                        CATEGORY,
                        &format!("tcl: renewal refused, signing in: {error}"),
                    );
                    st.base = None;
                }
                // no answer, or an answer it could not use: the base token stays, the password is not sent again
                Err((error, false)) => return Err(error),
            }
        }
        if now < st.wait_until {
            return Err(st.wait_error.clone().unwrap_or_else(|| NO_ANSWER.into()));
        }
        let base = match self.login().await {
            Ok(base) => base,
            Err((error, refused)) => {
                hold(&mut st, now, &error, refused);
                return Err(error);
            }
        };
        st.wait_until = 0;
        st.wait_error = None;
        match self.mint(&base, now).await {
            Ok(s) => {
                let s = Arc::new(s);
                st.base = Some(base);
                st.session = Some(s.clone());
                Ok(s)
            }
            Err((error, refused)) => {
                // a session that cannot be made even from a fresh sign-in: no new sign-in for a while either
                if !refused {
                    st.base = Some(base);
                }
                hold(&mut st, now, &error, refused);
                Err(error)
            }
        }
    }

    async fn post(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: String,
    ) -> Result<(u16, Value), String> {
        let mut req = self.http.post(url).body(body);
        for (name, value) in headers {
            req = req.header(*name, value);
        }
        let res = req.send().await.map_err(|e| failure(&e))?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| failure(&e))?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// The account sign-in, as TCL's Android app sends it.
    async fn login(&self) -> Result<Base, Failure> {
        let body = json!({
            "username": self.username,
            "password": md5_hex(&self.password),
            "equipment": 2,
            "osType": 1,
            "clientVersion": "4.8.1",
            "osVersion": "6.0",
            "deviceModel": "AndroidAndroid SDK built for x86",
            "captchaRule": 2,
            "channel": "app",
        });
        let headers = [
            ("th_platform", "android".to_string()),
            ("th_version", "4.8.1".to_string()),
            ("th_appbulid", "830".to_string()),
            ("user-agent", "Android".to_string()),
            ("content-type", JSON.to_string()),
        ];
        let (status, v) = self
            .post(&self.endpoints.login, &headers, body.to_string())
            .await
            .map_err(|e| (e, false))?;
        let user = v.get("user").cloned().unwrap_or(Value::Null);
        let base = Base {
            token: text(&v, "token"),
            sso: text(&user, "username"),
            country: text(&user, "countryAbbr"),
        };
        if status != 200 || v.get("status").and_then(Value::as_i64) != Some(1) {
            return Err((
                format!("TCL refused the sign-in: {}", reason(status, &v)),
                true,
            ));
        }
        if base.token.is_empty() || base.sso.is_empty() {
            return Err(("TCL's sign-in reply had no token".into(), true));
        }
        crate::log::info(CATEGORY, "tcl: signed in");
        Ok(base)
    }

    /// The short-lived part from the base token: cloud URLs, SaaS and Cognito tokens, AWS credentials.
    async fn mint(&self, base: &Base, now: i64) -> Result<Session, Failure> {
        let refused = |status: u16, v: &Value| {
            format!("TCL's cloud refused the session: {}", reason(status, v))
        };
        let json_type = [("content-type", JSON.to_string())];
        let (status, v) = self
            .post(
                &self.endpoints.cloud_urls,
                &json_type,
                json!({ "ssoId": base.sso, "ssoToken": base.token, "appId": APP_ID }).to_string(),
            )
            .await
            .map_err(|e| (e, false))?;
        if v.get("code").and_then(Value::as_i64) != Some(0) {
            return Err((refused(status, &v), true));
        }
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let (cloud_url, device_url, region) = (
            text(&data, "cloud_url").trim_end_matches('/').to_string(),
            text(&data, "device_url").trim_end_matches('/').to_string(),
            text(&data, "cloud_region"),
        );
        if !valid_region(&region) {
            return Err(("TCL's cloud answered an unexpected region".into(), false));
        }
        if !absolute_http(&cloud_url) || !absolute_http(&device_url) {
            return Err(("TCL's cloud answered an unexpected address".into(), false));
        }
        let (status, v) = self
            .post(
                &format!("{cloud_url}/v3/auth/refresh_tokens"),
                &json_type,
                json!({ "ssoToken": base.token, "userId": base.sso, "appId": APP_ID }).to_string(),
            )
            .await
            .map_err(|e| (e, false))?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let (saas, cognito_token, cognito_id) = (
            text(&data, "saasToken"),
            text(&data, "cognitoToken"),
            text(&data, "cognitoId"),
        );
        if v.get("code").and_then(Value::as_i64) != Some(0)
            || saas.is_empty()
            || cognito_token.is_empty()
            || cognito_id.is_empty()
        {
            return Err((refused(status, &v), true));
        }
        let cognito = self
            .endpoints
            .cognito
            .clone()
            .unwrap_or_else(|| format!("https://cognito-identity.{region}.amazonaws.com/"));
        let (status, v) = self
            .post(
                &cognito,
                &[
                    (
                        "x-amz-target",
                        "AWSCognitoIdentityService.GetCredentialsForIdentity".to_string(),
                    ),
                    ("content-type", "application/x-amz-json-1.1".to_string()),
                ],
                json!({ "IdentityId": cognito_id,
                    "Logins": { "cognito-identity.amazonaws.com": cognito_token } })
                .to_string(),
            )
            .await
            .map_err(|e| (e, false))?;
        let c = v.get("Credentials").cloned().unwrap_or(Value::Null);
        let creds = AwsCredentials {
            access_key: text(&c, "AccessKeyId"),
            secret: text(&c, "SecretKey"),
            session_token: Some(text(&c, "SessionToken")).filter(|t| !t.is_empty()),
        };
        if status != 200 || creds.access_key.is_empty() || creds.secret.is_empty() {
            return Err((
                format!("AWS Cognito refused TCL's identity (HTTP {status})"),
                false,
            ));
        }
        let expires_ms = c
            .get("Expiration")
            .and_then(Value::as_f64)
            .map_or(now + 3_600_000, |s| (s * 1000.0) as i64);
        Ok(Session {
            device_url,
            region,
            country: base.country.clone(),
            saas,
            creds,
            minted_ms: now,
            expires_ms,
        })
    }

    /// The device list with `session`: Ok(Err(reason)) when TCL refused the session.
    async fn things(&self, s: &Session) -> Result<Result<Vec<TclDevice>, String>, String> {
        let timestamp = (self.now_ms)().to_string();
        let nonce = nonce();
        let sign = md5_hex(&format!("{timestamp}{nonce}{}", s.saas));
        let headers = [
            ("platform", "android".to_string()),
            ("appversion", "5.4.1".to_string()),
            ("thomeversion", "4.8.1".to_string()),
            ("accesstoken", s.saas.clone()),
            ("countrycode", s.country.clone()),
            ("accept-language", "en".to_string()),
            ("content-type", JSON.to_string()),
            ("timestamp", timestamp),
            ("nonce", nonce),
            ("sign", sign),
        ];
        let (status, v) = self
            .post(
                &format!("{}/v3/user/get_things", s.device_url),
                &headers,
                "{}".into(),
            )
            .await?;
        let (Some(0), Some(list)) = (
            v.get("code").and_then(Value::as_i64),
            v.get("data").and_then(Value::as_array),
        ) else {
            return Ok(Err(reason(status, &v)));
        };
        Ok(Ok(list
            .iter()
            .filter_map(|d| {
                let id = text(d, "deviceId");
                if id.is_empty() {
                    return None;
                }
                let name = [text(d, "nickName"), text(d, "deviceName")]
                    .into_iter()
                    .find(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone());
                let online = match d.get("isOnline") {
                    Some(Value::String(s)) => s == "1",
                    Some(Value::Number(n)) => n.as_i64() == Some(1),
                    Some(Value::Bool(b)) => *b,
                    _ => false,
                };
                Some(TclDevice { id, name, online })
            })
            .collect()))
    }

    /// An AWS IoT call with the session, renewed and tried once more after a 401 or 403.
    async fn iot(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Result<(u16, String), String> {
        let session = self.session(None).await?;
        let mut reply = self.iot_call(&session, &method, path, query, body).await?;
        if matches!(reply.0, 401 | 403) {
            let session = self.session(Some(&session)).await?;
            reply = self.iot_call(&session, &method, path, query, body).await?;
        }
        if matches!(reply.0, 401 | 403) {
            return Err(format!(
                "AWS IoT refused TCL's credentials (HTTP {})",
                reply.0
            ));
        }
        Ok(reply)
    }

    async fn iot_call(
        &self,
        s: &Session,
        method: &reqwest::Method,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Result<(u16, String), String> {
        let host = self
            .endpoints
            .iot
            .clone()
            .unwrap_or_else(|| format!("https://data-ats.iot.{}.amazonaws.com", s.region));
        let text = match query {
            "" => format!("{host}{path}"),
            q => format!("{host}{path}?{q}"),
        };
        let url = reqwest::Url::parse(&text).map_err(|_| BAD_ID.to_string())?;
        let headers: Vec<(String, String)> = if body.is_empty() {
            Vec::new()
        } else {
            vec![("content-type".into(), "application/json".into())]
        };
        let signed = sign(
            &SignRequest {
                method: method.as_str(),
                url: &url,
                headers: &headers,
                body,
                region: &s.region,
                service: "iotdata",
            },
            &s.creds,
            (self.now_ms)(),
        );
        let mut req = self.http.request(method.clone(), url);
        for (name, value) in headers.iter().chain(signed.iter()) {
            req = req.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
        let res = req.send().await.map_err(|e| failure(&e))?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| failure(&e))?;
        Ok((status, text))
    }
}

const CATEGORY: &str = "Tcl";

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
