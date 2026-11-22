//! The update routes (`Program.cs:308-313`), local only: the status, a check, an apply, and the phone app's install.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::{AppState, json_response};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/update", get(status))
        .route("/api/phone/install", post(install))
        .route("/api/update/check", post(check))
        .route("/api/update/apply", post(apply))
}

/// The settings page's update card: the status, whether updates install themselves, and whether this copy can.
async fn status(State(s): State<AppState>) -> Response {
    json_response(
        StatusCode::OK,
        &json!({
            "status": s.updates.status(),
            "auto": s.cfg.get().updates.auto,
            "installed": s.updates.installed(),
        }),
    )
}

/// Settings › Connection › Install the phone app.
async fn install(State(s): State<AppState>) -> Response {
    match s.updates.install_missing().await {
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
    }
}

/// A check now (it waits for an apply that is running); the bare status.
async fn check(State(s): State<AppState>) -> Response {
    let _ = s.updates.check().await;
    json_response(StatusCode::OK, &s.updates.status())
}

/// Starts an apply and answers at once; it runs until it is done (or the agent ends).
async fn apply(State(s): State<AppState>) -> Response {
    let updates = s.updates.clone();
    tokio::spawn(async move { updates.apply().await });
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use crate::server::guard::tests::{send, test_state};

    async fn text(res: axum::response::Response) -> String {
        use http_body_util::BodyExt;
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    #[tokio::test]
    async fn the_update_routes_answer_as_c_sharp_before_any_check() {
        let (_d, s) = test_state();
        let app = crate::server::routes(s.clone());
        let res = send(&app, "GET", "/api/update", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        let version = crate::app_paths::version();
        assert_eq!(
            text(res).await,
            format!(
                r#"{{"status":{{"agentVersion":"{version}","appVersion":null,"latest":null,"releaseUrl":null,"agentUpdate":false,"appUpdate":false,"lastCheck":null,"state":"idle","error":null}},"auto":true,"installed":false}}"#
            )
        );
        let res = send(&app, "POST", "/api/phone/install", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            text(res).await,
            r#"{"error":"no connected phone is missing the app"}"#
        );
        let res = send(&app, "POST", "/api/update/apply", &[LOCAL]).await;
        assert_eq!(res.status(), 202);
        assert_eq!(text(res).await, "");
    }
}
