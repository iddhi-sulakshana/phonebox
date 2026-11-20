//! `GET /api/info` (the settings page's status line), `GET /api/latency`, `POST /api/restart`.

use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::json;

use super::{AppState, json_response, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/info", get(info))
        .route(
            "/api/latency",
            get(|State(s): State<AppState>| async move {
                json_response(StatusCode::OK, &s.timings.summary())
            }),
        )
        .route("/api/restart", post(restart))
}

async fn info(State(s): State<AppState>) -> Response {
    let usb = s.adb.status();
    let now = now_ms();
    // USB (adb) or a live stream from the phone in the last few seconds, which covers Wi-Fi / Tailscale too
    let phone_connected = now - s.phone.last_seen.load(Ordering::SeqCst) < 5000;
    let cfg = s.cfg.get();
    json_response(
        StatusCode::OK,
        &json!({
            "version": crate::app_paths::version(),
            "os": crate::app_paths::os(),
            "configPath": s.cfg.path().display().to_string(),
            "logPath": crate::app_paths::log_path().display().to_string(),
            "adb": crate::adb::find_adb(cfg.adb_path.as_deref()).map(|p| p.display().to_string()),
            "connected": usb.connected || phone_connected,
            "via": if usb.connected { Some("USB") } else if phone_connected { Some("Wi-Fi") } else { None },
            "devices": usb.all_devices,
            "appMissing": usb.missing_app,
            "usbProblem": usb.usb_problem,
            "webviewProblem": crate::webview_doctor::diagnose_recent(&crate::phone_registry::all(), now),
        }),
    )
}

/// Restart requested from the settings page (e.g. Wi-Fi turned on): stop; `main` hands over to a fresh instance.
async fn restart(State(s): State<AppState>) -> StatusCode {
    crate::app_restart::request();
    s.shutdown.notify_one();
    StatusCode::OK
}
