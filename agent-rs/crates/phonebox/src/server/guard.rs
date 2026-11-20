//! The request pipeline of `Program.cs`: request timings, then the security guard of the local port.
//!
//! The local port listens on 127.0.0.1 only; the Host check blocks DNS rebinding. The phone's USB proxy sends no Origin;
//! from browsers only the settings page itself (same origin) is accepted, and Sec-Fetch-Site keeps other sites out.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use super::AppState;
use crate::browser_guard::{is_foreign, starts_with_segments};
use crate::request_timings::{SLOW_MS, route_of};

/// Wraps `routes` in the timing and guard layers.
pub fn apply(routes: Router<AppState>, state: AppState) -> Router {
    routes
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(middleware::from_fn_with_state(state.clone(), timing))
        .with_state(state)
}

/// How long each kind of request takes (GET /api/latency), and a log line for slow control actions.
async fn timing(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !starts_with_segments(path, "/api") || starts_with_segments(path, "/api/stream") {
        return next.run(req).await;
    }
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string());
    let method = req.method().as_str().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    let ms = started.elapsed().as_millis() as i64;
    // only a request that matched a route, so unknown paths cannot add rows
    if let Some(route) = route_of(matched.as_deref(), &method) {
        state.timings.add(&route, ms);
        if ms >= SLOW_MS && method != "GET" {
            crate::log::info(
                "PhoneBoxAgent",
                &format!("slow request: {route} took {ms} ms"),
            );
        }
    }
    response
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The host part of a Host header (`127.0.0.1:8765` → `127.0.0.1`, `[::1]:8765` → `[::1]`).
fn host_of(value: &str) -> &str {
    if value.starts_with('[') {
        value.find(']').map(|i| &value[..=i]).unwrap_or(value)
    } else {
        value.split(':').next().unwrap_or("")
    }
}

async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let path = req.uri().path().to_string();
    let host = header_str(headers, "host").map(host_of).unwrap_or("");
    let origin = header_str(headers, "origin").unwrap_or("");
    let port = state.endpoint.port;
    let same_origin = origin.is_empty()
        || origin == format!("http://127.0.0.1:{port}")
        || origin == format!("http://localhost:{port}");
    let foreign = is_foreign(
        header_str(headers, "sec-fetch-site"),
        header_str(headers, "sec-fetch-mode"),
        req.method().as_str(),
        &path,
    );
    if (host != "127.0.0.1" && host != "localhost") || !same_origin || foreign {
        return StatusCode::FORBIDDEN.into_response();
    }
    let app = header_str(headers, "x-phonebox-app").map(str::to_string);
    if starts_with_segments(&path, "/api/state") && app.is_some() {
        state
            .phone
            .last_seen
            .store(crate::server::now_ms(), Ordering::SeqCst);
    }
    if let Some(device) = header_str(headers, "x-phonebox-device") {
        let problem = crate::phone_registry::touch(
            Some(device),
            header_str(headers, "x-phonebox-model"),
            "USB",
            header_str(headers, "x-phonebox-webview"),
        );
        if let Some(problem) = problem {
            crate::log::warn("PhoneBoxAgent", &format!("WebView: {problem}"));
        }
    }
    if let Some(app) = app.filter(|a| (1..32).contains(&a.len())) {
        *state
            .phone
            .app_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(app);
    }
    let mut response = next.run(req).await;
    // set before the handler runs in the C# agent, so a handler's own value wins
    let h = response.headers_mut();
    h.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    // only the panel may be framed, and only by the settings page itself (the live preview)
    let frame = if starts_with_segments(&path, "/panel") {
        "SAMEORIGIN"
    } else {
        "DENY"
    };
    h.entry(header::X_FRAME_OPTIONS)
        .or_insert(HeaderValue::from_static(frame));
    response
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, Response};
    use axum::routing::get;
    use tower::ServiceExt;

    pub(crate) fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = std::sync::Arc::new(crate::config::store::ConfigStore::new(
            dir.path().join("config.json"),
        ));
        let push = crate::push_store::PushStore::new(&dir.path().join("data.json"));
        let state = AppState::new(
            cfg,
            crate::agent_endpoint::AgentEndpoint::default(),
            push,
            std::sync::Arc::new(crate::state::StateSources::phase1()),
        );
        (dir, state)
    }

    fn test_router(state: AppState) -> Router {
        let routes = Router::new()
            .route("/api/info", get(|| async { "{}" }))
            .route("/panel/index.html", get(|| async { "<html>" }));
        apply(routes, state)
    }

    pub(crate) async fn send(
        app: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Response<Body> {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_foreign_host_and_origin() {
        let (_d, state) = test_state();
        let app = test_router(state);
        assert_eq!(
            send(&app, "GET", "/api/info", &[("host", "evil.example:8765")])
                .await
                .status(),
            403
        );
        assert_eq!(
            send(
                &app,
                "GET",
                "/api/info",
                &[
                    ("host", "127.0.0.1:8765"),
                    ("origin", "http://evil.example")
                ]
            )
            .await
            .status(),
            403
        );
        assert_eq!(
            send(&app, "GET", "/api/info", &[]).await.status(),
            403,
            "no Host at all"
        );
        assert_eq!(
            send(&app, "GET", "/api/info", &[("host", "127.0.0.1:8765")])
                .await
                .status(),
            200
        );
        assert_eq!(
            send(
                &app,
                "GET",
                "/api/info",
                &[
                    ("host", "localhost:8765"),
                    ("origin", "http://127.0.0.1:8765")
                ]
            )
            .await
            .status(),
            200
        );
        assert_eq!(
            send(
                &app,
                "GET",
                "/api/info",
                &[
                    ("host", "127.0.0.1:8765"),
                    ("sec-fetch-site", "cross-site"),
                    ("sec-fetch-mode", "no-cors")
                ]
            )
            .await
            .status(),
            403
        );
    }

    #[tokio::test]
    async fn sets_cache_and_frame_headers() {
        let (_d, state) = test_state();
        let app = test_router(state);
        let panel = send(
            &app,
            "GET",
            "/panel/index.html",
            &[("host", "127.0.0.1:8765")],
        )
        .await;
        assert_eq!(panel.headers()["x-frame-options"], "SAMEORIGIN");
        assert_eq!(panel.headers()["cache-control"], "no-store");
        let api = send(&app, "GET", "/api/info", &[("host", "127.0.0.1:8765")]).await;
        assert_eq!(api.headers()["x-frame-options"], "DENY");
        assert_eq!(api.headers()["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn the_phones_headers_register_it_and_its_app_version() {
        let (_d, state) = test_state();
        let app = test_router(state.clone());
        let headers = [
            ("host", "127.0.0.1:8765"),
            ("x-phonebox-device", "guardtest01"),
            ("x-phonebox-model", "RNE-L22"),
            ("x-phonebox-app", "0.9.22"),
            (
                "x-phonebox-webview",
                "com.google.android.webview/138.0.7204.179",
            ),
        ];
        assert_eq!(send(&app, "GET", "/api/info", &headers).await.status(), 200);
        assert!(
            crate::phone_registry::all()
                .iter()
                .any(|p| p.id == "guardtest01" && p.via == "USB" && p.model == "RNE-L22")
        );
        assert_eq!(
            state.phone.app_version.lock().unwrap().as_deref(),
            Some("0.9.22")
        );
    }

    #[tokio::test]
    async fn api_requests_are_timed_by_route() {
        let (_d, state) = test_state();
        let app = test_router(state.clone());
        send(&app, "GET", "/api/info", &[("host", "127.0.0.1:8765")]).await;
        send(&app, "GET", "/api/nope", &[("host", "127.0.0.1:8765")]).await; // matched no route: not recorded
        let rows = state.timings.summary();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].route, "GET /api/info");
    }
}
