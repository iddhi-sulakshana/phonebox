//! Home Assistant, OBS, Discord and Spotify routes (`Program.cs:537-548` and on).

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::query;
use super::{AppState, json_response};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ha/states", get(ha_states))
        .route("/api/ha/toggle/{id}", post(ha_toggle))
        .route("/api/ha/entities", get(ha_entities))
        .route("/api/ha/test", post(ha_test))
}

/// `?ids=a,b` (empty entries dropped, trimmed).
async fn ha_states(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let ids: Vec<String> = query::value(&q, "ids")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .map(String::from)
        .collect();
    json_response(StatusCode::OK, &s.ha.states(&ids).await)
}

async fn ha_toggle(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.ha.toggle(&id).await {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

async fn ha_entities(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.ha.list_entities().await)
}

/// Settings › Test with the (maybe unsaved) address and token: `{"result":"…"}`.
async fn ha_test(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match query::json_object(&headers, &body) {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let (Ok(url), Ok(token)) = (
        query::json_string(&body, "url"),
        query::json_string(&body, "token"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let result = s.ha.test(url.unwrap_or(""), token.unwrap_or("")).await;
    json_response(StatusCode::OK, &json!({ "result": result }))
}

#[cfg(test)]
mod tests {
    use crate::server::guard::tests::{send, test_state};
    use crate::testutil::{FakeHttp, Reply};
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn json_of(res: axum::http::Response<Body>) -> Value {
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    async fn post(
        app: &crate::server::App,
        path: &str,
        content_type: &str,
        body: &str,
    ) -> axum::http::Response<Body> {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:8765")
            .header("content-type", content_type)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn the_home_assistant_routes_answer_with_their_shapes() {
        let server = FakeHttp::start(|req| async move {
            match req.path.as_str() {
                "/api/states" => Reply::json(
                    200,
                    r#"[{"entity_id":"light.desk","state":"on","attributes":{"friendly_name":"Desk lamp"}}]"#,
                ),
                "/api/" if req.header("authorization") == Some("Bearer secret") => Reply::json(200, "{}"),
                "/api/" => Reply::json(401, "{}"),
                _ => Reply::json(200, "[]"),
            }
        })
        .await;
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        // not configured
        let r = json_of(send(&app, "GET", "/api/ha/states?ids=light.desk", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"configured": false, "error": crate::home_assistant::NOT_CONFIGURED, "entities": []})
        );
        s.cfg
            .save(
                &json!({
                    "homeAssistant": {"url": server.url, "token": "secret"},
                    "pages": [{"id": "home", "widgets": [{"type": "ha", "options": {"entities": "light.desk", "control": true}}]}],
                })
                .to_string(),
                None,
            )
            .unwrap();
        let r = json_of(
            send(
                &app,
                "GET",
                "/api/ha/states?ids=,light.desk%20,,nope.x",
                &[LOCAL],
            )
            .await,
        )
        .await;
        assert_eq!(
            r,
            json!({"configured": true, "error": null, "entities": [
                {"id":"light.desk","state":"on","name":"Desk lamp","unit":null,"deviceClass":null,"icon":null,"changed":0}]})
        );
        let res = send(&app, "POST", "/api/ha/toggle/light.desk", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(json_of(res).await, json!({"ok": true}));
        let res = send(&app, "POST", "/api/ha/toggle/light.other", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            json_of(res).await,
            json!({"error": "switching this entity is not allowed in the widget's settings"})
        );
        let r = json_of(send(&app, "GET", "/api/ha/entities", &[LOCAL]).await).await;
        assert_eq!(
            r["entities"],
            json!([{"id": "light.desk", "name": "Desk lamp", "switchable": true}])
        );
        // Settings › Test: a typed JSON body, names in any case
        let body = json!({"URL": server.url, "Token": "secret"}).to_string();
        let res = post(&app, "/api/ha/test", "application/json", &body).await;
        assert_eq!(json_of(res).await, json!({"result": "Connected"}));
        let body = json!({"url": server.url, "token": "nope"}).to_string();
        let res = post(
            &app,
            "/api/ha/test",
            "application/json; charset=utf-8",
            &body,
        )
        .await;
        assert_eq!(
            json_of(res).await,
            json!({"result": "The token was rejected"})
        );
        assert_eq!(
            post(&app, "/api/ha/test", "text/plain", &body)
                .await
                .status(),
            415
        );
        assert_eq!(
            post(&app, "/api/ha/test", "application/json", "")
                .await
                .status(),
            400
        );
        assert_eq!(
            post(&app, "/api/ha/test", "application/json", "{ nope")
                .await
                .status(),
            400
        );
        assert_eq!(
            post(&app, "/api/ha/test", "application/json", r#"{"url":5}"#)
                .await
                .status(),
            400
        );
    }

    #[tokio::test]
    async fn only_the_phones_home_assistant_routes_are_served_over_wifi() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(
            send(&wifi, "GET", "/api/ha/states?ids=a.b", &[key])
                .await
                .status(),
            200
        );
        assert_eq!(
            send(&wifi, "POST", "/api/ha/toggle/a.b", &[key])
                .await
                .status(),
            400
        );
        assert_eq!(
            send(&wifi, "GET", "/api/ha/entities", &[key])
                .await
                .status(),
            403
        );
        assert_eq!(
            send(&wifi, "POST", "/api/ha/test", &[key]).await.status(),
            403
        );
    }
}
