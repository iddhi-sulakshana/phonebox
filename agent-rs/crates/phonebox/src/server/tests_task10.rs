//! Task 10: settings page, panel files, settings / config / info / latency / restart routes.

use axum::body::to_bytes;
use axum::http::StatusCode;

use super::build_router;
use super::guard::tests::{send, test_state};
use crate::config::store::CONFLICT;

const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

async fn body(r: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    to_bytes(r.into_body(), usize::MAX).await.unwrap().to_vec()
}
async fn json(r: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    serde_json::from_slice(&body(r).await).unwrap()
}

async fn put_settings(
    app: &super::App,
    text: &str,
    version: Option<&str>,
) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/settings")
        .header("host", "127.0.0.1:8765")
        .header("content-type", "application/json");
    if let Some(v) = version {
        req = req.header("x-config-version", v);
    }
    app.clone()
        .oneshot(req.body(axum::body::Body::from(text.to_string())).unwrap())
        .await
        .unwrap()
}

// review of PR 1: C# reads the body through a StreamReader, which drops a BOM before the Wi-Fi checks; the Rust checks
// parsed the raw text, failed on the BOM and let a colliding port through to the save (which strips it)
#[tokio::test]
async fn a_bom_before_the_settings_does_not_skip_the_wifi_checks() {
    let (_dir, state) = test_state();
    let app = build_router(state.clone());
    let port = state.endpoint.port;
    let body = format!("\u{feff}{{\"lan\":{{\"enabled\":true,\"port\":{port}}}}}");
    let r = put_settings(&app, &body, None).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json(r).await["error"],
        format!("Wi-Fi port cannot be the same as the local port ({port})")
    );
    assert!(!state.cfg.get().lan.enabled);
}

#[tokio::test]
async fn stale_save_conflicts_and_headerless_save_succeeds() {
    let (_d, state) = test_state();
    let app = build_router(state.clone());
    let v = state.cfg.version_text();
    let text = crate::config::write::to_dotnet_json(&state.cfg.get());
    assert_eq!(put_settings(&app, &text, Some(&v)).await.status(), 200);
    let r = put_settings(&app, &text, Some(&v)).await; // v is stale now
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(json(r).await["error"], CONFLICT);
    let ok = put_settings(&app, &text, None).await;
    assert_eq!(ok.status(), 200);
    assert_eq!(json(ok).await["version"], state.cfg.version_text());
}

#[tokio::test]
async fn settings_put_requires_json_content_type() {
    use tower::ServiceExt;
    let (_d, state) = test_state();
    let app = build_router(state);
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/settings")
        .header("host", "127.0.0.1:8765")
        .header("content-type", "text/plain");
    assert_eq!(
        app.oneshot(req.body(axum::body::Body::from("{}")).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn settings_get_sends_the_settings_and_their_version() {
    let (_d, state) = test_state();
    let app = build_router(state.clone());
    let r = send(&app, "GET", "/api/settings", &[LOCAL]).await;
    assert_eq!(
        r.headers()["x-config-version"],
        state.cfg.version_text().as_str()
    );
    assert_eq!(
        r.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    assert_eq!(json(r).await["weather"]["city"], "Tallinn");
}

#[tokio::test]
async fn wifi_port_and_shared_keys_are_refused() {
    let (_d, state) = test_state();
    let app = build_router(state);
    let r = put_settings(&app, r#"{"lan":{"enabled":true,"port":8765}}"#, None).await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        json(r).await["error"],
        "Wi-Fi port cannot be the same as the local port (8765)"
    );
    let r = put_settings(
        &app,
        r#"{"lan":{"token":"same-key","webhookToken":"same-key"}}"#,
        None,
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        json(r).await["error"],
        "The Wi-Fi key and the webhook key must be different: make a new key for one of them"
    );
    assert_eq!(
        put_settings(&app, r#"{"lan":{"enabled":false,"port":8765}}"#, None)
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn phone_config_hides_commands_and_carries_the_version_as_text() {
    let (_d, state) = test_state();
    state.cfg.save(r#"{"shortcuts":[{"id":"s","label":"S","cmd":"secret.exe","args":"--token abc"}],"display":{"language":"tr"}}"#, None).unwrap();
    let app = build_router(state.clone());
    let r = send(&app, "GET", "/api/config", &[LOCAL]).await;
    let v = json(r).await;
    assert_eq!(v["version"], state.cfg.version_text());
    let text = v.to_string();
    assert!(!text.contains("secret") && !text.contains("--token"));
    assert_eq!(v["config"]["display"]["language"], "tr");
    assert!(
        v["config"]["display"]["homePage"].is_null()
            && v["config"]["display"].get("homePage").is_some(),
        "nulls are written"
    );
}

#[tokio::test]
async fn panel_files_are_the_embedded_bytes() {
    let (_d, state) = test_state();
    let app = build_router(state);
    let r = send(&app, "GET", "/panel/main.js", &[LOCAL]).await;
    assert_eq!(
        r.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        body(r).await,
        include_bytes!("../../../../../android/app/src/main/assets/www/main.js")
    );
    let w = send(&app, "GET", "/panel/widgets/basic.js", &[LOCAL]).await;
    assert_eq!(w.status(), 200);
    let index = send(&app, "GET", "/panel/", &[LOCAL]).await;
    assert_eq!(index.headers()["content-type"], "text/html; charset=utf-8");
    let redirect = send(&app, "GET", "/panel?x=1", &[LOCAL]).await;
    assert_eq!(redirect.status(), StatusCode::FOUND);
    assert_eq!(redirect.headers()["location"], "/panel/?x=1");
    for bad in [
        "/panel/nope.js",
        "/panel/..%2Fsettings.html",
        "/panel/a/b.js",
    ] {
        assert_eq!(
            send(&app, "GET", bad, &[LOCAL]).await.status(),
            404,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn settings_page_and_icon() {
    let (_d, state) = test_state();
    let app = build_router(state);
    let page = send(&app, "GET", "/", &[LOCAL]).await;
    assert_eq!(page.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        body(page).await,
        include_bytes!("../../../../assets/settings.html")
    );
    let icon = send(&app, "GET", "/favicon.ico", &[LOCAL]).await;
    assert_eq!(icon.headers()["content-type"], "image/x-icon");
}

#[tokio::test]
async fn info_reports_the_agent() {
    let (_d, state) = test_state();
    let app = build_router(state.clone());
    let v = json(send(&app, "GET", "/api/info", &[LOCAL]).await).await;
    assert_eq!(v["version"], crate::app_paths::version());
    assert_eq!(v["os"], crate::app_paths::os());
    assert_eq!(v["configPath"], state.cfg.path().display().to_string());
    assert_eq!(v["connected"], false);
    assert!(v["via"].is_null() && v["devices"].is_array() && v["appMissing"].is_array());
    for key in ["adb", "usbProblem", "webviewProblem", "logPath"] {
        assert!(v.get(key).is_some(), "{key}");
    }
}

#[tokio::test]
async fn latency_lists_timed_routes_and_restart_asks_to_stop() {
    let (_d, state) = test_state();
    let app = build_router(state.clone());
    send(&app, "GET", "/api/info", &[LOCAL]).await;
    let rows = json(send(&app, "GET", "/api/latency", &[LOCAL]).await).await;
    assert_eq!(rows[0]["route"], "GET /api/info");
    assert!(rows[0].get("p50Ms").is_some());
    assert!(!crate::app_restart::requested());
    assert_eq!(
        send(&app, "POST", "/api/restart", &[LOCAL]).await.status(),
        200
    );
    assert!(crate::app_restart::requested());
}

#[tokio::test]
async fn json_answers_are_utf8_with_dotnet_numbers() {
    let (_d, state) = test_state();
    state
        .cfg
        .save(r#"{"night":{"brightness":1}}"#, None)
        .unwrap();
    let app = build_router(state);
    for path in ["/api/info", "/api/latency", "/api/config", "/api/settings"] {
        let r = send(&app, "GET", path, &[LOCAL]).await;
        assert_eq!(
            r.headers()["content-type"],
            "application/json; charset=utf-8",
            "{path}"
        );
    }
    // ASP.NET writes the double 1.0 as 1 (JavaScript sees the same number; the parity check sees the same type)
    let config =
        String::from_utf8(body(send(&app, "GET", "/api/config", &[LOCAL]).await).await).unwrap();
    assert!(
        config.contains(r#""brightness":1,"#) || config.contains(r#""brightness":1}"#),
        "{config}"
    );
}

/// Review #1: the settings page starts with `/api/catalog`; without it the page never loads.
#[tokio::test]
async fn catalog_carries_the_widgets_and_templates_files() {
    let (_d, state) = test_state();
    let app = build_router(state);
    let r = send(&app, "GET", "/api/catalog", &[LOCAL]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    let expected = format!(
        "{{\"cols\":12,\"rows\":6,\"widgets\":{},\"templates\":{}}}",
        crate::catalog::WIDGETS_JSON,
        crate::catalog::TEMPLATES_JSON
    );
    assert_eq!(String::from_utf8(body(r).await).unwrap(), expected);
}

/// Review #2: the panel in the layout editor's live preview calls `/agent/api/...`, the phone's own path; on the local
/// port that is `/api/...`, rewritten before routing and before the guard (which must see the `/api` path).
#[tokio::test]
async fn the_live_preview_reaches_the_api_through_agent_api() {
    let (_d, state) = test_state();
    let app = build_router(state);
    let r = send(&app, "GET", "/agent/api/state?ev=0", &[LOCAL]).await;
    assert_eq!(r.status(), 200);
    assert!(json(r).await["events"].is_array());
    assert_eq!(
        send(&app, "GET", "/Agent/API/info", &[LOCAL])
            .await
            .status(),
        200
    ); // any case, as ASP.NET
    // a cross-site page navigation is fine for pages, never for the API: the guard sees the rewritten path
    let navigate = [
        LOCAL,
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-mode", "navigate"),
    ];
    assert_eq!(
        send(&app, "GET", "/agent/api/info", &navigate)
            .await
            .status(),
        403
    );
    assert_eq!(
        send(&app, "GET", "/agent/apix/info", &[LOCAL])
            .await
            .status(),
        404
    ); // whole segments only
}

/// Review #3: an emptied number field reaches the agent as null. C# refuses it (its model has no null for a number), so
/// the file keeps the user's value; it must not turn into the default (Tallinn's latitude) silently.
#[tokio::test]
async fn a_null_number_or_switch_is_refused_like_csharp() {
    let (_d, state) = test_state();
    let cfg = state.cfg.clone();
    let app = build_router(state);
    let put = |body: &'static str| {
        let app = app.clone();
        async move {
            let req = axum::http::Request::builder()
                .method("PUT")
                .uri("/api/settings")
                .header("host", "127.0.0.1:8765")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap();
            tower::ServiceExt::oneshot(app, req).await.unwrap()
        }
    };
    assert_eq!(
        put(r#"{"weather":{"city":"Kandy","lat":7.29,"lon":80.63}}"#)
            .await
            .status(),
        200
    );
    let r = put(r#"{"weather":{"city":"Kandy","lat":null,"lon":80.63}}"#).await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        json(r).await["error"],
        "Invalid JSON: The JSON value could not be converted to System.Double. Path: $.weather.lat."
    );
    assert_eq!(cfg.get().weather.lat, 7.29);
    let r = put(r#"{"pages":[{"id":"p1","widgets":[{"type":"clock","x":null}]}]}"#).await;
    assert_eq!(
        json(r).await["error"],
        "Invalid JSON: The JSON value could not be converted to System.Int32. Path: $.pages[0].widgets[0].x."
    );
    // nulls where C# has a reference type are fine (Normalize fills them), and so are null array items
    assert_eq!(
        put(r#"{"weather":{"city":null},"shortcuts":null,"pages":[null]}"#)
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn the_settings_hooks_run_once_in_order_after_a_successful_save() {
    use std::sync::{Arc, Mutex};
    let (_d, state) = test_state();
    let ran = Arc::new(Mutex::new(Vec::new()));
    for name in ["deck", "photos", "cameras", "radios"] {
        let ran = ran.clone();
        state
            .settings_hooks
            .add(Arc::new(move || ran.lock().unwrap().push(name)));
    }
    let app = build_router(state.clone());
    assert_eq!(put_settings(&app, "{ not json", None).await.status(), 400);
    let stale = put_settings(&app, "{}", Some("1")).await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert!(
        ran.lock().unwrap().is_empty(),
        "a failed save ran the hooks"
    );
    assert_eq!(put_settings(&app, "{}", None).await.status(), 200);
    assert_eq!(
        *ran.lock().unwrap(),
        ["deck", "photos", "cameras", "radios"]
    );
}
