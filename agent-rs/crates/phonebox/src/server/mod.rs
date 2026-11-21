//! The HTTP server (Kestrel in the C# agent): shared state, routes and the request pipeline of `Program.cs`.

pub mod guard;
pub mod routes_info;
pub mod routes_settings;
pub mod routes_state;
mod routes_system;
pub mod static_files;
#[cfg(test)]
mod tests_task10;

use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

use crate::agent_endpoint::AgentEndpoint;
use crate::config::store::ConfigStore;
use crate::push_store::PushStore;
use crate::request_timings::RequestTimings;

/// The whole app: the `/agent/api` rewrite, then every route behind the timing and guard layers.
pub type App =
    tower::util::MapRequest<axum::Router, fn(axum::extract::Request) -> axum::extract::Request>;

pub fn build_router(state: AppState) -> App {
    let rewrite: fn(axum::extract::Request) -> axum::extract::Request = rewrite_agent_api;
    tower::util::MapRequest::new(routes(state), rewrite)
}

/// `/agent/api/...` becomes `/api/...` (`Program.cs`): the panel in the layout editor's live preview calls the API by
/// the phone's own path. It runs before routing, timing and the guard, so all of them see the new path (the guard
/// must: a cross-site navigation is allowed to pages, never to `/api`). Local port only: the Wi-Fi listener (phase 4)
/// must not apply it.
fn rewrite_agent_api(mut req: axum::extract::Request) -> axum::extract::Request {
    const PREFIX: &str = "/agent/api";
    let path = req.uri().path();
    if crate::browser_guard::starts_with_segments(path, PREFIX) {
        let rest = &path[PREFIX.len()..];
        let target = match req.uri().query() {
            Some(q) => format!("/api{rest}?{q}"),
            None => format!("/api{rest}"),
        };
        let mut parts = req.uri().clone().into_parts();
        if let Ok(pq) = target.parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    req
}

fn routes(state: AppState) -> axum::Router {
    let routes = axum::Router::new()
        .merge(static_files::routes())
        .merge(routes_settings::routes())
        .merge(routes_info::routes())
        .merge(routes_state::routes())
        .merge(routes_system::routes())
        // Kestrel's default request body limit: a settings page with many pages still fits
        .layer(axum::extract::DefaultBodyLimit::max(30_000_000));
    guard::apply(routes, state)
}

/// Serves until [`Stop::stop`]: the live streams end and the server drains its open connections for at most `grace`
/// (ASP.NET's shutdown timeout, 30 s by default), then lets go of the ones still open: a client that stopped reading
/// must not keep the agent from stopping or restarting.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    use std::future::IntoFuture;
    let stop = state.stop.clone();
    let app = axum::ServiceExt::<axum::extract::Request>::into_make_service(build_router(state));
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let stop = stop.clone();
        async move { stop.stopped().await }
    });
    tokio::select! {
        result = server.into_future() => result,
        _ = async { stop.stopped().await; tokio::time::sleep(grace).await } => {
            crate::log::warn("Program", "connections still open after the shutdown grace period were closed");
            Ok(())
        }
    }
}

/// A JSON answer as ASP.NET writes one: `application/json; charset=utf-8`, compact, doubles in .NET's shortest form
/// (1.0 is written as 1).
pub fn json_response<T: serde::Serialize + ?Sized>(
    status: axum::http::StatusCode,
    value: &T,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let text = crate::config::write::to_dotnet_json_compact(value);
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ConfigStore>,
    pub endpoint: AgentEndpoint,
    pub push: Arc<PushStore>,
    pub timings: Arc<RequestTimings>,
    pub phone: Arc<PhoneSeenState>,
    pub adb: Arc<crate::adb::AdbReverse>,
    pub stop: Stop,
    /// The monitors behind `/api/state` and the live stream.
    pub sources: Arc<crate::state::StateSources>,
    /// The live stream's "state" frame, built once for all streams that wake on the same second.
    pub frames: Arc<crate::state_frame_cache::StateFrameCache>,
    /// The PC monitor (`pc` in the state); main starts it and puts the same one in `sources`.
    pub system: Arc<crate::system_monitor::SystemMonitor>,
    /// The ping monitor (`ping` in the state); main starts it and puts the same one in `sources`.
    pub ping: Arc<crate::ping_monitor::PingMonitor>,
}

impl AppState {
    pub fn new(
        cfg: Arc<ConfigStore>,
        endpoint: AgentEndpoint,
        push: Arc<PushStore>,
        sources: Arc<crate::state::StateSources>,
    ) -> AppState {
        let frames = {
            let (sources, cfg, push) = (sources.clone(), cfg.clone(), push.clone());
            let build = move || crate::state::build_state(&sources, &cfg, &push, None);
            Arc::new(crate::state_frame_cache::StateFrameCache::new(
                Box::new(build),
                100,
                None,
            ))
        };
        // not started here: main starts the loop (tests never run adb)
        let adb = crate::adb::AdbReverse::new(
            cfg.clone(),
            endpoint.port,
            Arc::new(crate::adb::ProcessRunner::default()),
        );
        AppState {
            adb,
            system: crate::system_monitor::SystemMonitor::new(cfg.clone()),
            ping: crate::ping_monitor::PingMonitor::new(cfg.clone()),
            cfg,
            endpoint,
            push,
            timings: Default::default(),
            phone: Default::default(),
            stop: Stop::default(),
            sources,
            frames,
        }
    }
}

/// The agent is stopping (Ctrl+C, SIGTERM, or a restart from the settings page). Live streams end on it, so the
/// server's graceful shutdown can finish while a phone is connected.
#[derive(Clone)]
pub struct Stop(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Stop {
    fn default() -> Self {
        Stop(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl Stop {
    pub fn stop(&self) {
        self.0.send_replace(true);
    }
    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }
    /// Returns once [`Stop::stop`] was called (at once if it already was).
    pub async fn stopped(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What the request pipeline learns from the phone's headers.
#[derive(Default)]
pub struct PhoneSeenState {
    /// Unix ms of the phone's last `/api/state` or stream frame (`ApprovalStore.PhoneLastSeen`).
    pub last_seen: AtomicI64,
    /// The phone app's version (`UpdateService.AppVersion`), from X-PhoneBox-App.
    pub app_version: Mutex<Option<String>>,
}
