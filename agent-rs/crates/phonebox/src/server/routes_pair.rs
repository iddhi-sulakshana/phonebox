//! Wi-Fi pairing (`Program.cs`): `GET /api/pair` (the phone, through the USB tunnel), `POST /api/pair/start`,
//! `/api/pair/cancel`, `GET /api/pair/status` (the settings page), `POST /api/pair/claim` (the phone, over Wi-Fi,
//! without the key) and `GET /api/lan`. All but the claim are local only (the Wi-Fi allow-list leaves them out).

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use serde_json::json;

use super::{AppState, json_response, now_ms};
use crate::config::store::ConfigStore;
use crate::lan::Wifi;

/// A keyless claim's body: more is refused (Kestrel's limit for keyless Wi-Fi requests).
pub const MAX_CLAIM_BODY: usize = 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pair", get(pair))
        .route("/api/pair/start", post(start))
        .route("/api/pair/cancel", post(cancel))
        .route("/api/pair/status", get(status))
        .route(
            "/api/pair/claim",
            post(
                |State(s): State<AppState>, headers: HeaderMap, body: Body| async move {
                    claim_pairing(&headers, body, &s.cfg, &s.wifi).await
                },
            ),
        )
        .route("/api/lan", get(lan))
}

async fn pair(State(s): State<AppState>) -> Response {
    let lan = s.cfg.get().lan.clone();
    let pc = crate::state::machine_name();
    let value = match s.wifi.cert.as_ref().filter(|_| lan.enabled) {
        None => json!({ "enabled": false, "pc": pc }),
        Some(cert) => json!({
            "enabled": true,
            "pc": pc,
            "port": s.wifi.port,
            "token": lan.token,
            "fingerprint": cert.fingerprint,
            "hosts": crate::lan::addresses(),
        }),
    };
    json_response(StatusCode::OK, &value)
}

async fn start(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.get();
    let Some(cert) = s.wifi.cert.as_ref().filter(|_| cfg.lan.enabled) else {
        return json_response(
            StatusCode::OK,
            &json!({ "enabled": false, "configured": cfg.lan.enabled }),
        );
    };
    let (code, expires_at, attempt) = s.wifi.codes.start(now_ms());
    let pc = crate::state::machine_name();
    let hosts = crate::lan::addresses();
    let link = crate::pairing::link(&pc, &cert.fingerprint, s.wifi.port, &hosts, &code);
    // the phone app of this fork's release (C# links upstream's)
    let version = crate::app_paths::version();
    let apk = format!(
        "https://github.com/{}/releases/download/v{version}/PhoneBox-{version}.apk",
        cfg.updates.repo
    );
    let (qr, app_qr) = (crate::pairing::svg(&link), crate::pairing::svg(&apk));
    json_response(
        StatusCode::OK,
        &json!({
            "enabled": true,
            "pc": pc,
            "hosts": hosts,
            "port": s.wifi.port,
            "expiresAt": expires_at,
            "claims": s.wifi.codes.claims(),
            "attempt": attempt,
            "check": crate::pairing::check(&cert.fingerprint),
            "link": link,
            "qr": qr,
            "app": apk,
            "appQr": app_qr,
        }),
    )
}

/// `(long attempt, …)`: a required parameter, so a missing or unparsable one is a 400.
async fn cancel(State(s): State<AppState>, Query(q): Query<Vec<(String, String)>>) -> StatusCode {
    match super::query::int64(&q, "attempt") {
        Ok(Some(attempt)) => {
            s.wifi.codes.cancel(attempt);
            StatusCode::OK
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn status(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &json!({ "claims": s.wifi.codes.claims() }))
}

/// How long a claim's body may take to arrive (Kestrel drops a body slower than 240 bytes/s after 5 s).
const CLAIM_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `POST /api/pair/claim`. Wi-Fi is checked again once the body has arrived, with the settings that answer the claim:
/// a claim still waiting for its body when Wi-Fi is turned off is refused and does not use up the code.
pub async fn claim_pairing(
    headers: &HeaderMap,
    body: Body,
    cfg: &ConfigStore,
    wifi: &Wifi,
) -> Response {
    let forbidden = || StatusCode::FORBIDDEN.into_response();
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let Some(cert) = wifi.cert.as_ref() else {
        return forbidden();
    };
    if !cfg.get().lan.enabled || declared.is_some_and(|n| n > MAX_CLAIM_BODY as u64) {
        return forbidden();
    }
    let bytes = match tokio::time::timeout(
        CLAIM_BODY_TIMEOUT,
        axum::body::to_bytes(body, MAX_CLAIM_BODY),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        _ => return forbidden(),
    };
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let code = crate::pairing::read_claim_code(content_type, &bytes);
    let cfg = cfg.get();
    if !cfg.lan.enabled || code.is_none() || !wifi.codes.claim(code.as_deref(), now_ms()) {
        return forbidden();
    }
    let model: Vec<String> = headers
        .get_all("x-phonebox-model")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    crate::log::info(
        "Program",
        &format!("phone paired over Wi-Fi ({})", model.join(",")),
    );
    json_response(
        StatusCode::OK,
        &json!({
            "enabled": true,
            "pc": crate::state::machine_name(),
            "port": wifi.port,
            "token": cfg.lan.token,
            "fingerprint": cert.fingerprint,
            "hosts": crate::lan::addresses(),
        }),
    )
}

async fn lan(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.get();
    let active = s.wifi.cert.is_some();
    let restart_needed =
        cfg.lan.enabled != active || (active && cfg.lan.port != i32::from(s.wifi.port));
    json_response(
        StatusCode::OK,
        &json!({
            "configured": cfg.lan.enabled,
            "active": active,
            "port": s.wifi.port,
            "restartNeeded": restart_needed,
            "hosts": crate::lan::addresses(),
            "fingerprint": s.wifi.cert.as_ref().map(|c| c.fingerprint.as_str()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use serde_json::Value;

    use super::*;
    use crate::server::build_router;
    use crate::server::guard::tests::test_state;

    fn test_cert() -> crate::lan_cert::LanCert {
        crate::lan_cert::LanCert::from(crate::lan_cert::create("pair-test", chrono::Utc::now()))
    }

    /// A state whose Wi-Fi listener runs (with a certificate) or not, and whose live config has Wi-Fi on or off.
    fn state(listening: bool, enabled: bool) -> (tempfile::TempDir, AppState) {
        let (dir, mut s) = test_state();
        s.cfg
            .save(
                &format!(r#"{{"lan":{{"enabled":{enabled},"port":8766}}}}"#),
                None,
            )
            .unwrap();
        s.wifi = Arc::new(Wifi {
            port: 8766,
            cert: listening.then(test_cert),
            codes: crate::pairing::PairingCodes::new(),
        });
        (dir, s)
    }

    async fn call(
        s: &AppState,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, Vec<u8>) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8765");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = build_router(s.clone())
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        (
            status,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }

    async fn json(s: &AppState, method: &str, path: &str) -> Value {
        let (status, body) = call(s, method, path, &[], "").await;
        assert_eq!(status, StatusCode::OK, "{method} {path}");
        serde_json::from_slice(&body).unwrap()
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    fn pc() -> String {
        crate::state::machine_name()
    }

    #[tokio::test]
    async fn the_phone_gets_everything_it_needs_for_wifi_only_while_wifi_runs() {
        let (_d, s) = state(true, true);
        let v = json(&s, "GET", "/api/pair").await;
        assert_eq!(
            keys(&v),
            ["enabled", "pc", "port", "token", "fingerprint", "hosts"]
        );
        assert_eq!(v["enabled"], true);
        assert_eq!(v["pc"], pc());
        assert_eq!(v["port"], 8766);
        assert_eq!(v["token"].as_str(), s.cfg.get().lan.token.as_deref());
        assert_eq!(
            v["fingerprint"],
            s.wifi.cert.as_ref().unwrap().fingerprint.as_str()
        );
        assert!(v["hosts"].is_array());

        // turned off in the settings (the listener still runs until a restart), or never started
        for (listening, enabled) in [(true, false), (false, true), (false, false)] {
            let (_d, s) = state(listening, enabled);
            let v = json(&s, "GET", "/api/pair").await;
            assert_eq!(
                v,
                serde_json::json!({ "enabled": false, "pc": pc() }),
                "{listening} {enabled}"
            );
            assert_eq!(keys(&v), ["enabled", "pc"]);
        }
    }

    #[tokio::test]
    async fn starting_a_pairing_without_wifi_says_whether_it_is_configured() {
        for (listening, enabled) in [(false, true), (false, false), (true, false)] {
            let (_d, s) = state(listening, enabled);
            let v = json(&s, "POST", "/api/pair/start").await;
            assert_eq!(keys(&v), ["enabled", "configured"]);
            assert_eq!(v["enabled"], false);
            assert_eq!(v["configured"], enabled);
        }
    }

    #[tokio::test]
    async fn starting_a_pairing_gives_the_link_the_check_and_both_qr_codes() {
        let (_d, s) = state(true, true);
        let before = crate::server::now_ms();
        let v = json(&s, "POST", "/api/pair/start").await;
        assert_eq!(
            keys(&v),
            [
                "enabled",
                "pc",
                "hosts",
                "port",
                "expiresAt",
                "claims",
                "attempt",
                "check",
                "link",
                "qr",
                "app",
                "appQr"
            ]
        );
        let fp = s.wifi.cert.as_ref().unwrap().fingerprint.clone();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["pc"], pc());
        assert_eq!(v["port"], 8766);
        let expires = v["expiresAt"].as_i64().unwrap();
        assert!((before + 300_000..=crate::server::now_ms() + 300_000).contains(&expires));
        assert_eq!(v["claims"], 0);
        assert_eq!(v["attempt"], 1);
        assert_eq!(v["check"], crate::pairing::check(&fp));
        let link = v["link"].as_str().unwrap();
        assert!(
            link.starts_with("https://ersinkoc.github.io/phonebox/pair/#v=1&pc="),
            "{link}"
        );
        assert!(link.contains(&format!("&fp={fp}&port=8766&h=")), "{link}");
        let code = link.rsplit_once("&c=").unwrap().1;
        assert_eq!(code.len(), 22);
        assert!(v["qr"].as_str().unwrap().starts_with("<svg"));
        let version = crate::app_paths::version();
        assert_eq!(
            v["app"],
            format!(
                "https://github.com/{}/releases/download/v{version}/PhoneBox-{version}.apk",
                s.cfg.get().updates.repo
            )
        );
        assert!(v["appQr"].as_str().unwrap().starts_with("<svg"));

        // the code from the link pairs once; a new start replaces it
        assert_eq!(json(&s, "POST", "/api/pair/start").await["attempt"], 2);
        assert!(!s.wifi.codes.claim(Some(code), crate::server::now_ms()));
    }

    #[tokio::test]
    async fn cancelling_needs_the_attempt_number() {
        let (_d, s) = state(true, true);
        let link = json(&s, "POST", "/api/pair/start").await["link"]
            .as_str()
            .unwrap()
            .to_string();
        let code = link.rsplit_once("&c=").unwrap().1.to_string();
        for path in [
            "/api/pair/cancel",
            "/api/pair/cancel?attempt=",
            "/api/pair/cancel?attempt=x",
        ] {
            assert_eq!(
                call(&s, "POST", path, &[], "").await.0,
                StatusCode::BAD_REQUEST,
                "{path}"
            );
        }
        // an older dialog's cancel leaves the pairing alone
        assert_eq!(
            call(&s, "POST", "/api/pair/cancel?attempt=0", &[], "").await,
            (StatusCode::OK, vec![])
        );
        assert_eq!(
            call(&s, "POST", "/api/pair/cancel?Attempt=1", &[], "").await,
            (StatusCode::OK, vec![])
        );
        assert!(
            !s.wifi.codes.claim(Some(&code), crate::server::now_ms()),
            "cancelled"
        );
    }

    async fn start_code(s: &AppState) -> String {
        let v = json(s, "POST", "/api/pair/start").await;
        v["link"]
            .as_str()
            .unwrap()
            .rsplit_once("&c=")
            .unwrap()
            .1
            .to_string()
    }

    #[tokio::test]
    async fn a_claim_gives_the_wifi_key_once_and_the_page_sees_it() {
        let (_d, s) = state(true, true);
        assert_eq!(
            json(&s, "GET", "/api/pair/status").await,
            serde_json::json!({ "claims": 0 })
        );
        let code = start_code(&s).await;
        let body = format!(r#"{{"code":"{code}"}}"#);
        let headers = [
            ("content-type", "application/json"),
            ("x-phonebox-model", "Pixel"),
        ];

        let (status, answer) = call(&s, "POST", "/api/pair/claim", &headers, &body).await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_slice(&answer).unwrap();
        assert_eq!(
            keys(&v),
            ["enabled", "pc", "port", "token", "fingerprint", "hosts"]
        );
        assert_eq!(v["enabled"], true);
        assert_eq!(v["port"], 8766);
        assert_eq!(v["token"].as_str(), s.cfg.get().lan.token.as_deref());
        assert_eq!(
            v["fingerprint"],
            s.wifi.cert.as_ref().unwrap().fingerprint.as_str()
        );
        assert_eq!(
            json(&s, "GET", "/api/pair/status").await,
            serde_json::json!({ "claims": 1 })
        );

        // used up
        assert_eq!(
            call(&s, "POST", "/api/pair/claim", &headers, &body).await,
            (StatusCode::FORBIDDEN, vec![])
        );
        assert_eq!(json(&s, "GET", "/api/pair/status").await["claims"], 1);
    }

    #[tokio::test]
    async fn a_bad_claim_is_refused_without_using_up_the_code() {
        let (_d, s) = state(true, true);
        let code = start_code(&s).await;
        let good = format!(r#"{{"code":"{code}"}}"#);
        let json_type = ("content-type", "application/json");
        let padded = format!(r#"{{"pad":"{}","code":"{code}"}}"#, "x".repeat(1100));
        for (headers, body) in [
            (vec![json_type], r#"{"code":"wrong"}"#.to_string()),
            (vec![("content-type", "text/plain")], good.clone()),
            (vec![json_type, ("content-length", "1025")], good.clone()),
            (vec![json_type], padded),
        ] {
            assert_eq!(
                call(&s, "POST", "/api/pair/claim", &headers, &body).await,
                (StatusCode::FORBIDDEN, vec![]),
                "{headers:?} {}",
                &body[..body.len().min(40)]
            );
        }
        assert!(
            s.wifi.codes.claim(Some(&code), crate::server::now_ms()),
            "the code still works"
        );

        // Wi-Fi off in the settings, or the listener not running: refused before the body is read
        for (listening, enabled) in [(true, false), (false, true)] {
            let (_d, s) = state(listening, enabled);
            let (status, _) = call(
                &s,
                "POST",
                "/api/pair/claim",
                &[json_type],
                r#"{"code":"x"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{listening} {enabled}");
        }
    }

    // PairingTests: a claim waiting for its body when Wi-Fi is turned off must be refused, and must not use up the code.
    #[tokio::test]
    async fn a_claim_that_outlives_wifi_being_turned_off_is_refused_and_keeps_the_code() {
        let (_d, s) = state(true, true);
        let (code, _, _) = s.wifi.codes.start(crate::server::now_ms());

        let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let bytes = axum::body::Bytes::from(format!(r#"{{"code":"{code}"}}"#));
        let body = Body::from_stream(futures_util::stream::once(async move {
            let _ = entered_tx.send(());
            let _ = released.await;
            Ok::<_, std::io::Error>(bytes)
        }));
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let claim = {
            let (cfg, wifi) = (s.cfg.clone(), s.wifi.clone());
            tokio::spawn(async move { claim_pairing(&headers, body, &cfg, &wifi).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        s.cfg.save(r#"{"lan":{"enabled":false}}"#, None).unwrap();
        release.send(()).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), claim)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status(), StatusCode::FORBIDDEN);
        assert!(s.wifi.codes.claim(Some(&code), crate::server::now_ms()));
    }

    #[tokio::test]
    async fn the_settings_page_learns_whether_wifi_needs_a_restart() {
        // (listener runs, live enabled, live port) → restartNeeded
        for (listening, enabled, port, restart) in [
            (false, false, 8766, false),
            (false, true, 8766, true),
            (true, true, 8766, false),
            (true, true, 8767, true),
            (true, false, 8766, true),
        ] {
            let (_d, s) = state(listening, enabled);
            s.cfg
                .save(
                    &format!(r#"{{"lan":{{"enabled":{enabled},"port":{port}}}}}"#),
                    None,
                )
                .unwrap();
            let v = json(&s, "GET", "/api/lan").await;
            let row = format!("{listening} {enabled} {port}");
            assert_eq!(
                keys(&v),
                [
                    "configured",
                    "active",
                    "port",
                    "restartNeeded",
                    "hosts",
                    "fingerprint"
                ],
                "{row}"
            );
            assert_eq!(v["configured"], enabled, "{row}");
            assert_eq!(v["active"], listening, "{row}");
            assert_eq!(v["port"], 8766, "the port Wi-Fi started with: {row}");
            assert_eq!(v["restartNeeded"], restart, "{row}");
            assert!(v["hosts"].is_array());
            match &s.wifi.cert {
                Some(cert) => assert_eq!(v["fingerprint"], cert.fingerprint.as_str()),
                None => assert_eq!(v["fingerprint"], Value::Null, "{row}"),
            }
        }
    }
}
