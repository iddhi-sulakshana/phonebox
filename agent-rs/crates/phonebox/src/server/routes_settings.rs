//! `GET/PUT /api/settings` (the settings page) and `GET /api/config` (what the phone sees).

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use super::{AppState, json_response};
use crate::config::store::CONFLICT;
use crate::config::write::to_dotnet_json;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/catalog", get(catalog))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/config", get(get_config))
}

/// Widget types and page templates for the settings page's layout editor (the page loads nothing before them).
async fn catalog() -> Response {
    use crate::catalog::{COLS, ROWS, TEMPLATES_JSON, WIDGETS_JSON};
    let text = format!(
        "{{\"cols\":{COLS},\"rows\":{ROWS},\"widgets\":{WIDGETS_JSON},\"templates\":{TEMPLATES_JSON}}}"
    );
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        text,
    )
        .into_response()
}

/// The settings, with the version the page sends back so it saves only over what it loaded.
async fn get_settings(State(s): State<AppState>) -> Response {
    let text = to_dotnet_json(&s.cfg.get());
    (
        [
            (
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".to_string(),
            ),
            (
                header::HeaderName::from_static("x-config-version"),
                s.cfg.version_text(),
            ),
        ],
        text,
    )
        .into_response()
}

async fn put_settings(State(s): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    // scripts may leave the version out
    let expected = headers
        .get("x-config-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());
    // C# reads the body through a StreamReader, which drops a BOM before these checks; the save strips it as well
    let body = match body.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => body,
    };
    let port = s.endpoint.port;
    if lan_port_collides_with_local(&body, port) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": format!("Wi-Fi port cannot be the same as the local port ({port})") }),
        );
    }
    if lan_keys_shared(&body) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "The Wi-Fi key and the webhook key must be different: make a new key for one of them" }),
        );
    }
    // the same lock as the icon commits, so an icon upload can't report a version that already holds a save it never
    // checked
    let saved = {
        let _commit = super::ICON_COMMIT.lock().unwrap_or_else(|e| e.into_inner());
        s.cfg.save(&body, expected)
    };
    match saved {
        Ok(version) => {
            // the deck, the photo frame and the cameras catch up before the page hears back
            let hooks = s.settings_hooks.clone();
            let _ = tokio::task::spawn_blocking(move || hooks.run()).await;
            // drop uploads for shortcuts that got deleted; commands may have changed, so resolve the program icons again
            s.icons.cleanup(|| {
                s.cfg
                    .get()
                    .shortcuts
                    .iter()
                    .map(|x| x.id.clone())
                    .collect::<Vec<_>>()
            });
            crate::shortcut_icons::clear_has_icon_cache();
            crate::log::info("Program", "settings saved");
            json_response(StatusCode::OK, &json!({ "version": version.to_string() }))
        }
        Err(e) if e == CONFLICT => json_response(StatusCode::CONFLICT, &json!({ "error": e })),
        Err(e) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": e })),
    }
}

async fn get_config(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let device = headers
        .get("x-phonebox-device")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let version = s.cfg.version_text();
    // a shortcut's picture: the upload's revision, else its program icon's
    let icons = s.icons.clone();
    let lookup = move |sc: &crate::config::Shortcut| match icons.revision(&sc.id) {
        Some(upload) => (Some(upload), true),
        None => (crate::shortcut_icons::revision(sc), false),
    };
    let config = crate::config::public::to_public(&s.cfg.get(), Some(device), Some(&lookup));
    json_response(
        StatusCode::OK,
        &json!({ "version": version, "config": config }),
    )
}

/// The settings' `lan` sections in order, read like `ConfigStore.Save` (last one wins, any letter case).
fn lan_sections(json: &str) -> Option<Vec<Value>> {
    let text = crate::config::read::strip_comments_and_trailing_commas(json);
    let Value::Object(root) = serde_json::from_str::<Value>(&text).ok()? else {
        return None;
    };
    Some(
        root.into_iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("lan"))
            .map(|(_, v)| v)
            .collect(),
    )
}

fn member<'a>(obj: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    obj.iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .next_back()
}

/// True when the settings turn Wi-Fi on at the local port itself.
pub fn lan_port_collides_with_local(json: &str, local_port: u16) -> bool {
    let Some(sections) = lan_sections(json) else {
        return false;
    };
    let Some(Value::Object(lan)) = sections.last() else {
        return false;
    };
    let enabled = member(lan, "enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let port = member(lan, "port")
        .and_then(Value::as_i64)
        .filter(|p| i32::try_from(*p).is_ok())
        .map(|p| p.clamp(1024, 65535))
        .unwrap_or(8766);
    enabled && port == local_port as i64
}

/// True when the settings give the phone key and the webhook key the same value.
pub fn lan_keys_shared(json: &str) -> bool {
    let Some(sections) = lan_sections(json) else {
        return false;
    };
    let Some(Value::Object(lan)) = sections.last() else {
        return false;
    };
    let token = member(lan, "token").and_then(Value::as_str);
    let webhook = member(lan, "webhookToken").and_then(Value::as_str);
    token.is_some_and(|t| !t.trim().is_empty()) && token == webhook
}
