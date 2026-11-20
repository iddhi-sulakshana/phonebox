//! The HTTP server (Kestrel in the C# agent): shared state, routes and the request pipeline of `Program.cs`.

pub mod guard;

use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

use crate::agent_endpoint::AgentEndpoint;
use crate::config::store::ConfigStore;
use crate::push_store::PushStore;
use crate::request_timings::RequestTimings;

/// The whole app: every route behind the timing and guard layers.
pub fn build_router(state: AppState) -> axum::Router {
    guard::apply(axum::Router::new(), state)
}

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ConfigStore>,
    pub endpoint: AgentEndpoint,
    pub push: Arc<PushStore>,
    pub timings: Arc<RequestTimings>,
    pub phone: Arc<PhoneSeenState>,
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
