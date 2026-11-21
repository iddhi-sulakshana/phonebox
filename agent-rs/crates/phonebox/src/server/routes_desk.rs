//! The desk routes (plan 3): volume, media, the spectrum, shortcuts and their icons, apps, dictation and the
//! trackpad. Each comes with its monitor.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use super::query::{int32, value};
use super::{AppState, json_response};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/volume", post(volume))
        .route("/api/run/{id}", post(run_shortcut))
        .route("/api/shortcuts/test", post(test_shortcut))
}

type QueryList = Query<Vec<(String, String)>>;

fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// `RunShortcutForPhone`: 200 empty, or a problem (500) whose detail is the Accessibility hint or the generic
/// sentence, never the raw error (it can carry configured command text).
async fn run_shortcut(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let (cfg, push, audio) = (s.cfg.get(), s.push.clone(), s.audio.clone());
    let result = tokio::task::spawn_blocking(move || {
        crate::shortcuts::run_for_phone(&cfg, &id, &push, &audio)
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(detail)) => problem(&detail),
        Err(_) => problem(crate::shortcuts::GENERIC_FAILURE),
    }
}

/// `Results.Problem(detail)`: ASP.NET's RFC 9110 problem document for a 500.
fn problem(detail: &str) -> Response {
    let body = serde_json::json!({
        "type": "https://tools.ietf.org/html/rfc9110#section-15.6.1",
        "title": "An error occurred while processing your request.",
        "status": 500,
        "detail": detail,
    });
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/problem+json")],
        crate::config::write::to_dotnet_json_compact(&body),
    )
        .into_response()
}

/// The settings page's "Test" button: runs a shortcut that is not saved yet and reports the raw error.
async fn test_shortcut(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let json_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json"));
    if !json_type {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    // malformed JSON is an unhandled exception in C#: a bare 500
    let Ok(sc) = serde_json::from_slice::<Option<crate::config::Shortcut>>(&body) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(sc) = sc.filter(|sc| !sc.cmd.trim().is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "command is empty" }),
        );
    };
    // the same pushed values and audio state as a phone run, so a macro's nested toggles pick the same command
    let (cfg, push, audio) = (s.cfg.get(), s.push.clone(), s.audio.clone());
    let err = tokio::task::spawn_blocking(move || {
        crate::shortcuts::execute(&sc, Some(&cfg), 0, Some(&push), Some(&audio))
    })
    .await
    .unwrap_or_else(|_| Some("the shortcut could not be run".into()));
    match err {
        None => StatusCode::OK.into_response(),
        Some(e) => json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": e })),
    }
}

/// `op=mute|up|down` (±5), else `level=N`; success pokes the state and answers the current reading (or `null`).
async fn volume(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(level) = int32(&q, "level") else {
        return bad_request();
    };
    let op = value(&q, "op");
    let audio = s.audio.clone();
    let result = tokio::task::spawn_blocking(move || {
        let ok = match (op.as_deref(), level) {
            (Some("mute"), _) => audio.toggle_mute(),
            (Some("up"), _) => audio.step(5),
            (Some("down"), _) => audio.step(-5),
            (_, Some(l)) => audio.set_volume(l),
            _ => false,
        };
        ok.then(|| audio.current())
    })
    .await;
    match result {
        Ok(Some(current)) => {
            crate::state_signal::poke(); // the phone's other screens see it now, not at the next second
            json_response(StatusCode::OK, &current)
        }
        _ => bad_request(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    use crate::audio_control::{AudioControl, AudioSnapshot};
    use crate::server::build_router;
    use crate::server::guard::tests::{send, test_state};

    async fn call(
        app: &crate::server::App,
        method: &str,
        path: &str,
    ) -> (StatusCode, String, Vec<u8>) {
        let res = send(app, method, path, &[("host", "127.0.0.1:8765")]).await;
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (
            status,
            kind,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }

    async fn post_body(
        app: &crate::server::App,
        path: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> (StatusCode, String, Vec<u8>) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:8765");
        if let Some(t) = content_type {
            req = req.header("content-type", t);
        }
        let res = app
            .clone()
            .oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (
            status,
            kind,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }

    #[tokio::test]
    async fn a_shortcut_run_answers_ok_or_a_problem_without_the_raw_error() {
        let (_dir, state) = test_state();
        let page = serde_json::json!({"shortcuts": [
            {"id": "ok", "label": "Ok", "cmd": "builtin:macro"},
            {"id": "bad", "label": "Bad", "cmd": "builtin:keys", "args": "ctrl+secret_key"}
        ]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let app = build_router(state);
        let (status, _, body) = call(&app, "POST", "/api/run/ok").await;
        assert_eq!((status, body.is_empty()), (StatusCode::OK, true));
        for path in ["/api/run/bad", "/api/run/phonebox-no-such"] {
            let (status, kind, body) = call(&app, "POST", path).await;
            assert_eq!(
                (status, kind.as_str()),
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "application/problem+json"
                ),
                "{path}"
            );
            let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                problem,
                serde_json::json!({
                    "type": "https://tools.ietf.org/html/rfc9110#section-15.6.1",
                    "title": "An error occurred while processing your request.",
                    "status": 500,
                    "detail": crate::shortcuts::GENERIC_FAILURE,
                }),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_settings_page_test_run_reports_the_raw_error() {
        let (_dir, state) = test_state();
        let app = build_router(state);
        let json = Some("application/json; charset=utf-8");
        assert_eq!(
            post_body(
                &app,
                "/api/shortcuts/test",
                None,
                r#"{"cmd":"builtin:macro"}"#
            )
            .await
            .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            post_body(&app, "/api/shortcuts/test", Some("text/plain"), "{}")
                .await
                .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        for body in [r#"{"cmd":"  "}"#, "null", "{}"] {
            let (status, _, out) = post_body(&app, "/api/shortcuts/test", json, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
                serde_json::json!({"error": "command is empty"})
            );
        }
        let (status, _, out) = post_body(
            &app,
            "/api/shortcuts/test",
            json,
            r#"{"cmd":"builtin:keys","args":"ctrl+zzz"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"error": "unknown key \"zzz\""})
        );
        let (status, _, out) = post_body(
            &app,
            "/api/shortcuts/test",
            json,
            r#"{"cmd":"builtin:macro","steps":["wait:1"]}"#,
        )
        .await;
        assert_eq!((status, out.is_empty()), (StatusCode::OK, true));
        assert_eq!(
            post_body(&app, "/api/shortcuts/test", json, "{not json")
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn toggles_show_the_toggle_keys_by_id() {
        // ids are normalized on load (lower case, dashes), and the map keys are those ids as they are
        let (_dir, state) = test_state();
        let page = serde_json::json!({"shortcuts": [
            {"id": "lamp-b", "label": "Lamp", "cmd": "builtin:macro", "toggle": true},
            {"id": "plain", "label": "Plain", "cmd": "builtin:macro"}
        ]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let toggles = crate::shortcuts::Toggles {
            cfg: state.cfg.clone(),
            push: state.push.clone(),
            audio: state.audio.clone(),
        };
        assert_eq!(
            crate::state::StateSource::snapshot(&toggles),
            serde_json::json!({"lamp-b": false})
        );
        let app = build_router(state);
        assert_eq!(
            call(&app, "POST", "/api/run/lamp-b").await.0,
            StatusCode::OK
        );
        assert_eq!(
            crate::state::StateSource::snapshot(&toggles),
            serde_json::json!({"lamp-b": true})
        );
    }

    #[tokio::test]
    async fn volume_follows_op_and_level_and_answers_the_current_reading() {
        let (_dir, mut state) = test_state();
        let device = Arc::new(Mutex::new(AudioSnapshot::new(50, false, true)));
        let (d1, d2, d3) = (device.clone(), device.clone(), device.clone());
        state.audio = AudioControl::with(
            Arc::new(move || Some(d1.lock().unwrap().clone())),
            Some(Arc::new(move |level| {
                d2.lock().unwrap().volume = level;
                Some(level)
            })),
            Some(Arc::new(move || {
                let mut d = d3.lock().unwrap();
                d.muted = !d.muted;
                true
            })),
        );
        let app = build_router(state);
        let json = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();

        let (status, kind, body) = call(&app, "POST", "/api/volume?op=up").await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::OK, "application/json; charset=utf-8")
        );
        assert_eq!(
            json(&body),
            serde_json::json!({"volume": 55, "muted": false, "canControl": true})
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=down").await.2)["volume"],
            50
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?level=30").await.2)["volume"],
            30
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?level=130").await.2)["volume"],
            100
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=mute").await.2)["muted"],
            true
        );
        // op wins over level; an unknown op falls back to the level
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=up&level=10").await.2)["volume"],
            100
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=loud&level=10").await.2)["volume"],
            10
        );

        let (status, _, body) = call(&app, "POST", "/api/volume").await;
        assert_eq!((status, body.is_empty()), (StatusCode::BAD_REQUEST, true));
        assert_eq!(
            call(&app, "POST", "/api/volume?level=x").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "POST", "/api/volume?op=MUTE").await.0,
            StatusCode::BAD_REQUEST,
            "ops are case-sensitive"
        );
    }

    #[tokio::test]
    async fn a_failed_volume_change_is_a_400_and_the_state_carries_audio() {
        let (_dir, mut state) = test_state();
        state.audio = AudioControl::with(
            Arc::new(|| Some(AudioSnapshot::new(20, true, true))),
            Some(Arc::new(|_| None)),
            None,
        );
        let audio = state.audio.clone();
        let app = build_router(state);
        assert_eq!(
            call(&app, "POST", "/api/volume?level=40").await.0,
            StatusCode::BAD_REQUEST
        );
        audio.current();
        let v = crate::state::StateSource::snapshot(&*audio);
        assert_eq!(
            v,
            serde_json::json!({"volume": 20, "muted": true, "canControl": true})
        );
    }
}
