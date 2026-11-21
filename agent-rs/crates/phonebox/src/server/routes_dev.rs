//! The AI and dev routes: agent sessions and GitHub (`/api/work*`), AI quotas, the Claude Code permission prompts and
//! hook, Docker, and command widgets.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;

use super::query::{boolean as query_bool, value as query};
use super::{AppState, json_response};
use crate::claude_hook;

type QueryList = Query<Vec<(String, String)>>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/work", get(work))
        .route("/api/work/conversation", get(conversation))
        .route("/api/quotas", get(quotas))
        .route("/api/approvals", get(approvals))
        .route("/api/approvals/{id}", post(decide))
        .route(claude_hook::ENDPOINT, post(permission))
        .route(
            "/api/claude/hook",
            get(hook_status).post(hook_install).delete(hook_remove),
        )
        .route("/api/docker", get(docker))
        .route("/api/docker/{id}/{action}", post(docker_act))
        .route("/api/widget/{page}/{widget}", get(widget))
}

/// `Results.Text("")`.
fn empty_text() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "").into_response()
}

#[derive(Serialize)]
struct Error<'a> {
    error: &'a str,
}

async fn work(State(s): State<AppState>) -> Response {
    let github = s
        .github
        .snapshot()
        .and_then(|g| serde_json::to_value(g).ok());
    json_response(
        StatusCode::OK,
        &crate::session_monitor::work_feed(&s.sessions, github),
    )
}

async fn conversation(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(id) = query(&q, "id") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let enabled = s.cfg.get().claude.share_conversation;
    let running = s.sessions.snapshot();
    let (transcripts, home) = (s.transcripts.clone(), s.sessions.home().to_path_buf());
    // the transcript is read from disk: off the async workers
    match tokio::task::spawn_blocking(move || transcripts.for_phone(enabled, &running, &id, &home))
        .await
    {
        Ok(view) => json_response(StatusCode::OK, &view),
        Err(_) => super::unhandled(),
    }
}

async fn quotas(State(s): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Quotas {
        providers: Vec<crate::quota::QuotaProvider>,
        overview: Option<crate::codex_bar::QuotaOverview>,
    }
    let now = crate::server::now_ms();
    json_response(
        StatusCode::OK,
        &Quotas {
            providers: s.quotas.snapshot(now),
            overview: s.quotas.overview(now),
        },
    )
}

async fn approvals(State(s): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Items {
        items: Vec<crate::approval_store::Approval>,
    }
    json_response(
        StatusCode::OK,
        &Items {
            items: s.approvals.snapshot(),
        },
    )
}

async fn decide(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): QueryList,
) -> Response {
    let Some(decision) = query(&q, "decision") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if s.approvals.decide(&id, &decision) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
    .into_response()
}

/// Waits for Allow / Deny from the phone; prints nothing otherwise. The client going away (the user answered in the
/// terminal) drops this future, and the prompt leaves the phone with it.
async fn permission(State(s): State<AppState>, body: Bytes) -> Response {
    // JsonDocument.ParseAsync: UTF-8, an optional byte order mark
    let Ok(text) = std::str::from_utf8(&body) else {
        return empty_text();
    };
    let Ok(input) = crate::json_text::parse(text.strip_prefix('\u{feff}').unwrap_or(text), false)
    else {
        return empty_text();
    };
    match s.hook.handle(&input, std::future::pending()).await {
        Some(output) => (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            output,
        )
            .into_response(),
        None => empty_text(),
    }
}

async fn hook_status(State(s): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Status {
        installed: bool,
        path: String,
    }
    let path = s.claude_settings.clone();
    let installed = tokio::task::spawn_blocking({
        let path = path.clone();
        move || claude_hook::installed(&path)
    })
    .await
    .unwrap_or(false);
    json_response(
        StatusCode::OK,
        &Status {
            installed,
            path: path.display().to_string(),
        },
    )
}

/// Adds or removes the hook: 200, or 400 with why not.
async fn configure(s: AppState, install: bool) -> Response {
    let (port, timeout) = (s.endpoint.port, s.cfg.get().claude.timeout_sec);
    let path = s.claude_settings.clone();
    let error =
        tokio::task::spawn_blocking(move || claude_hook::configure(install, port, timeout, &path))
            .await
            .unwrap_or_else(|e| Some(e.to_string()));
    match error {
        Some(error) => json_response(StatusCode::BAD_REQUEST, &Error { error: &error }),
        None => StatusCode::OK.into_response(),
    }
}

async fn hook_install(State(s): State<AppState>) -> Response {
    configure(s, true).await
}

async fn hook_remove(State(s): State<AppState>) -> Response {
    configure(s, false).await
}

async fn docker(State(s): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Docker {
        snapshot: crate::docker_monitor::DockerSnapshot,
        control: bool,
    }
    let snapshot = s.docker.snapshot_cold().await;
    json_response(
        StatusCode::OK,
        &Docker {
            snapshot,
            control: s.docker.control_allowed(),
        },
    )
}

async fn docker_act(
    State(s): State<AppState>,
    Path((id, action)): Path<(String, String)>,
) -> Response {
    match s.docker.act(&id, &action).await {
        Some(error) => json_response(StatusCode::BAD_REQUEST, &Error { error: &error }),
        None => json_response(StatusCode::OK, &serde_json::json!({ "ok": true })),
    }
}

async fn widget(
    State(s): State<AppState>,
    Path((page, widget)): Path<(String, String)>,
    Query(q): QueryList,
) -> Response {
    let Ok(force) = query_bool(&q, "force") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match s.commands.get(&page, &widget, force == Some(true)) {
        Some(result) => json_response(StatusCode::OK, &result),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
#[path = "routes_dev_tests.rs"]
mod tests;
