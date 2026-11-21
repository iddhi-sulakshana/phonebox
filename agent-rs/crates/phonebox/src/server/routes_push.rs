//! The webhook API (`Program.cs:430-466`): pushed values, notifications, page switches and refreshes from scripts and
//! other machines; and the phone's one settings write, a Note widget's text (`Program.cs:382-396`).

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};

use super::query;
use super::{AppState, json_response};

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/push", get(list).delete(clear))
        .route(
            "/api/push/{key}",
            get(one).post(set).put(set).delete(remove),
        )
        .route("/api/notify", post(notify))
        .route("/api/goto/{page}", post(goto))
        .route("/api/refresh", post(refresh))
        .route("/api/widget/{pageId}/{widgetId}/note", post(note))
}

/// A pushed value as the API shows it (web options: camelCase, nulls written); `data.json` keeps .NET's PascalCase.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemView<'a> {
    data: &'a Value,
    at: i64,
    expires_at: Option<i64>,
}

#[derive(Serialize)]
struct Error<'a> {
    error: &'a str,
}

async fn list(State(s): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct List<'a> {
        version: i64,
        items: indexmap::IndexMap<&'a str, ItemView<'a>>,
    }
    let snapshot = s.push.snapshot();
    let items = snapshot
        .iter()
        .map(|(k, v)| (k.as_str(), view(v)))
        .collect();
    json_response(
        StatusCode::OK,
        &List {
            version: s.push.version(),
            items,
        },
    )
}

fn view(item: &crate::push_store::PushItem) -> ItemView<'_> {
    ItemView {
        data: &item.data,
        at: item.at,
        expires_at: item.expires_at,
    }
}

/// The body (or `?value=`) as a pushed value, or why not (a 400).
fn body_value(q: &[(String, String)], headers: &HeaderMap, body: &Bytes) -> Result<Value, String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    crate::push_store::read_body(
        query::value(q, "value").as_deref(),
        content_type,
        length,
        body,
    )
}

fn bad_request(error: &str) -> Response {
    json_response(StatusCode::BAD_REQUEST, &Error { error })
}

async fn one(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    match s.push.get(&key) {
        Some(item) => json_response(StatusCode::OK, &view(&item)),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn set(
    State(s): State<AppState>,
    Path(key): Path<String>,
    Query(q): QueryList,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(ttl) = query::int32(&q, "ttl") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let data = match body_value(&q, &headers, &body) {
        Ok(data) => data,
        Err(error) => return bad_request(&error),
    };
    match s.push.set(&key, data, ttl) {
        Ok(()) => json_response(
            StatusCode::OK,
            &json!({ "ok": true, "key": key, "version": s.push.version() }),
        ),
        Err(error) => json_response(StatusCode::BAD_REQUEST, &Error { error: &error }),
    }
}

async fn remove(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    if s.push.remove(&key) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
    .into_response()
}

async fn clear(State(s): State<AppState>) -> Response {
    s.push.clear();
    StatusCode::OK.into_response()
}

async fn notify(
    State(s): State<AppState>,
    Query(q): QueryList,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let data = match body_value(&q, &headers, &body) {
        Ok(data) => data,
        Err(error) => return bad_request(&error),
    };
    // an object is the payload as sent; anything else is its text
    let payload = match data {
        Value::Object(_) => data,
        other => json!({ "text": crate::push_store::json_element_text(&other) }),
    };
    if payload.get("text").is_none() && payload.get("title").is_none() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &Error {
                error: "give a text or a title",
            },
        );
    }
    json_response(StatusCode::OK, &s.push.emit("notify", Some(payload)))
}

async fn goto(
    State(s): State<AppState>,
    Path(page): Path<String>,
    Query(q): QueryList,
) -> Response {
    let Ok(countdown) = query::int32(&q, "countdown") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // ?countdown=0 switches at once; otherwise the phone counts down like for a rule
    let data = json!({ "page": page, "countdown": countdown.map(|c| c.clamp(0, 30)) });
    json_response(StatusCode::OK, &s.push.emit("goto", Some(data)))
}

async fn refresh(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(reload) = query::boolean(&q, "reload") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    crate::wake::now(); // quotas, GitHub and calendars refresh now
    let kind = if reload == Some(true) {
        "reload"
    } else {
        "refresh"
    };
    json_response(StatusCode::OK, &s.push.emit(kind, None))
}

async fn note(
    State(s): State<AppState>,
    Path((page_id, widget_id)): Path<(String, String)>,
    Query(q): QueryList,
) -> Response {
    // the text travels in the query string: the Android proxy forwards no request body
    let text = query::value(&q, "text");
    let expected = query::value(&q, "version").and_then(|v| {
        v.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
            .parse::<i64>()
            .ok()
    });
    // the same lock as the settings save and the icon commits
    let r = {
        let _commit = super::ICON_COMMIT.lock().unwrap_or_else(|e| e.into_inner());
        crate::note_edit::apply(&s.cfg, &page_id, &widget_id, text.as_deref(), expected)
    };
    let error = r.error.as_deref().unwrap_or_default();
    match r.status {
        200 => json_response(StatusCode::OK, &json!({ "version": r.version })),
        404 => json_response(StatusCode::NOT_FOUND, &Error { error }),
        409 => json_response(StatusCode::CONFLICT, &Error { error }),
        _ => json_response(StatusCode::BAD_REQUEST, &Error { error }),
    }
}

#[cfg(test)]
#[path = "routes_push_tests.rs"]
mod tests;
