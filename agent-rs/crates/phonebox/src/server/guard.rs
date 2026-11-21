//! The request pipeline of `Program.cs`: request timings, then the security guard of the local port or of the Wi-Fi
//! port.
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

use super::{AppState, Via};
use crate::browser_guard::{is_foreign, starts_with_segments};
use crate::request_timings::{SLOW_MS, route_of};

/// Wraps `routes` in the timing and local guard layers.
pub fn apply_local(routes: Router<AppState>, state: AppState) -> Router {
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

/// Wraps `routes` in the timing and Wi-Fi guard layers.
pub fn apply_wifi(routes: Router<AppState>, state: AppState) -> Router {
    routes
        .layer(middleware::from_fn_with_state(state.clone(), wifi_guard))
        .layer(middleware::from_fn_with_state(state.clone(), timing))
        .with_state(state)
}

/// Every value of a header joined with "," (`StringValues.ToString()`); Err when one is not readable text.
fn joined(headers: &HeaderMap, name: header::HeaderName) -> Result<String, ()> {
    let values: Result<Vec<&str>, _> = headers.get_all(name).iter().map(|v| v.to_str()).collect();
    values.map(|v| v.join(",")).map_err(|_| ())
}

/// The Wi-Fi port (`Program.cs`): the phone's key opens the panel API; the separate webhook key opens only the webhook
/// endpoints, in a header or, for senders that cannot set headers, in `?token=`; a pairing claim needs no key. The
/// decision is made from the live config on every request.
async fn wifi_guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    use crate::lan::LanAccess;
    let headers = req.headers();
    // an Origin that is not readable text is still an Origin; an empty one is none (ASP.NET compares with "")
    let origin_present = joined(headers, header::ORIGIN).map_or(true, |o| !o.is_empty());
    let authorization = if headers.contains_key(header::AUTHORIZATION) {
        joined(headers, header::AUTHORIZATION).ok()
    } else {
        None
    };
    let query_token = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(req.uri())
        .ok()
        .and_then(|q| super::query::value(&q.0, "token"));
    let lan = state.cfg.get().lan.clone();
    let access = crate::lan::authorize_request(
        &lan,
        req.method().as_str(),
        req.uri().path(),
        origin_present,
        authorization.as_deref(),
        query_token.as_deref(),
    );
    match access {
        LanAccess::Denied => StatusCode::FORBIDDEN.into_response(),
        // pairing claims and webhooks must not touch the phone registry, the app version or "phone connected"; the
        // claim's 1 KB body cap is the claim handler's
        LanAccess::PairingClaim | LanAccess::Webhook => {
            req.extensions_mut().insert(Via::Wifi);
            let mut response = next.run(req).await;
            response
                .headers_mut()
                .entry(header::CACHE_CONTROL)
                .or_insert(HeaderValue::from_static("no-store"));
            response
        }
        LanAccess::Phone => tail(&state, req, next, Via::Wifi).await,
    }
}

async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let path = req.uri().path();
    let host = header_str(headers, "host").map(host_of).unwrap_or("");
    let port = state.endpoint.port;
    // an Origin that is not readable text never matches the settings page's own
    let same_origin = joined(headers, header::ORIGIN).is_ok_and(|origin| {
        origin.is_empty()
            || origin == format!("http://127.0.0.1:{port}")
            || origin == format!("http://localhost:{port}")
    });
    // Spotify's redirect after sign-in is a cross-site navigation to an /api path: let it through for the pending
    // login only (the Host and Origin checks still apply)
    let spotify_callback = req.method() == axum::http::Method::GET
        && path.eq_ignore_ascii_case("/api/controls/spotify/callback")
        && state.spotify.has_pending_state(
            axum::extract::Query::<Vec<(String, String)>>::try_from_uri(req.uri())
                .ok()
                .and_then(|q| crate::server::query::value(&q.0, "state"))
                .as_deref(),
        );
    let foreign = !spotify_callback
        && is_foreign(
            header_str(headers, "sec-fetch-site"),
            header_str(headers, "sec-fetch-mode"),
            req.method().as_str(),
            path,
        );
    if (host != "127.0.0.1" && host != "localhost") || !same_origin || foreign {
        return StatusCode::FORBIDDEN.into_response();
    }
    tail(&state, req, next, Via::Local).await
}

/// What both ports do for a request they let through (the phone tier on Wi-Fi): note the phone, then the security
/// headers.
async fn tail(state: &AppState, mut req: Request, next: Next, via: Via) -> Response {
    let headers = req.headers();
    let path = req.uri().path().to_string();
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
            via.transport(),
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
    req.extensions_mut().insert(via);
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
        apply_local(routes, state)
    }

    pub(crate) async fn send<S>(
        app: &S,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Response<Body>
    where
        S: tower::Service<
                Request<Body>,
                Response = Response<Body>,
                Error = std::convert::Infallible,
            > + Clone,
    {
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

    // ---------------------------------------------------------------- the Wi-Fi port
    use crate::server::Via;
    use axum::Extension;
    use axum::routing::post;

    const PHONE: &str = "Bearer phone-key";

    fn wifi_state(enabled: bool) -> (tempfile::TempDir, AppState) {
        let (d, s) = test_state();
        s.cfg
            .save(
                &format!(
                    r#"{{"lan":{{"enabled":{enabled},"port":8766,"token":"phone-key","webhookToken":"hook-key"}}}}"#
                ),
                None,
            )
            .unwrap();
        (d, s)
    }

    fn via_routes() -> Router<AppState> {
        let via =
            |v: Option<Extension<Via>>| async move { v.map(|v| v.0.label()).unwrap_or("none") };
        Router::new()
            .route("/api/state", get(via))
            .route("/api/settings", get(via))
            .route("/panel/index.html", get(via))
            .route("/api/push/{key}", post(via))
            .route("/api/pair/claim", post(via))
    }

    async fn body_of(res: Response<Body>) -> String {
        use http_body_util::BodyExt;
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    async fn send_raw<S>(
        app: &S,
        method: &str,
        path: &str,
        headers: &[(&str, &[u8])],
    ) -> Response<Body>
    where
        S: tower::Service<
                Request<Body>,
                Response = Response<Body>,
                Error = std::convert::Infallible,
            > + Clone,
    {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, HeaderValue::from_bytes(v).unwrap());
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn wifi_requests_need_the_phone_key() {
        let (_d, s) = wifi_state(true);
        let app = apply_wifi(via_routes(), s);
        let res = send(&app, "GET", "/api/state", &[]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_of(res).await, "");

        let res = send(&app, "GET", "/api/state", &[("authorization", PHONE)]).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert_eq!(res.headers()["x-frame-options"], "DENY");
        assert_eq!(body_of(res).await, "Wi-Fi");

        for headers in [
            vec![("authorization", "Bearer hook-key")],
            vec![("authorization", "bearer phone-key")],
            vec![("authorization", "Bearer  phone-key")],
            vec![("authorization", PHONE), ("authorization", PHONE)],
        ] {
            let res = send(&app, "GET", "/api/state", &headers).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{headers:?}");
        }
        // local-only paths stay closed with the key
        for path in ["/api/settings", "/panel/index.html"] {
            let res = send(&app, "GET", path, &[("authorization", PHONE)]).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        }

        // Wi-Fi turned off in the settings: refused at once (the port stays bound until a restart)
        let (_d, s) = wifi_state(false);
        let app = apply_wifi(via_routes(), s);
        let res = send(&app, "GET", "/api/state", &[("authorization", PHONE)]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_origin_closes_the_wifi_port() {
        let (_d, s) = wifi_state(true);
        let app = apply_wifi(via_routes(), s);
        let key: (&str, &[u8]) = ("authorization", PHONE.as_bytes());
        for origin in [
            &b"https://evil.example"[..],
            b"https://\xc3\xa9vil.example",
            b"\xff",
            b"null",
        ] {
            let res = send_raw(&app, "GET", "/api/state", &[key, ("origin", origin)]).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin:?}");
        }
        // two empty Origin headers join to ","
        let res = send_raw(
            &app,
            "GET",
            "/api/state",
            &[key, ("origin", b""), ("origin", b"")],
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // ASP.NET reads an empty Origin as no Origin
        let res = send_raw(&app, "GET", "/api/state", &[key, ("origin", b"")]).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_phone_seen_over_wifi_is_registered_as_wifi() {
        let (_d, s) = wifi_state(true);
        let app = apply_wifi(via_routes(), s.clone());
        let res = send(
            &app,
            "GET",
            "/api/state",
            &[
                ("authorization", PHONE),
                ("x-phonebox-device", "wifiguard1"),
                ("x-phonebox-app", "9.9.9"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let seen = crate::phone_registry::all()
            .into_iter()
            .find(|p| p.id == "wifiguard1")
            .unwrap();
        assert_eq!(seen.via, "Wi-Fi");
        assert!(s.phone.last_seen.load(Ordering::SeqCst) > 0);
        assert_eq!(
            s.phone.app_version.lock().unwrap().as_deref(),
            Some("9.9.9")
        );
    }

    #[tokio::test]
    async fn claims_and_webhooks_get_no_store_and_leave_the_phone_alone() {
        let (_d, s) = wifi_state(true);
        let app = apply_wifi(via_routes(), s.clone());
        let phone = [
            ("x-phonebox-device", "wifiguard2"),
            ("x-phonebox-app", "1.2.3"),
        ];
        for (path, extra) in [
            ("/api/pair/claim", None),
            ("/api/push/x?token=hook-key", None),
            ("/api/push/x?TOKEN=hook-key", None),
            ("/api/push/x", Some(("authorization", "Bearer hook-key"))),
        ] {
            let mut headers = phone.to_vec();
            headers.extend(extra);
            let res = send(&app, "POST", path, &headers).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(res.headers()["cache-control"], "no-store", "{path}");
            assert!(!res.headers().contains_key("x-frame-options"), "{path}");
        }
        assert!(
            crate::phone_registry::all()
                .iter()
                .all(|p| p.id != "wifiguard2")
        );
        assert_eq!(s.phone.last_seen.load(Ordering::SeqCst), 0);
        assert_eq!(*s.phone.app_version.lock().unwrap(), None);

        // the webhook key in the query opens nothing else; a claim needs POST
        let res = send(&app, "GET", "/api/state?token=hook-key", &[]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(&app, "GET", "/api/pair/claim", &[]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // the phone key on a webhook is the phone tier
        let res = send(&app, "POST", "/api/push/x", &[("authorization", PHONE)]).await;
        assert_eq!(res.headers()["x-frame-options"], "DENY");
    }

    #[tokio::test]
    async fn the_wifi_port_has_no_agent_api_rewrite() {
        let (_d, s) = wifi_state(true);
        let app = crate::server::build_wifi_router(s);
        let res = send(&app, "GET", "/agent/api/state", &[("authorization", PHONE)]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(&app, "GET", "/api/state", &[("authorization", PHONE)]).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.starts_with('{'));
    }

    #[tokio::test]
    async fn the_local_port_says_local_and_refuses_an_unreadable_origin() {
        let (_d, s) = test_state();
        let app = apply_local(via_routes(), s);
        let host: (&str, &[u8]) = ("host", b"127.0.0.1:8765");
        let res = send_raw(&app, "GET", "/api/state", &[host]).await;
        assert_eq!(body_of(res).await, "USB/local");
        let res = send_raw(
            &app,
            "GET",
            "/api/state",
            &[host, ("origin", b"http://127.0.0.1:8765\xff")],
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}
