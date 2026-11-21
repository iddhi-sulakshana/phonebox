//! The Control Deck routes (`Program.cs:90-130`); the deck's own routes join these with the deck.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{AppState, json_response, query};
use crate::control_deck::core::DeckCommand;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/controls/state", get(state))
        .route("/api/controls/command/{kind}", post(command))
        .route("/api/controls/capture", post(capture))
        .route("/api/controls/listen", get(listen))
        .route("/api/controls/task-output/{id}", get(task_output))
}

/// What the controls page shows: audio, media, hardware, the configured controls, profiles and timers.
async fn state(State(s): State<AppState>) -> Response {
    match s.controls.snapshot().await {
        Some(snapshot) => json_response(StatusCode::OK, &snapshot),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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

/// A profile draft from the current audio for the desktop editor (local only); 500 when audio is unavailable.
async fn capture(State(s): State<AppState>) -> Response {
    let controls = s.controls.clone();
    match tokio::task::spawn_blocking(move || controls.capture()).await {
        Ok(Some(profile)) => json_response(StatusCode::OK, &profile),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
