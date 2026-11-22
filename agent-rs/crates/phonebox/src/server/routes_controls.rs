//! The Control Deck routes (`Program.cs:90-130`); the deck's own routes join these with the deck.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{AppState, json_response, query};
use crate::control_deck::core::DeckCommand;
use crate::control_deck::radio_directory::CheckItem;
use axum::http::HeaderMap;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/controls/state", get(state))
        .route("/api/controls/command/{kind}", post(command))
        .route("/api/controls/capture", post(capture))
        .route("/api/controls/listen", get(listen))
        .route("/api/controls/radio-logo", get(radio_logo))
        .route(
            "/api/controls/radio-directory/countries",
            get(radio_countries),
        )
        .route(
            "/api/controls/radio-directory/stations",
            get(radio_stations),
        )
        .route("/api/controls/radio-directory/check", post(radio_check))
        .route("/api/controls/task-output/{id}", get(task_output))
}

/// What the controls page shows: audio, media, hardware, the configured controls, profiles and timers.
async fn state(State(s): State<AppState>) -> Response {
    match s.controls.snapshot().await {
        Some(snapshot) => json_response(StatusCode::OK, &snapshot),
        None => super::unhandled(),
    }
}

/// One control: `id`, `op`, `value`, `muted` and `lease` in the query. A dropped request stops what has not run yet.
async fn command(
    State(s): State<AppState>,
    Path(kind): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> Response {
    let (Ok(value), Ok(muted)) = (query::int32(&q, "value"), query::boolean(&q, "muted")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let command = DeckCommand {
        kind,
        id: query::value(&q, "id"),
        op: query::value(&q, "op"),
        value,
        muted,
        lease: query::value(&q, "lease"),
    };
    match s
        .controls
        .act(command, &crate::server::Stop::default())
        .await
    {
        Ok(result) => json_response(StatusCode::OK, &result),
        // only the request's own end cancels, and then nobody reads the answer
        Err(_) => super::unhandled(),
    }
}

/// The computer's sound as NDJSON (Windows, when the settings allow it); 403 or 429 with an empty body otherwise.
async fn listen(State(s): State<AppState>) -> Response {
    use futures_util::StreamExt;
    match s.controls.listen() {
        Err(status) => StatusCode::from_u16(status)
            .unwrap_or(StatusCode::FORBIDDEN)
            .into_response(),
        Ok(lines) => (
            [(header::CONTENT_TYPE, "application/x-ndjson")],
            axum::body::Body::from_stream(lines.map(Ok::<_, std::convert::Infallible>)),
        )
            .into_response(),
    }
}

/// A radio control's logo (also over Wi-Fi): a raster cached for an hour, or the monogram SVG never cached; 404 for
/// an id that is not a radio.
async fn radio_logo(State(s): State<AppState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let id = query::value(&q, "id").unwrap_or_default();
    let Some(logo) = s.controls.radio_logo(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if logo.is_fallback {
        "no-store"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CACHE_CONTROL, cache.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'".to_string(),
            ),
            (header::CONTENT_TYPE, logo.mime.clone()),
        ],
        logo.bytes.to_vec(),
    )
        .into_response()
}

/// The directory's countries (local only); 502 `{"ok":false,"error":…}` when no mirror answered.
async fn radio_countries(State(s): State<AppState>) -> Response {
    directory_answer(s.radio.countries().await.map(|c| json!({ "countries": c })))
}

/// A country's popular stations (`country`, `limit`; local only).
async fn radio_stations(
    State(s): State<AppState>,
    Query(q): Query<Vec<(String, String)>>,
) -> Response {
    let Ok(limit) = query::int32(&q, "limit") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let country = query::value(&q, "country").unwrap_or_default();
    let stations = s.radio.stations(&country, limit.unwrap_or(40)).await;
    directory_answer(stations.map(|st| json!({ "stations": st })))
}

fn directory_answer(result: Result<serde_json::Value, String>) -> Response {
    match result {
        Ok(body) => json_response(StatusCode::OK, &body),
        Err(error) => json_response(
            StatusCode::BAD_GATEWAY,
            &json!({ "ok": false, "error": error }),
        ),
    }
}

/// Checks the settings page's stream addresses: `{"stations":[{"id","streamUrl"}]}` (member names in any case) →
/// `{"results":[{"id","ok","status"}]}`. A body that is not JSON, or not of that shape, is a 500 as in C#.
async fn radio_check(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let json_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.split(';').next())
        .map(|t| t.trim().to_ascii_lowercase())
        .is_some_and(|t| {
            t == "application/json" || (t.starts_with("application/") && t.ends_with("+json"))
        });
    let items = json_type
        .then(|| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .flatten()
        .and_then(|v| bind_check(&v));
    let Some(items) = items else {
        return super::unhandled();
    };
    let results = s.radio.check(items).await;
    json_response(StatusCode::OK, &json!({ "results": results }))
}

/// `ReadFromJsonAsync<RadioCheckRequest>` with the web defaults: None where System.Text.Json would throw.
fn bind_check(body: &serde_json::Value) -> Option<Vec<Option<CheckItem>>> {
    use serde_json::Value;
    let member = |object: &serde_json::Map<String, Value>, name: &str| {
        object
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
            .next_back()
            .cloned()
    };
    let text = |value: Option<Value>| match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s)),
        Some(_) => None,
    };
    let object = match body {
        Value::Null => return Some(vec![]),
        Value::Object(object) => object,
        _ => return None,
    };
    match member(object, "stations") {
        None | Some(Value::Null) => Some(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => Some(None),
                Value::Object(o) => Some(Some(CheckItem {
                    id: text(member(o, "id"))?,
                    stream_url: text(member(o, "streamUrl"))?,
                })),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

/// A profile draft from the current audio for the desktop editor (local only); 500 when audio is unavailable.
async fn capture(State(s): State<AppState>) -> Response {
    let controls = s.controls.clone();
    match tokio::task::spawn_blocking(move || controls.capture()).await {
        Ok(Some(profile)) => json_response(StatusCode::OK, &profile),
        _ => super::unhandled(),
    }
}

/// The last output of a deck task's program, as plain text (local only).
async fn task_output(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain")], s.tasks.output(&id)).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn text(res: axum::response::Response) -> String {
        String::from_utf8(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn the_controls_state_commands_and_capture_answer_as_the_phone_expects() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s);
        let res = send(&app, "GET", "/api/controls/state", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["cache-control"], "no-store");
        let state: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
        assert_eq!(state["platform"], crate::app_paths::os());
        assert_eq!(
            state["audio"],
            serde_json::json!({"available":false,"error":"Audio backend is unavailable","devices":[],"sessions":[],"defaults":null})
        );
        assert_eq!(state["actions"], serde_json::json!([]));

        let res = send(&app, "POST", "/api/controls/command/foo", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(
            text(res).await,
            r#"{"ok":false,"error":"Unknown control action","lease":null}"#
        );
        let res = send(
            &app,
            "POST",
            "/api/controls/command/power-arm?id=sleep",
            &[LOCAL],
        )
        .await;
        assert_eq!(
            text(res).await,
            r#"{"ok":false,"error":"Enable power controls in desktop settings","lease":null}"#
        );
        let res = send(&app, "POST", "/api/controls/command/duck?op=off", &[LOCAL]).await;
        assert_eq!(text(res).await, r#"{"ok":true,"error":null,"lease":null}"#);
        for bad in ["device?id=x&value=abc", "device?id=x&muted=maybe"] {
            let res = send(
                &app,
                "POST",
                &format!("/api/controls/command/{bad}"),
                &[LOCAL],
            )
            .await;
            assert_eq!(res.status(), 400, "{bad}");
            assert_eq!(text(res).await, "");
        }

        // radio logos: 404 without a radio control of that id
        for path in [
            "/api/controls/radio-logo",
            "/api/controls/radio-logo?id=nope",
        ] {
            let res = send(&app, "GET", path, &[LOCAL]).await;
            assert_eq!(res.status(), 404, "{path}");
            assert_eq!(text(res).await, "");
        }

        // listening is off unless the settings allow it (and on Windows only)
        let res = send(&app, "GET", "/api/controls/listen", &[LOCAL]).await;
        assert_eq!(res.status(), 403);
        assert_eq!(text(res).await, "");

        // no audio here: the capture fails as the C# agent's unhandled error did
        let res = send(&app, "POST", "/api/controls/capture", &[LOCAL]).await;
        assert_eq!(res.status(), 500);
        assert_eq!(text(res).await, "");
    }

    #[tokio::test]
    async fn the_radio_directory_routes_check_without_the_network_and_refuse_bad_bodies() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s);
        let check = |body: &'static str, kind: &'static str| {
            let app = app.clone();
            async move {
                let req = axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/controls/radio-directory/check")
                    .header("host", "127.0.0.1:8765")
                    .header("content-type", kind)
                    .body(axum::body::Body::from(body))
                    .unwrap();
                tower::ServiceExt::oneshot(app, req).await.unwrap()
            }
        };
        let res = check(
            r#"{"Stations":[{"ID":"a","streamurl":"ftp://x"},{"id":"b","streamUrl":"http://127.0.0.1/"},null]}"#,
            "application/json; charset=utf-8",
        )
        .await;
        assert_eq!(res.status(), 200);
        assert_eq!(
            text(res).await,
            r#"{"results":[{"id":"a","ok":false,"status":"Invalid URL"},{"id":"b","ok":false,"status":"local address, not checked"},{"id":"","ok":false,"status":"Invalid URL"}]}"#
        );
        for (body, kind) in [("null", "application/json"), ("{}", "application/json")] {
            assert_eq!(text(check(body, kind).await).await, r#"{"results":[]}"#);
        }
        for (body, kind) in [
            ("{}", "text/plain"),
            ("{", "application/json"),
            (r#"{"stations":"x"}"#, "application/json"),
            (r#"{"stations":[{"id":1}]}"#, "application/json"),
        ] {
            let res = check(body, kind).await;
            assert_eq!(res.status(), 500, "{body} {kind}");
            assert_eq!(text(res).await, "");
        }
        let res = send(
            &app,
            "GET",
            "/api/controls/radio-directory/stations?country=1",
            &[LOCAL],
        )
        .await;
        assert_eq!(text(res).await, r#"{"stations":[]}"#);
        let res = send(
            &app,
            "GET",
            "/api/controls/radio-directory/stations?limit=x",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn a_tasks_output_is_plain_text_and_unknown_tasks_have_none() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s);
        let res = send(&app, "GET", "/api/controls/task-output/unknown", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "text/plain");
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"No task output");
    }
}
