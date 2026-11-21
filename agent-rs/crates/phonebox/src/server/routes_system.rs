//! The system routes: `GET /api/network/adapters` (Settings › Ping & network), the process list (`/api/procs*`) and
//! the device batteries (`GET /api/batteries`).

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{AppState, json_response};
use crate::process_monitor::KillResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/network/adapters", get(network_adapters))
        .route("/api/procs", get(procs))
        .route("/api/procs/icon", get(proc_icon))
        .route("/api/procs/detail", get(proc_detail))
        .route("/api/procs/kill", post(proc_kill))
        .route("/api/batteries", get(batteries))
}

/// Battery levels of wireless devices (headphones, mice, keyboards…), for the Device batteries widget.
async fn batteries(State(s): State<AppState>) -> Response {
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "devices": s.batteries.snapshot() }),
    )
}

/// `SystemMonitor.Adapters(cfg.Get().Ping.Adapters)`: the list does not depend on the chosen adapters.
async fn network_adapters() -> Response {
    json_response(StatusCode::OK, &crate::system_monitor::adapters())
}

type QueryList = Query<Vec<(String, String)>>;

use super::query::{int32 as query_int, value as query};

fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// The groups, the most CPU-hungry first (`top`: 40 by default, 1 to 200).
async fn procs(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(top) = query_int(&q, "top") else {
        return bad_request();
    };
    let all = s.procs.snapshot();
    let take = top.unwrap_or(40).clamp(1, 200) as usize;
    let body = serde_json::json!({
        "total": all.iter().map(|g| g.count).sum::<i32>(),
        "groupCount": all.len(),
        "groups": all.iter().take(take).collect::<Vec<_>>(),
    });
    json_response(StatusCode::OK, &body)
}

/// A process group's icon as PNG, or 404.
async fn proc_icon(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(name) = query(&q, "name") else {
        return bad_request();
    };
    let procs = s.procs.clone();
    match tokio::task::spawn_blocking(move || procs.icon(&name))
        .await
        .ok()
        .flatten()
    {
        Some(png) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn proc_detail(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Some(name) = query(&q, "name") else {
        return bad_request();
    };
    let procs = s.procs.clone();
    match tokio::task::spawn_blocking(move || procs.detail(&name)).await {
        Ok(detail) => json_response(StatusCode::OK, &detail),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `?name=X` ends every instance of X; `?pid=N` ends one process together with its children. Only when an enabled
/// processes widget allows it (403 otherwise); nothing ended is a 409.
async fn proc_kill(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(pid) = query_int(&q, "pid") else {
        return bad_request();
    };
    if !s.cfg.get().allow_kill() {
        let refused = KillResult {
            killed: 0,
            failed: 0,
            error: Some("Terminating processes is disabled in the settings".into()),
        };
        return json_response(StatusCode::FORBIDDEN, &refused);
    }
    let name = query(&q, "name").unwrap_or_default();
    let procs = s.procs.clone();
    let result = tokio::task::spawn_blocking(move || match pid {
        Some(id) => match u32::try_from(id) {
            Ok(id) => procs.kill_pid(id),
            Err(_) => KillResult {
                killed: 0,
                failed: 0,
                error: Some("process not found".into()),
            },
        },
        None => procs.kill(&name),
    })
    .await;
    match result {
        Ok(r) if r.killed > 0 => json_response(StatusCode::OK, &r),
        Ok(r) => json_response(StatusCode::CONFLICT, &r),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    use crate::server::build_router;
    use crate::server::guard::tests::{send, test_state};

    #[tokio::test]
    async fn network_adapters_lists_the_up_adapters_as_json() {
        let (_dir, state) = test_state();
        let app = build_router(state);
        let res = send(
            &app,
            "GET",
            "/api/network/adapters",
            &[("host", "127.0.0.1:8765")],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(!list.is_empty(), "{}", String::from_utf8_lossy(&body));
        assert!(
            list.iter()
                .all(|a| a["name"].is_string() && a["auto"].is_boolean())
        );
    }

    async fn call(
        app: &crate::server::App,
        method: &str,
        path: &str,
    ) -> (StatusCode, String, serde_json::Value) {
        let res = send(app, method, path, &[("host", "127.0.0.1:8765")]).await;
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, kind, json)
    }

    #[tokio::test]
    async fn the_process_list_answers_and_refuses_a_bad_top() {
        let (_dir, state) = test_state();
        let app = build_router(state);
        let (status, kind, json) = call(&app, "GET", "/api/procs?top=5").await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::OK, "application/json; charset=utf-8")
        );
        assert!(
            json["total"].is_number()
                && json["groupCount"].is_number()
                && json["groups"].is_array(),
            "{json}"
        );
        assert_eq!(
            call(&app, "GET", "/api/procs?top=abc").await.0,
            StatusCode::BAD_REQUEST
        );
        // ASP.NET: an int? that is present must parse; several values are joined with "," (so not a number)
        assert_eq!(
            call(&app, "GET", "/api/procs?top=").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "GET", "/api/procs?top=1&top=2").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "GET", "/api/procs?top=%205%20").await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn batteries_list_the_devices_and_start_the_reads() {
        let (_dir, state) = test_state();
        let monitor = state.batteries.clone();
        let app = build_router(state);
        let (status, kind, json) = call(&app, "GET", "/api/batteries").await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::OK, "application/json; charset=utf-8")
        );
        assert_eq!(json, serde_json::json!({"devices": []}));
        assert!(monitor.is_wanted());
    }

    #[tokio::test]
    async fn icon_and_detail_need_a_name() {
        let (_dir, state) = test_state();
        let app = build_router(state);
        assert_eq!(
            call(&app, "GET", "/api/procs/icon").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "GET", "/api/procs/detail").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "GET", "/api/procs/icon?name=missing-proc-f390")
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let (_, _, json) = call(&app, "GET", "/api/procs/detail?name=a&name=b").await;
        assert_eq!(json["name"], "a,b");
        let (status, _, json) = call(&app, "GET", "/api/procs/detail?NAME=missing-proc-f390").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json,
            serde_json::json!({"name": "missing-proc-f390", "title": "missing-proc-f390", "locked": false, "instances": []})
        );
    }

    #[tokio::test]
    async fn kill_follows_the_setting_and_reports_conflicts() {
        let (_dir, state) = test_state();
        let cfg = state.cfg.clone();
        let app = build_router(state);
        let page = |allow: bool| {
            serde_json::json!({"pages": [{"id": "p", "title": "P", "icon": "x", "enabled": true,
                "widgets": [{"type": "processes", "w": 4, "h": 4, "options": {"allowKill": allow}}]}]})
            .to_string()
        };
        // a processes widget that does not allow ending processes
        cfg.save(&page(false), None).unwrap();
        let (status, _, json) = call(&app, "POST", "/api/procs/kill?name=missing-proc-f390").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            json,
            serde_json::json!({"killed": 0, "failed": 0, "error": "Terminating processes is disabled in the settings"})
        );
        cfg.save(&page(true), None).unwrap();
        assert!(cfg.get().allow_kill());
        let (status, _, json) = call(&app, "POST", "/api/procs/kill?name=missing-proc-f390").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["error"], "process not found");
        assert_eq!(
            call(&app, "POST", "/api/procs/kill?pid=abc").await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, _, json) = call(&app, "POST", "/api/procs/kill?pid=-5").await;
        assert_eq!(
            (status, json["error"].as_str()),
            (StatusCode::CONFLICT, Some("process not found"))
        );
        let (status, _, json) = call(&app, "POST", "/api/procs/kill").await;
        assert_eq!(
            (status, json["error"].as_str()),
            (StatusCode::CONFLICT, Some("no name given"))
        );
    }
}
