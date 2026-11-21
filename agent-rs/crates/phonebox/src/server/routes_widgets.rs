//! The widget data routes of plan 6 survey B: feeds (and, later in plan 6, calendars, photos, cameras and the feed
//! proxy) (`Program.cs:494-528`, `:809`).

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::json;

use super::query;
use super::{AppState, json_response};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/feeds", get(feeds))
        .route("/api/feeds/open", post(feeds_open))
}

/// Headlines for the Feeds widget; the feed and article addresses stay here, the phone opens an article by its id.
async fn feeds(State(s): State<AppState>) -> Response {
    let snap = s.feeds.snapshot();
    json_response(
        StatusCode::OK,
        &json!({ "items": snap.items, "updated": snap.updated, "errors": snap.errors,
            "canOpen": s.feeds.open_allowed() }),
    )
}

/// The id comes from the query string (any body is ignored).
async fn feeds_open(State(s): State<AppState>, Query(q): QueryList) -> Response {
    match s.feeds.open(query::value(&q, "id").as_deref()) {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

#[cfg(test)]
mod tests {
    use crate::server::guard::tests::{send, test_state};
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn json_of(res: axum::http::Response<Body>) -> Value {
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn the_feed_routes_answer_with_their_shapes() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/feeds", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"items": [], "updated": 0, "errors": [], "canOpen": false})
        );
        let off = r#"Opening articles is off: turn on "Open articles on the PC" in the News feeds widget's settings"#;
        for path in ["/api/feeds/open", "/api/feeds/open?id=x"] {
            let res = send(&app, "POST", path, &[LOCAL]).await;
            assert_eq!(res.status(), 400, "{path}");
            assert_eq!(json_of(res).await, json!({"error": off}), "{path}");
        }
    }

    #[tokio::test]
    async fn the_phone_reaches_the_feeds_over_wifi() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(send(&wifi, "GET", "/api/feeds", &[key]).await.status(), 200);
        assert_eq!(
            send(&wifi, "POST", "/api/feeds/open?id=x", &[key])
                .await
                .status(),
            400
        );
    }
}
