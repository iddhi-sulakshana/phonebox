//! The battery care routes (`Program.cs:609-616`): the phone's battery report, the guard's state and the settings'
//! test switch.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::{AppState, json_response, query};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/phone/battery", post(battery))
        .route("/api/charge", get(status))
        .route("/api/charge/test", post(test_switch))
}

/// The phone reports its battery (`int level`, `bool plugged` required; `double? temp`, `string? source`,
/// `string? usb`): kept for the charge guard and the USB doctor.
async fn battery(Query(q): QueryList) -> Response {
    let (Ok(Some(level)), Ok(Some(plugged)), Ok(temp)) = (
        query::int32(&q, "level"),
        query::boolean(&q, "plugged"),
        query::float64(&q, "temp"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    crate::charge_guard::report(
        level,
        plugged,
        temp,
        query::value(&q, "source").as_deref(),
        query::value(&q, "usb").as_deref(),
    );
    StatusCode::OK.into_response()
}

/// The guard's state for the settings page (every 5 s while it is open).
async fn status(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.charge.status())
}

/// Settings › Battery care › Test: switches the configured supply on or off (whether the guard is on or not).
async fn test_switch(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(action) = query::value(&q, "action") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !matches!(action.as_str(), "on" | "off") {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "unknown action" }),
        );
    }
    let charge = s.cfg.get().charge.clone();
    match s.charge.switch(&charge, &action).await {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};

    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn read(res: axum::http::Response<Body>) -> (u16, String) {
        let status = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn the_phone_reports_its_battery_and_the_settings_read_the_guard() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        for bad in [
            "/api/phone/battery?plugged=true",
            "/api/phone/battery?level=50",
            "/api/phone/battery?level=50&plugged=maybe",
            "/api/phone/battery?level=x&plugged=true",
            "/api/phone/battery?level=50&plugged=true&temp=warm",
        ] {
            assert_eq!(
                read(send(&app, "POST", bad, &[LOCAL]).await).await.0,
                400,
                "{bad}"
            );
        }
        // the level is clamped, the source trimmed and lower-cased; no USB view (an older app) explains nothing
        assert_eq!(
            read(
                send(
                    &app,
                    "POST",
                    "/api/phone/battery?level=150&plugged=true&temp=31.5&source=%20AC%20",
                    &[LOCAL]
                )
                .await
            )
            .await,
            (200, String::new())
        );
        let (status, body) = read(send(&app, "GET", "/api/charge", &[LOCAL]).await).await;
        assert_eq!(status, 200);
        let mut status: Value = serde_json::from_str(&body).unwrap();
        assert!(status["phone"]["at"].as_i64().unwrap() > 0);
        status["phone"]["at"] = json!(0);
        assert_eq!(
            status,
            json!({"phone": {"level": 100, "plugged": true, "temp": 31.5, "at": 0, "source": "ac", "usb": null},
                   "power": null, "switchedAt": null, "error": null})
        );
    }

    #[tokio::test]
    async fn the_test_switch_needs_on_or_off_and_reports_why_it_failed() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s);
        assert_eq!(
            read(send(&app, "POST", "/api/charge/test", &[LOCAL]).await)
                .await
                .0,
            400
        );
        let (status, body) =
            read(send(&app, "POST", "/api/charge/test?action=dance", &[LOCAL]).await).await;
        assert_eq!(
            (status, body.as_str()),
            (400, r#"{"error":"unknown action"}"#)
        );
        let (status, body) =
            read(send(&app, "POST", "/api/charge/test?action=on", &[LOCAL]).await).await;
        assert_eq!(
            (status, body.as_str()),
            (400, r#"{"error":"no Home Assistant switch set"}"#)
        );
    }
}
