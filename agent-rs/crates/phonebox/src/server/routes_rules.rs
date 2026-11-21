//! The rule routes (`Program.cs:600-623`): the settings page's Test button and the pause (phone and settings).

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value, json};

use super::{AppState, json_response, query};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rules/test", post(test_rule))
        .route("/api/rules/pause", post(pause))
}

/// Settings › Rules › Test: runs a rule's action once (the rule may be unsaved, and is not normalized).
async fn test_rule(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body = match query::json_object(&headers, &body) {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let Some(mut rule) = bind_rule(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !crate::config::RULE_ACTIONS.contains(&rule.then.as_str()) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "unknown action" }),
        );
    }
    if rule.id.trim().is_empty() {
        rule.id = "test".into();
    }
    let rules = s.rules.clone();
    // a shortcut action runs a program: off the async workers
    let _ = tokio::task::spawn_blocking(move || rules.test(&rule)).await;
    json_response(StatusCode::OK, &json!({ "ok": true }))
}

/// The rule's members as ASP.NET's web defaults bind them: names in any case (the last of a name wins), unknown ones
/// ignored, and a number may come as text. A null name or id reads as empty (C# threw on a null name).
fn bind_rule(body: Map<String, Value>) -> Option<crate::config::RuleConfig> {
    const FIELDS: [&str; 22] = [
        "id",
        "name",
        "enabled",
        "when",
        "match",
        "value",
        "then",
        "page",
        "text",
        "level",
        "key",
        "pushValue",
        "whileActive",
        "shortcut",
        "priority",
        "forSec",
        "cooldownMin",
        "days",
        "during",
        "countdownSec",
        "op",
        "arg",
    ];
    let mut out = Map::new();
    for (key, value) in body {
        let Some(name) = FIELDS.iter().find(|f| f.eq_ignore_ascii_case(&key)) else {
            continue;
        };
        let value = match (*name, value) {
            ("id" | "name" | "when" | "then", Value::Null) => {
                out.remove(*name);
                continue;
            }
            ("priority" | "forSec" | "cooldownMin" | "countdownSec", Value::String(t)) => {
                Value::from(t.parse::<i32>().ok()?)
            }
            ("value", Value::String(t)) => {
                let number = t
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && !t.contains(['i', 'I', 'n', 'N']))?;
                Value::from(number)
            }
            (_, value) => value,
        };
        out.insert(name.to_string(), value);
    }
    serde_json::from_value(Value::Object(out)).ok()
}

/// Pauses all rules for a while (the phone's overview, the settings); `minutes=0` resumes, no minutes is an hour.
async fn pause(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(minutes) = query::int32(&q, "minutes") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    s.rules.pause(minutes.unwrap_or(60));
    json_response(
        StatusCode::OK,
        &json!({ "pausedUntil": s.rules.paused_until() }),
    )
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    async fn post(
        app: &crate::server::App,
        path: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> (u16, Value) {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header(LOCAL.0, LOCAL.1);
        if let Some(t) = content_type {
            req = req.header("content-type", t);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn the_test_button_runs_an_unsaved_rules_action_once() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let json_type = Some("application/json");
        assert_eq!(
            post(&app, "/api/rules/test", Some("text/plain"), "{}")
                .await
                .0,
            415
        );
        assert_eq!(post(&app, "/api/rules/test", None, "{}").await.0, 415);
        for body in ["", "not json", "null", "[]"] {
            assert_eq!(
                post(&app, "/api/rules/test", json_type, body).await.0,
                400,
                "{body}"
            );
        }
        assert_eq!(
            post(&app, "/api/rules/test", json_type, r#"{"then":"dance"}"#).await,
            (400, json!({"error": "unknown action"}))
        );
        // names in any case, a number as text; a blank id is "test"; "while active" is on unless the rule says otherwise
        let seq = s.push.seq();
        assert_eq!(
            post(
                &app,
                "/api/rules/test",
                json_type,
                r#"{"Then":"goto","PAGE":"work","priority":"5","id":" ","name":null}"#
            )
            .await,
            (200, json!({"ok": true}))
        );
        let events = s.push.since(seq);
        let goto = events.iter().find(|e| e.kind == "goto").expect("a goto");
        assert_eq!(
            goto.data,
            Some(
                json!({"page": "work", "rule": "test", "hold": true, "name": null, "detail": "test",
                        "priority": 5, "countdown": null})
            )
        );
        // a field of the wrong kind does not bind
        assert_eq!(
            post(
                &app,
                "/api/rules/test",
                json_type,
                r#"{"then":"notify","priority":"high"}"#
            )
            .await
            .0,
            400
        );
    }

    #[tokio::test]
    async fn rules_pause_for_an_hour_by_default_and_zero_resumes() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s.clone());
        let read = |res: axum::http::Response<Body>| async move {
            let status = res.status().as_u16();
            let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        };
        let before = crate::server::now_ms();
        let (status, body) = read(send(&app, "POST", "/api/rules/pause", &[LOCAL]).await).await;
        assert_eq!(status, 200);
        let until = body["pausedUntil"].as_i64().unwrap();
        assert!(until >= before + 3_600_000 && until <= crate::server::now_ms() + 3_600_000);
        assert_eq!(s.rules.paused_until(), Some(until));
        assert_eq!(
            read(send(&app, "POST", "/api/rules/pause?minutes=0", &[LOCAL]).await).await,
            (200, json!({"pausedUntil": null}))
        );
        assert_eq!(
            send(&app, "POST", "/api/rules/pause?minutes=soon", &[LOCAL])
                .await
                .status(),
            400
        );
    }
}
