//! The desk routes (plan 3): volume, media, the spectrum, shortcuts and their icons, apps, dictation and the
//! trackpad. Each comes with its monitor.

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
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
        .route("/api/trackpad", any(trackpad))
        .route("/api/dictate", post(dictate))
        .route("/api/media/{action}", post(media_action))
        .route("/api/art", get(art))
        .route("/api/spectrum", get(spectrum))
}

/// The Spectrum bars widget: each call keeps capture running for a few seconds and answers the latest bands (the live
/// stream also sends them as "spectrum" events; panels without the stream poll this instead). Only band levels leave
/// the agent, never audio; nothing is captured unless an enabled page has the widget.
async fn spectrum(State(s): State<AppState>) -> Response {
    let analyzer = s.spectrum.clone();
    let shown = tokio::task::spawn_blocking(move || analyzer.want())
        .await
        .unwrap_or(false);
    json_response(
        StatusCode::OK,
        &json!({
            "shown": shown,
            "active": s.spectrum.active(),
            "fps": crate::spectrum_analyzer::FPS,
            "b": if shown { s.spectrum.frame() } else { None },
        }),
    )
}

/// playpause, next, prev, seek (`ms`): a radio station handles it when one is the current media, else the system's
/// player; `{"ok":true,"media":…}` with what plays now, or 400. The phone's other screens see it at once.
async fn media_action(
    State(s): State<AppState>,
    Path(action): Path<String>,
    Query(q): QueryList,
) -> Response {
    let Ok(ms) = super::query::int64(&q, "ms") else {
        return bad_request();
    };
    let (media, deck) = (s.media.clone(), s.deck.clone());
    // the deck's media focus, held across the awaits: a radio start and this command never both win
    let gate = deck.media_focus();
    let focus = gate.lock().await;
    let system = media.snapshot();
    let outcome = if let Some(handled) = deck.try_control_radio(&action, system.as_ref()).await {
        (handled, deck.effective_media(media.snapshot()))
    } else {
        let starting = action == "next"
            || action == "prev"
            || (action == "playpause" && system.as_ref().map(|m| m.playing) != Some(true));
        let control = {
            let (media, action) = (media.clone(), action.clone());
            tokio::task::spawn_blocking(move || media.control(&action, ms))
        };
        let ok = control.await.unwrap_or(false);
        if ok && starting {
            // waits while a station opens: off the async workers
            let deck = deck.clone();
            let _ = tokio::task::spawn_blocking(move || deck.stop_all_radios()).await;
        }
        (ok, deck.effective_media(media.snapshot()))
    };
    drop(focus);
    crate::state_signal::poke();
    match outcome {
        (true, now) => json_response(StatusCode::OK, &json!({ "ok": true, "media": now })),
        _ => bad_request(),
    }
}

/// The current cover, or 404.
async fn art(State(s): State<AppState>) -> Response {
    match s.media.art() {
        Some(art) => (
            [(header::CONTENT_TYPE, art.mime.clone())],
            art.bytes.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The Dictation widget's text, typed into the app in front: `{"ok":true}`, or the reason with its status.
async fn dictate(State(s): State<AppState>, Query(q): QueryList, headers: HeaderMap) -> Response {
    let device = header_text(&headers, "x-phonebox-device").unwrap_or_default();
    let (widget, text) = (value(&q, "widget"), value(&q, "text"));
    let (status, error) = s
        .dictation
        .type_async(Some(&device), widget.as_deref(), text.as_deref())
        .await;
    match error {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &json!({ "error": error }),
        ),
    }
}

/// `TrackpadHandler.HandleEndpoint`: 403 unless allowed (before any upgrade), then the socket, else a POST body as one
/// payload (200 whatever it holds), else 405.
async fn trackpad(
    State(s): State<AppState>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: axum::extract::Request,
) -> Response {
    use crate::trackpad_handler as t;
    // fail closed: the widget must be on an enabled page with control on
    if !t::is_allowed(&s.cfg.get()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Ok(upgrade) = upgrade {
        let (cfg, stop) = (s.cfg.clone(), s.stop.clone());
        // a message over the limit closes the socket; the C# agent would read on, and ignore it
        return upgrade
            .max_message_size(1 << 20)
            .on_upgrade(move |socket| t::handle_socket(socket, cfg, stop));
    }
    // side effects need a POST with a JSON body; GET and other methods do nothing
    if request.method() != axum::http::Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let has_body = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    if has_body {
        let Ok(bytes) = axum::body::to_bytes(request.into_body(), 30_000_000).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let cfg = s.cfg.clone();
        let _ = tokio::task::spawn_blocking(move || t::process_payload(&bytes, &cfg)).await;
    }
    StatusCode::OK.into_response()
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
        Err(_) => super::unhandled(),
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
        Err(_) => super::unhandled(),
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
        return super::unhandled();
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
            return super::unhandled();
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
        // as a real client sends it
        if !body.is_empty() {
            req = req.header("content-length", body.len().to_string());
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

    // ---- trackpad ----

    fn trackpad_state(control: bool) -> (tempfile::TempDir, crate::server::AppState) {
        let (dir, state) = test_state();
        let page = serde_json::json!({"pages": [{"id": "p", "title": "P", "enabled": true,
            "widgets": [{"id": "t", "type": "trackpad", "x": 0, "y": 0, "w": 6, "h": 4,
                         "options": {"control": control}}]}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn handle_endpoint_returns403_when_service_missing_or_control_disabled() {
        let rec = crate::key_sender::MouseRecorder::start();
        let (_dir, state) = trackpad_state(false);
        let app = build_router(state);
        for method in ["POST", "GET", "PUT"] {
            let r = request(
                &app,
                method,
                "/api/trackpad",
                &[("content-type", "application/json")],
                br#"{"type":"move","dx":5,"dy":-2}"#.to_vec(),
            )
            .await;
            assert_eq!(
                (r.status, r.body.len()),
                (StatusCode::FORBIDDEN, 0),
                "{method}"
            );
        }
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn handle_endpoint_returns405_on_get() {
        let rec = crate::key_sender::MouseRecorder::start();
        let (_dir, state) = trackpad_state(true);
        let app = build_router(state);
        for method in ["GET", "PUT", "DELETE"] {
            let r = request(&app, method, "/api/trackpad?type=click", &[], vec![]).await;
            assert_eq!(
                (r.status, r.body.len()),
                (StatusCode::METHOD_NOT_ALLOWED, 0),
                "{method}"
            );
        }
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn handle_endpoint_executes_on_post_with_json() {
        let rec = crate::key_sender::MouseRecorder::start();
        let (_dir, state) = trackpad_state(true);
        let app = build_router(state);
        let r = request(
            &app,
            "POST",
            "/api/trackpad",
            &[("content-type", "application/json")],
            br#"{"type":"move","dx":5.5,"dy":-2.2}"#.to_vec(),
        )
        .await;
        assert_eq!((r.status, r.body.len()), (StatusCode::OK, 0));
        assert_eq!(
            rec.calls(),
            [crate::key_sender::MouseCall {
                action: "move".into(),
                dx: 6,
                dy: -2,
                delta: 0
            }]
        );
        // an empty or broken body is still a 200, and does nothing
        for body in [&b""[..], b"{nope", b"[1,2]"] {
            let r = request(&app, "POST", "/api/trackpad", &[], body.to_vec()).await;
            assert_eq!((r.status, r.body.len()), (StatusCode::OK, 0));
        }
        assert_eq!(rec.calls().len(), 1);
    }

    /// Review Focus 4: a socket that sends garbage (broken JSON, wrong kinds, binary frames, an oversized message) keeps
    /// working, and a button still held when it closes is released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flood_of_malformed_frames_is_ignored_and_a_held_button_is_released() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let rec = crate::key_sender::MouseRecorder::start();
        let (_dir, state) = trackpad_state(true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = state.stop.clone();
        let server = tokio::spawn(crate::server::serve(
            listener,
            state,
            std::time::Duration::from_secs(1),
        ));

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/trackpad"))
                .await
                .unwrap();
        let junk = [
            "{",
            "not json",
            "[]",
            "5",
            r#"{"type":5}"#,
            r#"{"dx":true}"#,
            "",
            r#"{"type":"teleport"}"#,
        ];
        for i in 0..400 {
            ws.send(Message::text(junk[i % junk.len()])).await.unwrap();
        }
        ws.send(Message::binary(vec![b'{'; 64])).await.unwrap();
        // larger than the C# agent's 1 KiB receive buffer: it sees two halves of broken JSON
        let oversized = format!(r#"{{"type":"click","pad":"{}"}}"#, "x".repeat(2000));
        ws.send(Message::text(oversized)).await.unwrap();
        ws.send(Message::text(r#"{"type":"down"}"#)).await.unwrap();
        ws.close(None).await.unwrap();
        while let Some(Ok(_)) = ws.next().await {}

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while rec.calls().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let actions: Vec<String> = rec.calls().into_iter().map(|c| c.action).collect();
        assert_eq!(actions, ["mouseDown", "mouseUp"]);
        stop.stop();
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_socket_is_refused_before_the_upgrade_when_control_is_off() {
        let (_dir, state) = trackpad_state(false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = state.stop.clone();
        let server = tokio::spawn(crate::server::serve(
            listener,
            state,
            std::time::Duration::from_secs(1),
        ));
        let err = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/trackpad"))
            .await
            .unwrap_err();
        match err {
            tokio_tungstenite::tungstenite::Error::Http(res) => assert_eq!(res.status(), 403),
            other => panic!("{other}"),
        }
        stop.stop();
        let _ = server.await;
    }

    // ---- dictation ----

    #[tokio::test]
    async fn dictation_types_for_a_widget_that_allows_it_and_says_why_not_otherwise() {
        let (_dir, mut state) = test_state();
        let page = serde_json::json!({"pages": [{"id": "p", "title": "P", "widgets": [
            {"id": "d", "type": "dictate", "x": 0, "y": 0, "w": 4, "h": 3, "options": {"control": true}},
            {"id": "off", "type": "dictate", "x": 4, "y": 0, "w": 4, "h": 3, "options": {"control": false}}
        ]}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        let typed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = typed.clone();
        state.dictation = crate::dictation::test_dictation(
            state.cfg.clone(),
            Arc::new(move |t| {
                sink.lock().unwrap().push(t.to_string());
                Ok(true)
            }),
        );
        let app = build_router(state);
        let device = [("x-phonebox-device", "desk-phone")];
        let r = request(
            &app,
            "POST",
            "/api/dictate?widget=p/d&text=hello%20there",
            &device,
            vec![],
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json(), serde_json::json!({"ok": true}));
        assert_eq!(*typed.lock().unwrap(), ["hello there "]);

        let r = request(
            &app,
            "POST",
            "/api/dictate?widget=p/off&text=x",
            &device,
            vec![],
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(
            r.json(),
            serde_json::json!({"error": "typing is off: turn on \"Type the text on the computer\" in the dictation widget's settings"})
        );
        let r = request(&app, "POST", "/api/dictate?widget=p/d", &device, vec![]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(r.json(), serde_json::json!({"error": "nothing to type"}));
        assert_eq!(typed.lock().unwrap().len(), 1);
    }

    // ---- media ----

    struct FakeMedia {
        playing: Mutex<bool>,
        calls: Mutex<Vec<String>>,
    }

    impl crate::media_monitor::SystemMedia for FakeMedia {
        fn snapshot(&self) -> Option<crate::media_monitor::MediaSnapshot> {
            Some(crate::media_monitor::MediaSnapshot::new(
                "Spotify",
                "One",
                "A",
                "B",
                *self.playing.lock().unwrap(),
                1000,
                200000,
                0,
                true,
            ))
        }
        fn art(&self) -> Option<Arc<crate::media_monitor::AlbumArt>> {
            Some(Arc::new(crate::media_monitor::AlbumArt {
                bytes: vec![0xff, 0xd8, 0xff],
                mime: "image/jpeg".into(),
            }))
        }
        fn control(&self, action: &str, ms: Option<i64>) -> bool {
            self.calls.lock().unwrap().push(format!("{action} {ms:?}"));
            if action == "playpause" {
                let mut p = self.playing.lock().unwrap();
                *p = !*p;
            }
            matches!(action, "playpause" | "next" | "prev" | "seek")
        }
        fn pause(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn media_actions_answer_what_plays_now_and_bind_like_asp_net() {
        let (_dir, mut state) = test_state();
        let fake = Arc::new(FakeMedia {
            playing: Mutex::new(false),
            calls: Mutex::new(vec![]),
        });
        state.media = fake.clone();
        let app = build_router(state);
        let before = crate::state_signal::version();
        let r = request(&app, "POST", "/api/media/playpause", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        let v = r.json();
        assert_eq!(v["ok"], true);
        assert_eq!(v["media"]["playing"], true);
        assert_eq!(v["media"]["title"], "One");
        assert!(
            crate::state_signal::version() > before,
            "the phone's other screens are poked"
        );

        let r = request(&app, "POST", "/api/media/seek?MS=5000", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        let r = request(&app, "POST", "/api/media/bogus", &[], vec![]).await;
        assert_eq!((r.status, r.body.len()), (StatusCode::BAD_REQUEST, 0));
        let r = request(&app, "POST", "/api/media/seek?ms=x", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            *fake.calls.lock().unwrap(),
            ["playpause None", "seek Some(5000)", "bogus None"]
        );

        let r = request(&app, "GET", "/api/art", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            (r.header("content-type"), r.body.as_slice()),
            ("image/jpeg", &[0xff, 0xd8, 0xff][..])
        );
    }

    #[tokio::test]
    async fn a_media_command_waits_for_the_decks_media_focus() {
        use crate::server::guard::tests::{send, test_state};
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let held = s.deck.media_focus().lock_owned().await;
        let mut call = Box::pin(send(
            &app,
            "POST",
            "/api/media/bogus",
            &[("host", "127.0.0.1:8765")],
        ));
        let early = tokio::time::timeout(std::time::Duration::from_millis(200), &mut call).await;
        assert!(
            early.is_err(),
            "the command did not wait for the media focus"
        );
        drop(held);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .unwrap();
        assert_eq!(res.status(), 400); // "bogus" is no media action
    }

    #[tokio::test]
    async fn without_a_cover_the_art_is_404_and_the_state_has_no_media() {
        let (_dir, state) = test_state();
        let media = state.media.clone();
        let app = build_router(state);
        if media.art().is_none() {
            let r = request(&app, "GET", "/api/art", &[], vec![]).await;
            assert_eq!((r.status, r.body.len()), (StatusCode::NOT_FOUND, 0));
        }
        let source = crate::media_monitor::MediaState {
            media: Arc::new(FakeMedia {
                playing: Mutex::new(true),
                calls: Mutex::new(vec![]),
            }),
            deck: Arc::new(crate::media_monitor::NoDeck::default()),
        };
        let v = crate::state::StateSource::snapshot(&source);
        assert_eq!(v["app"], "Spotify");
        assert_eq!(v["ageMs"], 0);
    }

    // ---- spectrum ----

    #[tokio::test]
    async fn the_spectrum_route_answers_whether_it_is_shown_and_the_bands() {
        let (_dir, state) = test_state();
        let app = build_router(state.clone());
        let r = request(&app, "GET", "/api/spectrum", &[], vec![]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            r.json(),
            serde_json::json!({"shown": false, "active": false, "fps": 30, "b": null})
        );

        let page = serde_json::json!({"pages": [{"id": "p", "title": "P",
            "widgets": [{"id": "s", "type": "spectrum", "x": 0, "y": 0, "w": 6, "h": 3}]}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        state
            .spectrum
            .pretend_running(vec![7; crate::spectrum_analyzer::BANDS]);
        let r = request(&app, "GET", "/api/spectrum", &[], vec![]).await;
        let v = r.json();
        assert_eq!(
            (v["shown"].clone(), v["active"].clone(), v["fps"].clone()),
            (
                serde_json::json!(true),
                serde_json::json!(true),
                serde_json::json!(30)
            )
        );
        assert_eq!(v["b"].as_array().unwrap().len(), 48);
    }
}
