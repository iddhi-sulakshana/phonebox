//! The settings page (`/`), its icon, and the phone's panel under `/panel/` for the layout editor's live preview.

use axum::Router;
use axum::extract::{Path, RawQuery};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

pub const SETTINGS_HTML: &[u8] = include_bytes!("../../../../assets/settings.html");
pub const FAVICON: &[u8] = include_bytes!("../../../../assets/phonebox.ico");

/// The panel's files (index.html, main.js, style.css and the widget modules), embedded from the Android app's assets.
#[derive(rust_embed::RustEmbed)]
#[folder = "../../../android/app/src/main/assets/www/"]
pub struct Www;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    SETTINGS_HTML,
                )
            }),
        )
        .route(
            "/favicon.ico",
            get(|| async { ([(header::CONTENT_TYPE, "image/x-icon")], FAVICON) }),
        )
        .route(
            "/panel",
            // 302 Found, like ASP.NET's Results.Redirect (axum's Redirect::to answers 303)
            get(|RawQuery(q): RawQuery| async move {
                let to = format!("/panel/{}", q.map(|q| format!("?{q}")).unwrap_or_default());
                (StatusCode::FOUND, [(header::LOCATION, to)])
            }),
        )
        .route("/panel/", get(|| async { panel_file("index.html") }))
        .route(
            "/panel/{*file}",
            get(|Path(file): Path<String>| async move { panel_file(&file) }),
        )
}

/// `^(widgets/)?[A-Za-z0-9_.-]+$`, without "..".
fn valid_panel_path(file: &str) -> bool {
    let name = file.strip_prefix("widgets/").unwrap_or(file);
    !name.is_empty()
        && !file.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn panel_file(file: &str) -> Response {
    if !valid_panel_path(file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = match file.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    match Www::get(file) {
        Some(f) => ([(header::CONTENT_TYPE, content_type)], f.data.into_owned()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
