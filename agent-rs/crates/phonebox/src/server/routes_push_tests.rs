//! The webhook routes and the note route (route behavior from `Program.cs`, survey-d §2).

use axum::body::{Body, to_bytes};
use axum::http::{Request, Response};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::server::guard::tests::{send, test_state};
use crate::server::{AppState, build_router};

const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

async fn call(
    app: &crate::server::App,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &str,
) -> Response<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:8765");
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn text(res: Response<Body>) -> String {
    String::from_utf8(
        to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn json_of(res: Response<Body>) -> Value {
    serde_json::from_str(&text(res).await).unwrap()
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

fn app() -> (tempfile::TempDir, AppState, crate::server::App) {
    let (d, s) = test_state();
    let app = build_router(s.clone());
    (d, s, app)
}

#[tokio::test]
async fn a_pushed_value_round_trips_with_the_web_shape() {
    let (_d, s, app) = app();
    let res = call(&app, "POST", "/api/push/temp?value=21.5", None, "").await;
    assert_eq!(res.status(), 200);
    let ok = json_of(res).await;
    assert_eq!(keys(&ok), ["ok", "key", "version"]);
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["key"], "temp");
    assert_eq!(ok["version"], s.push.version());
    let one = json_of(send(&app, "GET", "/api/push/temp", &[LOCAL]).await).await;
    assert_eq!(keys(&one), ["data", "at", "expiresAt"]);
    assert_eq!(one["data"], 21.5);
    assert!(one["expiresAt"].is_null());
    let all = json_of(send(&app, "GET", "/api/push", &[LOCAL]).await).await;
    assert_eq!(keys(&all), ["version", "items"]);
    assert_eq!(all["items"]["temp"]["data"], 21.5);
    // PUT does the same, the body as plain text
    let res = call(
        &app,
        "PUT",
        "/api/push/door",
        Some("text/plain"),
        "  open \n",
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(s.push.get("door").unwrap().data, json!("open"));
    let res = send(&app, "DELETE", "/api/push/temp", &[LOCAL]).await;
    assert_eq!(res.status(), 200);
    assert!(text(res).await.is_empty());
    let res = send(&app, "DELETE", "/api/push/temp", &[LOCAL]).await;
    assert_eq!(res.status(), 404);
    assert!(text(res).await.is_empty());
    assert_eq!(
        send(&app, "GET", "/api/push/temp", &[LOCAL]).await.status(),
        404
    );
    let before = s.push.version();
    let res = send(&app, "DELETE", "/api/push", &[LOCAL]).await;
    assert_eq!(res.status(), 200);
    assert!(s.push.snapshot().is_empty());
    assert_ne!(s.push.version(), before);
}

#[tokio::test]
async fn push_errors_are_answered() {
    let (_d, _s, app) = app();
    let res = call(
        &app,
        "POST",
        "/api/push/x",
        Some("application/json"),
        "{ nope",
    )
    .await;
    assert_eq!(res.status(), 400);
    let e = json_of(res).await;
    assert!(
        e["error"].as_str().unwrap().starts_with("invalid JSON: "),
        "{e}"
    );
    let res = call(&app, "POST", "/api/push/has%20space?value=1", None, "").await;
    assert_eq!(res.status(), 400);
    assert_eq!(
        json_of(res).await,
        json!({"error": "key must be 1-64 characters: letters, digits, '.', '_' or '-'"})
    );
    let res = call(&app, "POST", "/api/push/x?ttl=abc&value=1", None, "").await;
    assert_eq!(res.status(), 400);
    assert!(text(res).await.is_empty());
    let res = call(&app, "POST", "/api/push/x?ttl=60&value=1", None, "").await;
    assert_eq!(res.status(), 200);
    let one = json_of(send(&app, "GET", "/api/push/x", &[LOCAL]).await).await;
    assert!(one["expiresAt"].as_i64().unwrap() > one["at"].as_i64().unwrap());
    let big = "x".repeat(65_537);
    let res = call(&app, "POST", "/api/push/x", Some("text/plain"), &big).await;
    assert_eq!(res.status(), 400);
    assert_eq!(
        json_of(res).await,
        json!({"error": "body too large (max 64 KB)"})
    );
}

#[tokio::test]
async fn notify_sends_the_payload_or_a_text() {
    let (_d, s, app) = app();
    let res = call(
        &app,
        "POST",
        "/api/notify",
        Some("application/json"),
        r#"{"title":"Hi","level":"ok"}"#,
    )
    .await;
    assert_eq!(res.status(), 200);
    let event = json_of(res).await;
    assert_eq!(keys(&event), ["seq", "kind", "data", "at"]);
    assert_eq!(event["kind"], "notify");
    assert_eq!(event["data"], json!({"title": "Hi", "level": "ok"}));
    assert_eq!(s.push.since(0).last().unwrap().kind, "notify");
    // JsonElement.ToString(): a bool is "True", JSON null is ""
    for (ct, body, shown) in [
        ("text/plain", "true", "True"),
        ("text/plain", "door open", "door open"),
        ("text/plain", "42", "42"),
        ("application/json", "null", ""),
        ("application/json", "[1, 2]", "[1,2]"),
    ] {
        let event = json_of(call(&app, "POST", "/api/notify", Some(ct), body).await).await;
        assert_eq!(event["data"], json!({ "text": shown }), "{body}");
    }
    let res = call(
        &app,
        "POST",
        "/api/notify",
        Some("application/json"),
        r#"{"level":"ok"}"#,
    )
    .await;
    assert_eq!(res.status(), 400);
    assert_eq!(
        json_of(res).await,
        json!({"error": "give a text or a title"})
    );
    let res = call(&app, "POST", "/api/notify?value=hello", None, "").await;
    assert_eq!(json_of(res).await["data"], json!({"text": "hello"}));
}

#[tokio::test]
async fn goto_and_refresh_send_their_events() {
    let (_d, _s, app) = app();
    let event = json_of(call(&app, "POST", "/api/goto/work?countdown=99", None, "").await).await;
    assert_eq!(event["kind"], "goto");
    assert_eq!(event["data"], json!({"page": "work", "countdown": 30}));
    let event = json_of(call(&app, "POST", "/api/goto/2?countdown=-4", None, "").await).await;
    assert_eq!(event["data"], json!({"page": "2", "countdown": 0}));
    let event = json_of(call(&app, "POST", "/api/goto/work", None, "").await).await;
    assert_eq!(event["data"], json!({"page": "work", "countdown": null}));
    assert_eq!(
        call(&app, "POST", "/api/goto/work?countdown=x", None, "")
            .await
            .status(),
        400
    );
    let wakes = crate::wake::count();
    let event = json_of(call(&app, "POST", "/api/refresh", None, "").await).await;
    assert_eq!(event["kind"], "refresh");
    assert!(event["data"].is_null());
    assert!(crate::wake::count() > wakes);
    let event = json_of(call(&app, "POST", "/api/refresh?reload=true", None, "").await).await;
    assert_eq!(event["kind"], "reload");
    // bool? does not bind "1" in ASP.NET: the settings page's Reload button gets a 400 from C# too
    let res = call(&app, "POST", "/api/refresh?reload=1", None, "").await;
    assert_eq!(res.status(), 400);
    assert!(text(res).await.is_empty());
}

#[tokio::test]
async fn the_note_route_saves_only_a_notes_text() {
    let (_d, s, app) = app();
    s.cfg
        .save(
            r#"{"pages":[{"id":"p","title":"P","widgets":[{"id":"n","type":"note","options":{"text":"old"}},{"id":"c","type":"clock"}]}]}"#,
            None,
        )
        .unwrap();
    let res = call(&app, "POST", "/api/widget/p/n/note?text=hello", None, "").await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        json_of(res).await,
        json!({"version": s.cfg.version().to_string()})
    );
    let res = call(&app, "POST", "/api/widget/p/missing/note?text=x", None, "").await;
    assert_eq!(res.status(), 404);
    assert_eq!(json_of(res).await, json!({"error": "widget not found"}));
    let res = call(
        &app,
        "POST",
        "/api/widget/p/n/note?text=x&version=1",
        None,
        "",
    )
    .await;
    assert_eq!(res.status(), 409);
    assert_eq!(json_of(res).await["error"], crate::config::store::CONFLICT);
    // a version that is not a number is no version
    let res = call(
        &app,
        "POST",
        "/api/widget/p/n/note?text=x&version=abc",
        None,
        "",
    )
    .await;
    assert_eq!(res.status(), 200);
    let res = call(&app, "POST", "/api/widget/p/n/note", None, "").await;
    assert_eq!(res.status(), 400);
    assert_eq!(json_of(res).await, json!({"error": "text expected"}));
    let res = call(&app, "POST", "/api/widget/p/c/note?text=x", None, "").await;
    assert_eq!(json_of(res).await, json!({"error": "option not allowed"}));
}

#[tokio::test]
async fn webhooks_take_the_webhook_key_over_wifi() {
    let (_d, s) = test_state();
    s.cfg
        .save(
            r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key","webhookToken":"hook-key"}}"#,
            None,
        )
        .unwrap();
    let wifi = crate::server::build_wifi_router(s);
    let hook = ("authorization", "Bearer hook-key");
    let phone = ("authorization", "Bearer phone-key");
    assert_eq!(send(&wifi, "GET", "/api/push", &[hook]).await.status(), 200);
    assert_eq!(
        send(&wifi, "POST", "/api/push/door?value=open", &[hook])
            .await
            .status(),
        200
    );
    assert_eq!(
        send(&wifi, "POST", "/api/refresh", &[hook]).await.status(),
        200
    );
    assert_eq!(send(&wifi, "GET", "/api/push", &[]).await.status(), 403);
    // the note route is the phone's, not the webhook's
    assert_eq!(
        send(&wifi, "POST", "/api/widget/p/n/note?text=x", &[hook])
            .await
            .status(),
        403
    );
    assert_ne!(
        send(&wifi, "POST", "/api/widget/p/n/note?text=x", &[phone])
            .await
            .status(),
        403
    );
}
