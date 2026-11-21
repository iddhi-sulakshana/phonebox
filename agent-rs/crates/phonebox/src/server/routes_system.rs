//! The system routes: `GET /api/network/adapters` (Settings › Ping & network); the process and battery routes come
//! with their monitors.

use axum::Router;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;

use super::{AppState, json_response};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/network/adapters", get(network_adapters))
}

/// `SystemMonitor.Adapters(cfg.Get().Ping.Adapters)`: the list does not depend on the chosen adapters.
async fn network_adapters() -> Response {
    json_response(StatusCode::OK, &crate::system_monitor::adapters())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    use crate::server::build_router;
    use crate::server::guard::tests::{send, test_state};

    #[tokio::test]
    async fn network_adapters_lists_the_up_adapters_as_json() {
        let (_dir, state) = test_state();
        let app = build_router(state);
        let res = send(
            &app,
            "GET",
            "/api/network/adapters",
            &[("host", "127.0.0.1:8765")],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(!list.is_empty(), "{}", String::from_utf8_lossy(&body));
        assert!(
            list.iter()
                .all(|a| a["name"].is_string() && a["auto"].is_boolean())
        );
    }
}
