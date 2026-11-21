//! The widget data routes of plan 6 survey B: feeds, calendars, photos, cameras and the feed proxy
//! (`Program.cs:494-528`, `:809`).

use axum::Router;
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
        .route("/api/feeds", get(feeds))
        .route("/api/feeds/open", post(feeds_open))
        .route("/api/proxy", get(proxy))
        .route("/api/calendar", get(calendar))
        .route("/api/photos", get(photos))
        .route("/api/photos/{id}", get(photo))
        .route("/api/camera", get(camera))
}

/// The latest picture of the camera a Camera widget shows (`fresh`: exactly "true" or "1", read as text so an odd
/// value is not a 400). Never cached; the camera's address stays here.
async fn camera(State(s): State<AppState>, headers: HeaderMap, Query(q): QueryList) -> Response {
    use axum::http::{HeaderValue, header};
    let device = headers
        .get("x-phonebox-device")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let widget = query::value(&q, "widget");
    let fresh = matches!(query::value(&q, "fresh").as_deref(), Some("true" | "1"));
    let cameras = s.cameras.clone();
    // it may start ffmpeg: off the async workers
    let (status, jpeg, state) =
        tokio::task::spawn_blocking(move || cameras.frame(Some(&device), widget.as_deref(), fresh))
            .await
            .unwrap_or((503, None, "starting"));
    let mut res = match jpeg {
        // no newer picture than the one this widget has
        _ if status == 204 => StatusCode::NO_CONTENT.into_response(),
        Some(jpeg) => ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response(),
        None => json_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            &json!({ "state": state }),
        ),
    };
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

/// The photo ids of the chosen folder (scanned in the background), or why there are none.
async fn photos(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.photos.list())
}

/// One (scaled-down) photo by id; the phone may keep it for a day.
async fn photo(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.photos.photo(Some(&id)).await {
        Ok((bytes, mime)) => (
            [
                (axum::http::header::CONTENT_TYPE, mime),
                (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => json_response(StatusCode::NOT_FOUND, &json!({ "error": error })),
    }
}

/// The next two weeks of events (the feed addresses stay here).
async fn calendar(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &*s.calendar.snapshot())
}

/// The built-in public widget feeds for a docked phone without internet (local and USB only).
async fn proxy(State(s): State<AppState>, Query(q): QueryList) -> Response {
    use crate::widget_feed_proxy::ProxyOutcome;
    let status = |code: u16| StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    match crate::widget_feed_proxy::fetch(query::value(&q, "url").as_deref(), &s.proxy).await {
        ProxyOutcome::Disallowed => json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "Disallowed or invalid feed URL" }),
        ),
        ProxyOutcome::Status(code) => status(code).into_response(),
        ProxyOutcome::Body {
            status: code,
            content_type,
            body,
        } => (
            status(code),
            [(axum::http::header::CONTENT_TYPE, content_type)],
            body,
        )
            .into_response(),
    }
}

/// Headlines for the Feeds widget; the feed and article addresses stay here, the phone opens an article by its id.
async fn feeds(State(s): State<AppState>) -> Response {
    let snap = s.feeds.snapshot();
    json_response(
        StatusCode::OK,
        &json!({ "items": snap.items, "updated": snap.updated, "errors": snap.errors,
            "canOpen": s.feeds.open_allowed() }),
    )
}

/// The id comes from the query string (any body is ignored).
async fn feeds_open(State(s): State<AppState>, Query(q): QueryList) -> Response {
    match s.feeds.open(query::value(&q, "id").as_deref()) {
        None => json_response(StatusCode::OK, &json!({ "ok": true })),
        Some(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

#[cfg(test)]
mod tests {
    use crate::server::guard::tests::{send, test_state};
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn json_of(res: axum::http::Response<Body>) -> Value {
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn the_feed_routes_answer_with_their_shapes() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/feeds", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"items": [], "updated": 0, "errors": [], "canOpen": false})
        );
        let off = r#"Opening articles is off: turn on "Open articles on the PC" in the News feeds widget's settings"#;
        for path in ["/api/feeds/open", "/api/feeds/open?id=x"] {
            let res = send(&app, "POST", path, &[LOCAL]).await;
            assert_eq!(res.status(), 400, "{path}");
            assert_eq!(json_of(res).await, json!({"error": off}), "{path}");
        }
    }

    #[tokio::test]
    async fn the_proxy_route_refuses_other_addresses_and_passes_the_upstream_answer() {
        use futures_util::{FutureExt, StreamExt};
        let (_d, mut s) = test_state();
        s.proxy = std::sync::Arc::new(|url: reqwest::Url| {
            let status = if url.path() == "/teapot" {
                418
            } else if url.path() == "/moved" {
                301
            } else {
                200
            };
            async move {
                Ok(crate::widget_feed_proxy::ProxyReply {
                    status,
                    content_type: Some("application/json;charset=utf-8".into()),
                    content_length: None,
                    body: futures_util::stream::iter([
                        Ok::<_, crate::widget_feed_proxy::ProxyFail>(
                            axum::body::Bytes::from_static(b"{\"t\":1}"),
                        ),
                    ])
                    .boxed(),
                })
            }
            .boxed()
        });
        let app = crate::server::build_router(s.clone());
        for path in [
            "/api/proxy",
            "/api/proxy?url=https://example.com/x",
            "/api/proxy?url=http://api.open-meteo.com/v1/forecast",
            "/api/proxy?url=https://api.open-meteo.com:8443/x",
        ] {
            let res = send(&app, "GET", path, &[LOCAL]).await;
            assert_eq!(res.status(), 400, "{path}");
            assert_eq!(
                json_of(res).await,
                json!({"error": "Disallowed or invalid feed URL"}),
                "{path}"
            );
        }
        let res = send(
            &app,
            "GET",
            "/api/proxy?url=https%3A%2F%2Fapi.open-meteo.com%2Fteapot",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 418);
        assert_eq!(
            res.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert_eq!(
            to_bytes(res.into_body(), usize::MAX).await.unwrap(),
            &b"{\"t\":1}"[..]
        );
        let res = send(
            &app,
            "GET",
            "/api/proxy?url=https://api.open-meteo.com/moved",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 502);
        assert!(res.headers().get("content-type").is_none());
        assert!(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_calendar_route_answers_the_snapshot_on_both_ports() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/calendar", &[LOCAL]).await).await;
        assert_eq!(r, json!({"events": [], "updated": 0, "errors": []}));
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(
            send(&wifi, "GET", "/api/calendar", &[key]).await.status(),
            200
        );
    }

    #[tokio::test]
    async fn the_photo_routes_answer_ids_and_one_photo_at_a_time() {
        let (dir, s) = test_state();
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let jpeg = [
            0xFFu8, 0xD8, 0xFF, 0xC0, 0, 0x11, 8, 0, 0x10, 0, 0x10, 3, 1, 0x22, 0, 2, 0x11, 1, 3,
            0x11, 1, 0xFF, 0xD9,
        ];
        std::fs::write(photos.join("a.jpg"), jpeg).unwrap();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/photos", &[LOCAL]).await).await;
        assert_eq!(r, json!({"ids": [], "error": null, "scanning": false}));
        let res = send(&app, "GET", "/api/photos/abc", &[LOCAL]).await;
        assert_eq!(res.status(), 404);
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert_eq!(
            json_of(res).await,
            json!({"error": "Add a Photo frame widget to a page to show photos"})
        );

        s.cfg
            .save(
                &format!(
                    r#"{{"photos":{{"folder":{}}},"pages":[{{"id":"p","title":"P","widgets":[{{"id":"w","type":"photos","x":0,"y":0,"w":6,"h":4}}]}}]}}"#,
                    serde_json::to_string(&photos.to_string_lossy()).unwrap()
                ),
                None,
            )
            .unwrap();
        let first = json_of(send(&app, "GET", "/api/photos", &[LOCAL]).await).await;
        assert_eq!(first, json!({"ids": [], "error": null, "scanning": true}));
        let mut ids = Value::Null;
        for _ in 0..200 {
            let r = json_of(send(&app, "GET", "/api/photos", &[LOCAL]).await).await;
            if r["scanning"] == false {
                ids = r["ids"].clone();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let id = ids[0].as_str().unwrap().to_string();
        let res = send(&app, "GET", &format!("/api/photos/{id}"), &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "image/jpeg");
        assert_eq!(res.headers()["cache-control"], "private, max-age=86400");
        assert_eq!(
            to_bytes(res.into_body(), usize::MAX).await.unwrap(),
            &jpeg[..]
        );
        let res = send(&app, "GET", "/api/photos/0123456789abcdef", &[LOCAL]).await;
        assert_eq!(json_of(res).await, json!({"error": "Unknown photo"}));
    }

    #[tokio::test]
    async fn the_camera_route_answers_a_state_that_is_never_cached_on_both_ports() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"},
                    "pages":[{"id":"p","title":"P","widgets":[{"id":"w","type":"camera","x":0,"y":0,"w":4,"h":3,"options":{"camera":"gone00000001"}}]}]}"#,
                None,
            )
            .unwrap();
        let app = crate::server::build_router(s.clone());
        let phone = ("x-phonebox-device", "phone1");
        // fresh is read as text: odd values are not a 400
        for (path, state) in [
            ("/api/camera", "unknown"),
            ("/api/camera?widget=p/x", "unknown"),
            ("/api/camera?widget=p/w", "pick"),
            ("/api/camera?widget=p/w&fresh=True", "pick"),
            ("/api/camera?widget=p/w&fresh=x", "pick"),
        ] {
            let res = send(&app, "GET", path, &[LOCAL, phone]).await;
            assert_eq!(res.status(), 404, "{path}");
            assert_eq!(res.headers()["cache-control"], "no-store", "{path}");
            assert_eq!(res.headers()["x-content-type-options"], "nosniff", "{path}");
            assert_eq!(json_of(res).await, json!({"state": state}), "{path}");
        }
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        let res = send(&wifi, "GET", "/api/camera?widget=p/w", &[key]).await;
        assert_eq!(json_of(res).await, json!({"state": "pick"}));
    }

    #[tokio::test]
    async fn the_phone_reaches_the_feeds_over_wifi() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(send(&wifi, "GET", "/api/feeds", &[key]).await.status(), 200);
        assert_eq!(
            send(&wifi, "POST", "/api/feeds/open?id=x", &[key])
                .await
                .status(),
            400
        );
    }
}
