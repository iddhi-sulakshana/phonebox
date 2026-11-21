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
        .route("/api/obs", get(obs))
        .route("/api/obs/{action}", post(obs_act))
        .route("/api/obs-test", post(obs_test))
        .route("/api/controls/obs", get(obs_deck))
        .route("/api/controls/obs/{action}", post(obs_deck_act))
        .route("/api/discord", get(discord))
        .route("/api/discord/img", get(discord_image))
        .route("/api/discord/{action}", post(discord_act))
}

/// The Discord widget: `{"enabled","snapshot","control"}`; buttons only if a widget allows control.
async fn discord(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.get();
    json_response(
        StatusCode::OK,
        &json!({ "enabled": cfg.discord.enabled, "snapshot": s.discord.snapshot(),
            "control": crate::discord_monitor::control_allowed(&cfg) }),
    )
}

/// Ids go in the query string: the Android app lets image requests through by exact path. All three are required.
async fn discord_image(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let (Some(kind), Some(id), Some(hash)) = (
        query::value(&q, "kind"),
        query::value(&q, "id"),
        query::value(&q, "hash"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match s.discord.image(&kind, &id, &hash).await {
        Some((bytes, mime)) => ([(axum::http::header::CONTENT_TYPE, mime)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn discord_act(
    State(s): State<AppState>,
    Path(action): Path<String>,
    Query(q): QueryList,
) -> Response {
    let channel = query::value(&q, "channel");
    match s.discord.act(&action, channel.as_deref()).await {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

/// The OBS widget: `{"enabled","snapshot","control"}`.
async fn obs(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.get();
    let control = cfg.widgets(true).iter().any(|w| {
        (w.r#type == "obs" || w.r#type.starts_with("obs-")) && w.bool("control") == Some(true)
    });
    json_response(
        StatusCode::OK,
        &json!({ "enabled": cfg.obs.enabled, "snapshot": s.obs.snapshot(), "control": control }),
    )
}

async fn obs_act(
    State(s): State<AppState>,
    Path(action): Path<String>,
    Query(q): QueryList,
) -> Response {
    let scene = query::value(&q, "scene");
    match s.obs.act(&action, scene.as_deref()).await {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

/// Settings › General › OBS › Test (local only: not under `/api/obs`, which the phone may reach over Wi-Fi).
async fn obs_test(headers: HeaderMap, body: Bytes) -> Response {
    let body = match query::json_object(&headers, &body) {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let (Ok(url), Ok(password)) = (
        query::json_string(&body, "url"),
        query::json_string(&body, "password"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // an absent url is ObsConfig's default
    let url = match query::json_field(&body, "url") {
        None => crate::obs_monitor::DEFAULT_URL,
        Some(_) => url.unwrap_or(""),
    };
    let result = crate::obs_monitor::test(url, password.unwrap_or("")).await;
    json_response(StatusCode::OK, &json!({ "result": result }))
}

async fn obs_deck(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.obs.deck_snapshot().await)
}

/// Always a `DeckResult`; a `value` or `enabled` that does not bind is a 400.
async fn obs_deck_act(
    State(s): State<AppState>,
    Path(action): Path<String>,
    Query(q): QueryList,
) -> Response {
    let (Ok(value), Ok(enabled)) = (query::int32(&q, "value"), query::boolean(&q, "enabled"))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let id = query::value(&q, "id");
    json_response(
        StatusCode::OK,
        &s.obs.deck_act(&action, id.as_deref(), value, enabled).await,
    )
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
        assert_eq!(send(&wifi, "GET", "/api/obs", &[key]).await.status(), 200);
        assert_eq!(
            send(&wifi, "GET", "/api/controls/obs", &[key])
                .await
                .status(),
            200
        );
        assert_eq!(
            send(&wifi, "POST", "/api/obs-test", &[key]).await.status(),
            403
        );
    }

    #[tokio::test]
    async fn the_discord_routes_answer_with_their_shapes() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/discord", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"enabled": false, "snapshot": {"connected": false, "error": null, "user": null, "muted": false,
                "deafened": false, "channelId": null, "channelName": null, "channelKind": null, "members": [],
                "channels": [], "notifications": []}, "control": false})
        );
        let res = send(&app, "POST", "/api/discord/mute", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            json_of(res).await,
            json!({"error": "controlling Discord is turned off in the widget's settings"})
        );
        // all three ids are required; an image the agent never reported is not fetched
        let res = send(&app, "GET", "/api/discord/img?kind=user&id=1", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert!(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        let res = send(
            &app,
            "GET",
            "/api/discord/img?kind=user&id=1&hash=",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 404);
        assert!(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_phone_reaches_the_discord_widget_over_wifi() {
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
            send(&wifi, "GET", "/api/discord", &[key]).await.status(),
            200
        );
        assert_eq!(
            send(
                &wifi,
                "GET",
                "/api/discord/img?kind=guild&id=1&hash=a",
                &[key]
            )
            .await
            .status(),
            404
        );
        assert_eq!(
            send(&wifi, "POST", "/api/discord/leave", &[key])
                .await
                .status(),
            400
        );
    }

    #[tokio::test]
    async fn the_obs_routes_answer_with_their_shapes() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/obs", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"enabled": false, "snapshot": {"connected": false, "error": null, "streaming": false, "streamMs": 0,
                "recording": false, "recordPaused": false, "recordMs": 0, "scene": null, "scenes": [], "micMuted": null,
                "micName": null}, "control": false})
        );
        let res = send(&app, "POST", "/api/obs/record", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            json_of(res).await,
            json!({"error": "controlling OBS is turned off in the widget's settings"})
        );
        let r = json_of(send(&app, "GET", "/api/controls/obs", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"connected": false, "error": "Configure the OBS connection in desktop settings"})
        );
        let r = json_of(send(&app, "POST", "/api/controls/obs/record", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"ok": false, "error": "Enable OBS control in the widget's desktop settings", "lease": null})
        );
        assert_eq!(
            send(&app, "POST", "/api/controls/obs/volume?value=x", &[LOCAL])
                .await
                .status(),
            400
        );
        assert_eq!(
            send(&app, "POST", "/api/controls/obs/mute?enabled=1", &[LOCAL])
                .await
                .status(),
            400
        );
        assert_eq!(
            post(&app, "/api/obs-test", "text/plain", "{}")
                .await
                .status(),
            415
        );
        assert_eq!(
            post(&app, "/api/obs-test", "application/json", "[]")
                .await
                .status(),
            400
        );
        // nothing listens on port 9
        let res = post(
            &app,
            "/api/obs-test",
            "application/json",
            r#"{"url":"ws://127.0.0.1:9"}"#,
        )
        .await;
        assert_eq!(
            json_of(res).await,
            json!({"result": "No answer: is OBS running with its WebSocket server on?"})
        );
        let res = post(
            &app,
            "/api/obs-test",
            "application/json",
            r#"{"URL":"http://127.0.0.1:9"}"#,
        )
        .await;
        assert_eq!(
            json_of(res).await,
            json!({"result": "Only Uris starting with 'ws://' or 'wss://' are supported."})
        );
        // the state frame's obs: null while not connected
        assert!(crate::state::StateSource::snapshot(s.obs.as_ref()).is_null());
    }
}
