//! The desk routes (plan 3): volume, media, the spectrum, shortcuts and their icons, apps, dictation and the
//! trackpad. Each comes with its monitor.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::query::{boolean, int32, value};
use super::{AppState, ICON_COMMIT, json_response};
use crate::config::store::{CONFLICT, ConfigStore};
use crate::shortcut_icon_uploads::ShortcutIconUploads;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/volume", post(volume))
        .route("/api/run/{id}", post(run_shortcut))
        .route("/api/shortcuts/test", post(test_shortcut))
        .route("/api/shortcuts/icon", get(shortcut_icon))
        .route(
            "/api/settings/shortcut-icon",
            post(upload_icon).delete(remove_icon),
        )
        .route("/api/apps", get(apps))
        .route("/api/apps/icon", get(app_icon))
        .route("/api/apps/open", post(open_app))
}

/// The installed apps for the All apps widget, and whether the phone may open them.
async fn apps(State(s): State<AppState>) -> Response {
    let catalog = s.apps.clone();
    let list = tokio::task::spawn_blocking(move || {
        let apps = catalog.list();
        let supported = catalog.supported();
        json!({
            "supported": supported,
            "canOpen": supported && catalog.open_allowed(),
            "apps": apps.iter().map(|a| json!({ "id": a.id, "name": a.name })).collect::<Vec<_>>(),
        })
    })
    .await;
    match list {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// An app's icon (96 px PNG); icons do not change, so the phone may keep them a day.
async fn app_icon(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(id) = value(&q, "id") else {
        return bad_request();
    };
    match s.apps.icon(&id).await {
        Some(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            png,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn open_app(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(id) = value(&q, "id") else {
        return bad_request();
    };
    let catalog = s.apps.clone();
    match tokio::task::spawn_blocking(move || catalog.open(&id)).await {
        Ok(None) => json_response(StatusCode::OK, &json!({ "ok": true })),
        Ok(Some(error)) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
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

/// A request header as ASP.NET reads one into a string: several values joined with ",".
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(","))
}

fn conflict() -> Response {
    json_response(StatusCode::CONFLICT, &json!({ "error": CONFLICT }))
}

fn commit_lock() -> std::sync::MutexGuard<'static, ()> {
    ICON_COMMIT.lock().unwrap_or_else(|e| e.into_inner())
}

/// A shortcut's picture: an image the user uploaded (settings page) first, else its program's own icon when its
/// command is an actual file on disk. 404 when neither exists, so the phone shows the emoji. `upload=true`: the upload
/// only (see [`crate::shortcut_icons::picture`]). Served so a browser never runs an SVG's script or sniffs a type.
async fn shortcut_icon(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let (Some(id), Ok(upload)) = (value(&q, "id"), boolean(&q, "upload")) else {
        return bad_request();
    };
    let Some(sc) = s.cfg.get().shortcuts.iter().find(|x| x.id == id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let icons = s.icons.clone();
    let found = tokio::task::spawn_blocking(move || {
        crate::shortcut_icons::picture(&sc, &icons, upload == Some(true))
    })
    .await
    .ok()
    .flatten();
    let Some((bytes, content_type)) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

/// Uploads a custom icon for a shortcut (PNG, WEBP or SVG, 512 KB max), replacing any earlier one. Under
/// `/api/settings`, so local-only like the rest of the settings API: over Wi-Fi the phone only ever reads an icon.
async fn upload_icon(
    State(s): State<AppState>,
    Query(q): QueryList,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let Some(id) = value(&q, "id") else {
        return bad_request();
    };
    let version = header_text(&headers, "x-config-version");
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    upload_shortcut_icon(
        &id,
        version.as_deref(),
        body,
        content_type,
        &s.cfg,
        &s.icons,
    )
    .await
}

/// Removes a shortcut's uploaded icon; it falls back to its program's own icon, or the emoji.
async fn remove_icon(
    State(s): State<AppState>,
    Query(q): QueryList,
    headers: HeaderMap,
) -> Response {
    let Some(id) = value(&q, "id") else {
        return bad_request();
    };
    let version = header_text(&headers, "x-config-version");
    let _commit = commit_lock();
    if !s.cfg.get().shortcuts.iter().any(|x| x.id == id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if s.cfg.version_conflict(version.as_deref()) {
        return conflict();
    }
    s.icons.remove(&id);
    s.cfg.touch();
    json_response(StatusCode::OK, &json!({ "version": s.cfg.version_text() }))
}

/// `UploadShortcutIconAsync`: the checks, then the body (read outside the lock), then the checks again, the write and
/// the version bump as one step under [`ICON_COMMIT`]: of two uploads that started from the same version the second
/// gets 409 instead of replacing the first one's icon, and a settings save that deleted the shortcut meanwhile wins.
pub(crate) async fn upload_shortcut_icon(
    id: &str,
    config_version: Option<&str>,
    body: axum::body::Body,
    content_type: Option<&str>,
    cfg: &ConfigStore,
    icons: &ShortcutIconUploads,
) -> Response {
    let unknown = || {
        json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": "Unknown shortcut (save it once first)" }),
        )
    };
    let known = || cfg.get().shortcuts.iter().any(|s| s.id == id);
    if !known() {
        return unknown();
    }
    if cfg.version_conflict(config_version) {
        return conflict();
    }
    // a body that breaks off is Kestrel's "unexpected end of request content", a 400
    let Ok(bytes) = ShortcutIconUploads::read_bounded(body).await else {
        return bad_request();
    };
    let _commit = commit_lock();
    if cfg.version_conflict(config_version) {
        return conflict();
    }
    if !known() {
        return unknown();
    }
    match icons.save(id, content_type, bytes.as_deref()) {
        Err(e) => {
            // the C# agent throws here: an unhandled exception, a 500 with no body
            crate::log::warn("Program", &format!("saving the icon failed: {e}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Ok(Some(error)) => {
            return json_response(StatusCode::BAD_REQUEST, &json!({ "error": error }));
        }
        Ok(None) => {}
    }
    if !known() {
        icons.cleanup(|| {
            cfg.get()
                .shortcuts
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>()
        });
        return if cfg.version_conflict(config_version) {
            conflict()
        } else {
            unknown()
        };
    }
    cfg.touch();
    json_response(StatusCode::OK, &json!({ "version": cfg.version_text() }))
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

    // ---- shortcut icons ----

    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    struct Sent {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    impl Sent {
        fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).unwrap()
        }
        fn header(&self, name: &str) -> &str {
            self.headers
                .get(name)
                .map(|v| v.to_str().unwrap())
                .unwrap_or("")
        }
    }

    async fn request(
        app: &crate::server::App,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Sent {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8765");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = app
            .clone()
            .oneshot(req.body(axum::body::Body::from(body)).unwrap())
            .await
            .unwrap();
        Sent {
            status: res.status(),
            headers: res.headers().clone(),
            body: res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        }
    }

    fn shortcut_of(config: &serde_json::Value, id: &str) -> serde_json::Value {
        config["config"]["shortcuts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn an_uploaded_icon_is_served_reported_to_the_phone_and_removed() {
        let (_dir, state) = test_state();
        let page =
            serde_json::json!({"shortcuts": [{"id": "sc", "label": "Sc", "cmd": "builtin:mute"}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let (cfg, icons) = (state.cfg.clone(), state.icons.clone());
        let app = build_router(state);

        let none = request(&app, "GET", "/api/shortcuts/icon?id=sc", &[], vec![]).await;
        assert_eq!((none.status, none.body.len()), (StatusCode::NOT_FOUND, 0));
        let config = request(&app, "GET", "/api/config", &[], vec![])
            .await
            .json();
        assert_eq!(
            shortcut_of(&config, "sc")["iconRev"],
            serde_json::Value::Null
        );
        assert_eq!(shortcut_of(&config, "sc")["iconUpload"], false);

        let before = cfg.version();
        let version = cfg.version_text();
        let up = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            &[
                ("content-type", "image/png"),
                ("x-config-version", &version),
            ],
            PNG_1PX.to_vec(),
        )
        .await;
        assert_eq!(up.status, StatusCode::OK);
        assert_eq!(
            up.json(),
            serde_json::json!({"version": cfg.version_text()})
        );
        assert_ne!(cfg.version(), before, "other tabs must notice the upload");

        for path in [
            "/api/shortcuts/icon?id=sc",
            "/api/shortcuts/icon?id=sc&upload=true",
            "/api/shortcuts/icon?ID=sc&Upload=False",
        ] {
            let got = request(&app, "GET", path, &[], vec![]).await;
            assert_eq!(got.status, StatusCode::OK, "{path}");
            assert_eq!(got.body, PNG_1PX);
            assert_eq!(got.header("content-type"), "image/png");
            assert_eq!(
                got.header("content-security-policy"),
                "default-src 'none'; style-src 'unsafe-inline'; sandbox"
            );
            assert_eq!(got.header("x-content-type-options"), "nosniff");
        }
        let config = request(&app, "GET", "/api/config", &[], vec![])
            .await
            .json();
        assert_eq!(shortcut_of(&config, "sc")["iconUpload"], true);
        assert_eq!(
            shortcut_of(&config, "sc")["iconRev"],
            serde_json::json!(icons.revision("sc").unwrap())
        );

        let gone = request(
            &app,
            "DELETE",
            "/api/settings/shortcut-icon?id=sc",
            &[],
            vec![],
        )
        .await;
        assert_eq!(gone.status, StatusCode::OK);
        assert_eq!(
            gone.json(),
            serde_json::json!({"version": cfg.version_text()})
        );
        assert!(!icons.exists("sc"));
        let none = request(&app, "GET", "/api/shortcuts/icon?id=sc", &[], vec![]).await;
        assert_eq!(none.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn icon_uploads_refuse_unknown_shortcuts_stale_pages_and_bad_files() {
        let (_dir, state) = test_state();
        let page =
            serde_json::json!({"shortcuts": [{"id": "sc", "label": "Sc", "cmd": "builtin:mute"}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let icons = state.icons.clone();
        let app = build_router(state);
        let png = &[("content-type", "image/png")];

        let unknown = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=nope",
            png,
            PNG_1PX.to_vec(),
        )
        .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(
            unknown.json(),
            serde_json::json!({"error": "Unknown shortcut (save it once first)"})
        );
        let unknown = request(
            &app,
            "DELETE",
            "/api/settings/shortcut-icon?id=nope",
            &[],
            vec![],
        )
        .await;
        assert_eq!(
            (unknown.status, unknown.body.len()),
            (StatusCode::NOT_FOUND, 0)
        );

        let stale = [("content-type", "image/png"), ("x-config-version", "12345")];
        for method in ["POST", "DELETE"] {
            let r = request(
                &app,
                method,
                "/api/settings/shortcut-icon?id=sc",
                &stale,
                PNG_1PX.to_vec(),
            )
            .await;
            assert_eq!(r.status, StatusCode::CONFLICT, "{method}");
            assert_eq!(
                r.json(),
                serde_json::json!({"error": crate::config::store::CONFLICT})
            );
        }
        // a version that is not a number never conflicts (scripts)
        let r = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            &[("content-type", "image/png"), ("x-config-version", "x")],
            PNG_1PX.to_vec(),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);

        let gif = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            &[("content-type", "image/gif")],
            vec![1, 2, 3],
        )
        .await;
        assert_eq!(gif.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            gif.json(),
            serde_json::json!({"error": "Only PNG, WEBP or SVG icons are supported"})
        );
        let big = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            png,
            vec![0x89; crate::shortcut_icon_uploads::MAX_BYTES + 1],
        )
        .await;
        assert_eq!(
            big.json(),
            serde_json::json!({"error": "The icon is larger than 512 KB"})
        );
        assert_eq!(
            icons.read("sc").unwrap().0,
            PNG_1PX,
            "a refused upload keeps the icon"
        );

        // ASP.NET's binding: a missing id, or an upload flag that is not a bool, is a 400
        for (method, path) in [
            ("GET", "/api/shortcuts/icon"),
            ("GET", "/api/shortcuts/icon?id=sc&upload=maybe"),
            ("GET", "/api/shortcuts/icon?id=sc&upload="),
            ("POST", "/api/settings/shortcut-icon"),
            ("DELETE", "/api/settings/shortcut-icon"),
        ] {
            let r = request(&app, method, path, png, PNG_1PX.to_vec()).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{method} {path}");
        }
        let r = request(
            &app,
            "GET",
            "/api/shortcuts/icon?id=sc&upload=%20TRUE%20",
            &[],
            vec![],
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let r = request(&app, "GET", "/api/shortcuts/icon?id=nope", &[], vec![]).await;
        assert_eq!((r.status, r.body.len()), (StatusCode::NOT_FOUND, 0));
    }

    #[tokio::test]
    async fn a_failed_move_is_a_500_and_keeps_the_icon() {
        let (_dir, state) = test_state();
        let page =
            serde_json::json!({"shortcuts": [{"id": "sc", "label": "Sc", "cmd": "builtin:mute"}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let icons = state.icons.clone();
        let icons_dir = state.cfg.path().parent().unwrap().join("icons");
        let app = build_router(state);
        let r = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            &[("content-type", "image/png")],
            PNG_1PX.to_vec(),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        std::fs::create_dir(icons_dir.join("sc.svg")).unwrap();
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec();
        let r = request(
            &app,
            "POST",
            "/api/settings/shortcut-icon?id=sc",
            &[("content-type", "image/svg+xml")],
            svg,
        )
        .await;
        assert_eq!(
            (r.status, r.body.len()),
            (StatusCode::INTERNAL_SERVER_ERROR, 0)
        );
        assert_eq!(icons.read("sc").unwrap().1, "image/png");
    }

    #[tokio::test]
    async fn a_settings_save_drops_the_icons_of_deleted_shortcuts() {
        let (_dir, state) = test_state();
        let page = serde_json::json!({"shortcuts": [
            {"id": "keep-me", "label": "Keep", "cmd": "builtin:mute"},
            {"id": "temp-sc", "label": "Temp", "cmd": "builtin:mute"}
        ]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let icons = state.icons.clone();
        let app = build_router(state);
        for id in ["keep-me", "temp-sc"] {
            let r = request(
                &app,
                "POST",
                &format!("/api/settings/shortcut-icon?id={id}"),
                &[("content-type", "image/png")],
                PNG_1PX.to_vec(),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK);
        }
        let page = serde_json::json!({"shortcuts": [{"id": "keep-me", "label": "Keep", "cmd": "builtin:mute"}]});
        let r = request(
            &app,
            "PUT",
            "/api/settings",
            &[("content-type", "application/json")],
            page.to_string().into_bytes(),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(icons.exists("keep-me"));
        assert!(!icons.exists("temp-sc"));
    }

    /// The body is read outside the commit lock: a settings save that lands while it is read (deleting the shortcut)
    /// makes the upload a 409, and leaves no orphaned file behind.
    #[tokio::test]
    async fn shortcut_icon_upload_rejects_concurrent_settings_save_across_body_read_without_orphans()
     {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        let cfg = Arc::new(crate::config::store::ConfigStore::new(cfg_path.clone()));
        cfg.save(
            r#"{"shortcuts":[{"id":"keep-me","label":"Keep","cmd":"builtin:mute"},{"id":"temp-sc","label":"Temp","cmd":"builtin:mute"}]}"#,
            None,
        )
        .unwrap();
        let icons = Arc::new(crate::shortcut_icon_uploads::ShortcutIconUploads::new(
            dir.path().join("icons"),
        ));

        let v1 = cfg.version_text();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let body = axum::body::Body::from_stream(futures_util::stream::once(async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(PNG_1PX))
        }));
        let upload = {
            let (cfg, icons) = (cfg.clone(), icons.clone());
            tokio::spawn(async move {
                super::upload_shortcut_icon(
                    "temp-sc",
                    Some(&v1),
                    body,
                    Some("image/png"),
                    &cfg,
                    &icons,
                )
                .await
            })
        };
        // an upload that returns before reading the body fails here instead of hanging the test
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .expect("the upload read the body")
            .expect("the upload read the body");

        // move the file's time back so the save below gets a new version, and delete temp-sc while the body is read
        let back = std::time::UNIX_EPOCH
            + std::time::Duration::from_nanos(
                ((cfg.version() - crate::config::store::UNIX_EPOCH_TICKS) * 100) as u64,
            )
            - std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&cfg_path)
            .unwrap()
            .set_modified(back)
            .unwrap();
        let _ = cfg.get();
        let saved_v2 = cfg
            .save(
                r#"{"shortcuts":[{"id":"keep-me","label":"Keep","cmd":"builtin:mute"}]}"#,
                Some(cfg.version()),
            )
            .unwrap();
        icons.cleanup(|| {
            cfg.get()
                .shortcuts
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>()
        });

        let _ = release_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), upload)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status(), StatusCode::CONFLICT);
        assert!(!icons.exists("temp-sc"));
        assert_eq!(saved_v2, cfg.version());
    }

    // ---- apps ----

    fn apps_state(widget: &str) -> (tempfile::TempDir, crate::server::AppState, String) {
        let (dir, mut state) = test_state();
        let page = serde_json::json!({"pages": [{"id": "p", "title": "P",
            "widgets": [{"id": "w", "type": widget, "x": 0, "y": 0, "w": 6, "h": 4}]}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let root = dir.path().join("Applications");
        std::fs::create_dir_all(root.join("Notes.app").join("Contents")).unwrap();
        let notes = root.join("Notes.app").to_string_lossy().into_owned();
        state.apps = crate::app_catalog::test_catalog(
            state.cfg.clone(),
            vec![root.to_string_lossy().into_owned()],
            Arc::new(|_| Some(vec![0x89, b'P', b'N', b'G'])),
        );
        (dir, state, notes)
    }

    #[tokio::test]
    async fn the_app_list_carries_support_the_open_permission_and_ids_with_names() {
        let (_dir, state, notes) = apps_state("apps");
        let app = build_router(state);
        let r = request(&app, "GET", "/api/apps", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            r.json(),
            serde_json::json!({"supported": true, "canOpen": true,
                "apps": [{"id": crate::app_catalog::id_of(&notes), "name": "Notes"}]})
        );
        let (_dir, state, _) = apps_state("clock");
        let app = build_router(state);
        let r = request(&app, "GET", "/api/apps", &[], vec![]).await;
        assert_eq!(r.json()["canOpen"], false);
    }

    #[tokio::test]
    async fn app_icons_are_cached_by_the_phone_and_unknown_ones_are_404() {
        let (_dir, state, notes) = apps_state("apps");
        let app = build_router(state);
        let id = crate::app_catalog::id_of(&notes);
        let r = request(&app, "GET", &format!("/api/apps/icon?id={id}"), &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.body, [0x89, b'P', b'N', b'G']);
        assert_eq!(r.header("content-type"), "image/png");
        assert_eq!(r.header("cache-control"), "private, max-age=86400");
        let r = request(
            &app,
            "GET",
            "/api/apps/icon?id=0123456789abcdef",
            &[],
            vec![],
        )
        .await;
        assert_eq!((r.status, r.body.len()), (StatusCode::NOT_FOUND, 0));
        for (method, path) in [("GET", "/api/apps/icon"), ("POST", "/api/apps/open")] {
            let r = request(&app, method, path, &[], vec![]).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{path}");
        }
    }

    #[tokio::test]
    async fn opening_an_app_is_refused_with_the_reason() {
        let (_dir, state, notes) = apps_state("clock");
        let app = build_router(state);
        let id = crate::app_catalog::id_of(&notes);
        let r = request(
            &app,
            "POST",
            &format!("/api/apps/open?id={id}"),
            &[],
            vec![],
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            r.json(),
            serde_json::json!({"error": "Add an All apps widget to a page to open apps from the phone"})
        );
        let (_dir, state, _) = apps_state("apps");
        let app = build_router(state);
        let r = request(
            &app,
            "POST",
            "/api/apps/open?id=0123456789abcdef",
            &[],
            vec![],
        )
        .await;
        assert_eq!(r.json(), serde_json::json!({"error": "Unknown app"}));
    }
}
