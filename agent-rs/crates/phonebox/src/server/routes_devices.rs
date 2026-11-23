//! The owner's devices: the Growatt inverter (`/api/growatt*`) and the TCL air conditioners (`/api/tcl*`). Their
//! Settings › Test routes (`/api/growatt-test`, `/api/tcl-test`) are local only.

use axum::Router;
use axum::body::Bytes;
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
        .route("/api/growatt", get(growatt_view))
        .route("/api/growatt/settings", get(growatt_settings))
        .route("/api/growatt/settings/{name}", post(growatt_set))
        .route("/api/growatt/relogin", post(growatt_relogin))
        .route("/api/growatt-test", post(growatt_test))
        .route("/api/tcl", get(tcl_view))
        .route("/api/tcl/{unit}/{action}", post(tcl_act))
        .route("/api/tcl-test", post(tcl_test))
}

/// `{"ok":true, …more}` or 400 `{"error"}`.
fn outcome(result: Result<serde_json::Value, String>) -> Response {
    match result {
        Ok(mut more) => {
            if let Some(o) = more.as_object_mut() {
                o.insert("ok".into(), json!(true));
            }
            json_response(StatusCode::OK, &more)
        }
        Err(error) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": error })),
    }
}

async fn growatt_view(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.growatt.view().await)
}

async fn growatt_settings(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.growatt.settings().await)
}

async fn growatt_set(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(q): QueryList,
) -> Response {
    let value = query::value(&q, "value").unwrap_or_default();
    outcome(s.growatt.set(&name, &value).await.map(|()| json!({})))
}

async fn growatt_relogin(State(s): State<AppState>) -> Response {
    outcome(
        s.growatt
            .relogin()
            .await
            .map(|connected| json!({ "connected": connected })),
    )
}

/// Settings › Test with the (maybe unsaved) address, username and password: `{"result":"…"}`.
async fn growatt_test(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match query::json_object(&headers, &body) {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let (Ok(url), Ok(username), Ok(password)) = (
        query::json_string(&body, "url"),
        query::json_string(&body, "username"),
        query::json_string(&body, "password"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let result = s
        .growatt
        .test(
            url.unwrap_or(""),
            username.unwrap_or(""),
            password.unwrap_or(""),
        )
        .await;
    json_response(StatusCode::OK, &json!({ "result": result }))
}

async fn tcl_view(State(s): State<AppState>) -> Response {
    json_response(StatusCode::OK, &s.tcl.view().await)
}

/// `unit` is a unit's id or `all`.
async fn tcl_act(
    State(s): State<AppState>,
    Path((unit, action)): Path<(String, String)>,
    Query(q): QueryList,
) -> Response {
    let value = query::value(&q, "value").unwrap_or_default();
    outcome(
        s.tcl
            .act(&unit, &action, &value)
            .await
            .map(|sent| json!({ "sent": sent })),
    )
}

/// Settings › Test with the (maybe unsaved) account: `{"result":"…"}`.
async fn tcl_test(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match query::json_object(&headers, &body) {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let (Ok(username), Ok(password)) = (
        query::json_string(&body, "username"),
        query::json_string(&body, "password"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let result = s
        .tcl
        .test(username.unwrap_or(""), password.unwrap_or(""))
        .await;
    json_response(StatusCode::OK, &json!({ "result": result }))
}

#[cfg(test)]
mod tests {
    use crate::server::guard::tests::{send, test_state};
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn json_of(res: axum::http::Response<Body>) -> Value {
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    async fn post(
        app: &crate::server::App,
        path: &str,
        content_type: &str,
        body: &str,
    ) -> axum::http::Response<Body> {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:8765")
            .header("content-type", content_type)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn the_growatt_routes_answer_with_their_shapes() {
        let srv = crate::growatt::fake::Server::start().await;
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/growatt", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"configured": false, "error": crate::growatt::NOT_CONFIGURED, "control": false, "host": "",
                "status": null, "totals": null})
        );
        s.cfg
            .save(
                &json!({
                    "growatt": {"url": srv.fake.url, "password": "sun"},
                    "pages": [{"id": "solar", "widgets": [{"type": "growatt", "options": {"control": true}}]}],
                })
                .to_string(),
                None,
            )
            .unwrap();
        let r = json_of(send(&app, "GET", "/api/growatt", &[LOCAL]).await).await;
        assert_eq!(r["error"], Value::Null);
        assert_eq!(r["control"], json!(true));
        assert_eq!(r["status"]["pvW"].as_f64(), Some(1264.0)); // written as the C# agent writes numbers: 1264
        assert_eq!(r["totals"]["solar"]["today"], json!(5.2));
        let r = json_of(send(&app, "GET", "/api/growatt/settings", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"error": null, "acOutputSource": 1, "chargeSource": 0, "maxChargeCurrent": 30})
        );
        let res = send(
            &app,
            "POST",
            "/api/growatt/settings/charge-source?value=9",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            json_of(res).await,
            json!({"error": "The value must be 0 to 2"})
        );
        let res = send(
            &app,
            "POST",
            "/api/growatt/settings/charge-source?value=2",
            &[LOCAL],
        )
        .await;
        assert_eq!(res.status(), 200);
        assert_eq!(json_of(res).await, json!({"ok": true}));
        assert_eq!(srv.settings.lock().unwrap()["charge-source"], 2);
        let res = send(&app, "POST", "/api/growatt/relogin", &[LOCAL]).await;
        assert_eq!(json_of(res).await, json!({"ok": true, "connected": true}));
        // Settings › Test: a typed JSON body, names in any case
        let body = json!({"URL": srv.fake.url, "Password": "sun"}).to_string();
        let res = post(&app, "/api/growatt-test", "application/json", &body).await;
        assert_eq!(json_of(res).await, json!({"result": "Connected"}));
        let body =
            json!({"url": srv.fake.url, "username": "admin", "password": "nope"}).to_string();
        let res = post(&app, "/api/growatt-test", "application/json", &body).await;
        assert_eq!(
            json_of(res).await,
            json!({"result": "The Growatt server refused the username or password"})
        );
        assert_eq!(
            post(&app, "/api/growatt-test", "text/plain", &body)
                .await
                .status(),
            415
        );
        assert_eq!(
            post(
                &app,
                "/api/growatt-test",
                "application/json",
                r#"{"url":5}"#
            )
            .await
            .status(),
            400
        );
    }

    #[tokio::test]
    async fn the_tcl_routes_answer_with_their_shapes() {
        let cloud = crate::tcl::fake::Cloud::start().await;
        let (_d, mut s) = test_state();
        let clock = cloud.clock.clone();
        s.tcl = std::sync::Arc::new(crate::tcl::Tcl::with_endpoints(
            s.cfg.clone(),
            cloud.endpoints(),
            std::sync::Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst)),
        ));
        let app = crate::server::build_router(s.clone());
        let r = json_of(send(&app, "GET", "/api/tcl", &[LOCAL]).await).await;
        assert_eq!(
            r,
            json!({"configured": false, "error": crate::tcl::NOT_CONFIGURED, "control": false, "units": []})
        );
        s.cfg
            .save(
                &json!({
                    "tcl": {"username": crate::tcl::fake::ACCOUNT, "password": crate::tcl::fake::PASSWORD},
                    "pages": [{"id": "ac", "widgets": [{"type": "tcl", "options": {"control": true}}]}],
                })
                .to_string(),
                None,
            )
            .unwrap();
        let r = json_of(send(&app, "GET", "/api/tcl", &[LOCAL]).await).await;
        assert_eq!(r["error"], Value::Null);
        assert_eq!(r["control"], json!(true));
        assert_eq!(r["units"].as_array().unwrap().len(), 3);
        assert_eq!(r["units"][0]["name"], json!("Shop front"));
        assert_eq!(r["units"][0]["inside"].as_f64(), Some(29.5));
        let res = send(&app, "POST", "/api/tcl/all/generator?value=2", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(json_of(res).await, json!({"ok": true, "sent": 2}));
        let res = send(&app, "POST", "/api/tcl/ac1/temp?value=40", &[LOCAL]).await;
        assert_eq!(res.status(), 400);
        assert_eq!(
            json_of(res).await,
            json!({"error": "The temperature must be a whole number from 16 to 31"})
        );
        // Settings › Test with the typed account
        let body =
            json!({"Username": crate::tcl::fake::ACCOUNT, "password": crate::tcl::fake::PASSWORD})
                .to_string();
        let res = post(&app, "/api/tcl-test", "application/json", &body).await;
        assert_eq!(json_of(res).await, json!({"result": "Connected · 3 units"}));
        assert_eq!(
            post(&app, "/api/tcl-test", "text/plain", &body)
                .await
                .status(),
            415
        );
        assert_eq!(
            post(
                &app,
                "/api/tcl-test",
                "application/json",
                r#"{"username":1}"#
            )
            .await
            .status(),
            400
        );
    }

    #[tokio::test]
    async fn only_the_phones_tcl_routes_are_served_over_wifi() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(send(&wifi, "GET", "/api/tcl", &[key]).await.status(), 200);
        assert_eq!(
            send(&wifi, "POST", "/api/tcl/all/power?value=on", &[key])
                .await
                .status(),
            400
        );
        assert_eq!(
            send(&wifi, "POST", "/api/tcl-test", &[key]).await.status(),
            403
        );
    }

    #[tokio::test]
    async fn only_the_phones_growatt_routes_are_served_over_wifi() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(
            send(&wifi, "GET", "/api/growatt", &[key]).await.status(),
            200
        );
        assert_eq!(
            send(&wifi, "GET", "/api/growatt/settings", &[key])
                .await
                .status(),
            200
        );
        // not configured: refused by the service, not by the Wi-Fi guard
        assert_eq!(
            send(
                &wifi,
                "POST",
                "/api/growatt/settings/charge-source?value=1",
                &[key]
            )
            .await
            .status(),
            400
        );
        assert_eq!(
            send(&wifi, "POST", "/api/growatt/relogin", &[key])
                .await
                .status(),
            400
        );
        assert_eq!(
            send(&wifi, "POST", "/api/growatt-test", &[key])
                .await
                .status(),
            403
        );
    }
}
