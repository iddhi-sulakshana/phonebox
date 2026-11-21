//! The app console routes (`Program.cs:560-569`): what the widget shows, and pressing one of its keys.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::{AppState, json_response, query};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/console", get(state))
        .route("/api/console/{profile}/{slot}", post(act))
}

fn device(headers: &HeaderMap) -> String {
    headers
        .get("x-phonebox-device")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The matching profile's id, label and key labels for the widget the phone names (never the app or the keys).
async fn state(State(s): State<AppState>, headers: HeaderMap, Query(q): QueryList) -> Response {
    let (device, widget) = (device(&headers), query::value(&q, "widget"));
    let console = s.console.clone();
    // the focused window is asked for: off the async workers
    let (status, body) =
        tokio::task::spawn_blocking(move || console.state(Some(&device), widget.as_deref()))
            .await
            .unwrap_or((500, json!({ "error": "the app console could not be read" })));
    json_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &body,
    )
}

/// Presses a slot of a profile (`times` repeats a dial turn). A dropped request stops the keys not sent yet.
async fn act(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((profile, slot)): Path<(String, String)>,
    Query(q): QueryList,
) -> Response {
    let Ok(times) = query::int32(&q, "times") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (device, widget) = (device(&headers), query::value(&q, "widget"));
    let outcome = s
        .console
        .act(
            Some(&device),
            widget.as_deref(),
            &profile,
            &slot,
            times.unwrap_or(1),
            &crate::server::Stop::default(),
        )
        .await;
    match outcome {
        Ok((_, None)) => json_response(StatusCode::OK, &json!({ "ok": true })),
        Ok((status, Some(error))) => json_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &json!({ "error": error }),
        ),
        // only the request's own end cancels, and then nobody reads the answer
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};

    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn read(res: axum::http::Response<Body>) -> (u16, Value) {
        let status = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn the_console_routes_answer_the_widget_and_refuse_without_its_switch() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"},
                    "pages":[{"id":"p","title":"P","widgets":[{"id":"c","type":"app-console","x":0,"y":0,"w":6,"h":4,"options":{"control":false}}]}],
                    "console":{"profiles":[{"id":"ps","label":"Photoshop","apps":"photoshop","keys":[{"label":"Undo","keys":"ctrl+z"}]}]}}"#,
                None,
            )
            .unwrap();
        let app = crate::server::build_router(s.clone());
        let phone = ("x-phonebox-device", "phone1");
        assert_eq!(
            read(send(&app, "GET", "/api/console?widget=p/x", &[LOCAL, phone]).await).await,
            (
                404,
                json!({"error": "this app console widget isn't on an enabled page of this phone"})
            )
        );
        let (status, body) =
            read(send(&app, "GET", "/api/console?widget=p/c", &[LOCAL, phone]).await).await;
        assert_eq!(status, 200);
        assert_eq!(body["control"], json!(false));
        assert_eq!(body["supported"], json!(cfg!(windows)));
        assert_eq!(
            read(
                send(
                    &app,
                    "POST",
                    "/api/console/ps/k0?widget=p/c",
                    &[LOCAL, phone]
                )
                .await
            )
            .await,
            (
                403,
                json!({"error": "sending keys is off: turn on \"Send keys to the computer\" in the app console widget's settings"})
            )
        );
        assert_eq!(
            send(
                &app,
                "POST",
                "/api/console/ps/k0?widget=p/c&times=many",
                &[LOCAL, phone]
            )
            .await
            .status(),
            400
        );
        // the phone reaches both over Wi-Fi
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        assert_eq!(
            send(&wifi, "GET", "/api/console?widget=p/c", &[key])
                .await
                .status(),
            200
        );
        assert_eq!(
            send(&wifi, "POST", "/api/console/ps/k0?widget=p/c", &[key])
                .await
                .status(),
            403
        );
    }
}
