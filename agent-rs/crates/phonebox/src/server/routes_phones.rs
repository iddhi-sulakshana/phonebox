//! Settings › Phones: the phones that talked to this agent with their settings, and "Show on phone"
//! (`Program.cs:424, 754-770`).

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;

use super::{AppState, json_response};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/phones", get(phones))
        .route("/api/phone/open", post(open))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhoneView {
    id: String,
    model: Option<String>,
    last_seen: Option<i64>,
    via: Option<String>,
    web_view: Option<String>,
    webview_problem: Option<String>,
    name: Option<String>,
    pages: Option<Vec<String>>,
}

async fn phones(State(s): State<AppState>) -> Response {
    let c = s.cfg.get();
    let seen = crate::phone_registry::all();
    let mut ids: Vec<String> = seen.iter().map(|p| p.id.clone()).collect();
    for p in &c.phones {
        if !ids.contains(&p.id) {
            ids.push(p.id.clone());
        }
    }
    let mut list: Vec<PhoneView> = ids
        .into_iter()
        .map(|id| {
            let s = seen.iter().find(|p| p.id == id);
            let configured = c.phones.iter().find(|p| p.id == id);
            PhoneView {
                model: s.map(|p| p.model.clone()),
                last_seen: s.map(|p| p.last_seen),
                via: s.map(|p| p.via.clone()),
                web_view: s.and_then(|p| p.web_view.clone()),
                webview_problem: s.and_then(|p| {
                    crate::webview_doctor::diagnose(p.web_view.as_deref(), Some(&p.model))
                }),
                name: configured.map(|p| p.name.clone()),
                pages: configured.and_then(|p| p.pages.clone()),
                id,
            }
        })
        .collect();
    // OrderByDescending(lastSeen ?? 0): stable
    list.sort_by_key(|p| std::cmp::Reverse(p.last_seen.unwrap_or(0)));
    json_response(StatusCode::OK, &list)
}

/// Wakes every phone in use and starts the panel; an adb failure is a 500 as in C#.
async fn open(State(s): State<AppState>) -> Response {
    match s.adb.open_panel().await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            crate::log::warn("Program", &format!("could not open the panel: {e}"));
            super::unhandled()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::guard::tests::{send, test_state};
    use axum::body::to_bytes;

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    #[tokio::test]
    async fn the_phone_list_joins_the_seen_phones_and_the_configured_ones() {
        let (_d, s) = test_state();
        s.cfg
            .save(
                r#"{"pages":[{"id":"desk"}],"phones":[{"id":"p6configured","name":"Kitchen","pages":["desk"]},{"id":"p6seen","name":"Desk phone"}]}"#,
                None,
            )
            .unwrap();
        crate::phone_registry::forget(&["p6seen", "p6configured"]);
        crate::phone_registry::touch(
            Some("p6seen"),
            Some("RNE-L22"),
            "usb",
            Some("Chrome/138.0.0.0"),
        );
        let app = crate::server::build_router(s);
        let res = send(&app, "GET", "/api/phones", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let ours: Vec<&serde_json::Value> = list
            .iter()
            .filter(|p| p["id"].as_str().is_some_and(|id| id.starts_with("p6")))
            .collect();
        assert_eq!(ours.len(), 2);
        // the seen phone first (newest), then the configured one never seen
        let seen = ours[0];
        let keys: Vec<&str> = seen
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "model",
                "lastSeen",
                "via",
                "webView",
                "webviewProblem",
                "name",
                "pages"
            ]
        );
        assert_eq!(seen["id"], "p6seen");
        assert_eq!(seen["model"], "RNE-L22");
        assert_eq!(seen["via"], "usb");
        assert!(seen["lastSeen"].as_i64().unwrap() > 0);
        assert_eq!(seen["name"], "Desk phone");
        assert!(seen["pages"].is_null());
        let configured = ours[1];
        assert_eq!(configured["id"], "p6configured");
        for key in ["model", "lastSeen", "via", "webView", "webviewProblem"] {
            assert!(configured[key].is_null(), "{key}");
        }
        assert_eq!(configured["name"], "Kitchen");
        assert_eq!(configured["pages"], serde_json::json!(["desk"]));
        crate::phone_registry::forget(&["p6seen", "p6configured"]);
    }

    #[tokio::test]
    async fn show_on_phone_answers_an_empty_ok_and_stays_local() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let res = send(&app, "POST", "/api/phone/open", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert!(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        s.cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
                None,
            )
            .unwrap();
        let wifi = crate::server::build_wifi_router(s);
        let key = ("authorization", "Bearer phone-key");
        for (method, path) in [("GET", "/api/phones"), ("POST", "/api/phone/open")] {
            assert_eq!(
                send(&wifi, method, path, &[key]).await.status(),
                403,
                "{path}"
            );
        }
    }
}
