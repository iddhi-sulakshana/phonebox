//! `UpdateService`: what the settings page shows about updates, the check against the fork's GitHub releases, and the
//! loop (20 s after start, then every minute: a check every 6 h, or 15 min after a failed one; apply when `updates.auto`
//! is on). An agent on a pre-release follows pre-releases (the newest published one); a stable agent asks for the
//! latest release, as C# did (plan 7's ruling).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;

use super::assets::{Release, property, select_assets};
use super::github::{FetchError, UpdateHttp, is_github_api, transient};
use super::version::{newer, parses};
use crate::config::store::ConfigStore;
use crate::server::{PhoneSeenState, Stop};

pub type GhToken = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// What the service reaches outside itself; fakes in tests.
pub struct UpdateDeps {
    pub http: Arc<dyn UpdateHttp>,
    pub gh_token: GhToken,
    pub installer: Arc<dyn super::phone::PhoneInstaller>,
    /// Where downloads and the staged agent go (`LocalDir/updates`).
    pub downloads: PathBuf,
    pub api_base: String,
    pub agent_version: String,
    pub rid: &'static str,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// This program, as it was at startup (the swap renames it later).
    pub exe: PathBuf,
    /// Where install scripts put the agent; only that copy updates itself.
    pub install_dir: PathBuf,
}

impl UpdateDeps {
    pub fn real() -> UpdateDeps {
        UpdateDeps {
            http: Arc::new(super::github::ReqwestHttp::default()),
            gh_token: Arc::new(|| Box::pin(super::github::gh_token())),
            installer: Arc::new(super::phone::AdbInstaller),
            downloads: crate::app_paths::local_dir().join("updates"),
            api_base: super::github::api_base(std::env::var("PHONEBOX_UPDATE_API").ok().as_deref()),
            agent_version: crate::app_paths::version().to_string(),
            rid: crate::app_paths::rid(),
            now: Arc::new(crate::server::now_ms),
            exe: std::env::current_exe().unwrap_or_default(),
            install_dir: crate::app_paths::install_dir(),
        }
    }
}

/// `UpdateStatus`, in C#'s order with nulls written.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub agent_version: String,
    pub app_version: Option<String>,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub agent_update: bool,
    pub app_update: bool,
    pub last_check: Option<i64>,
    pub state: String,
    pub error: Option<String>,
}

pub(super) struct Inner {
    pub latest: Option<Release>,
    pub last_check: Option<i64>,
    pub state: String,
    pub error: Option<String>,
    pub phone_retry: Vec<String>,
    pub phone_retry_for: Option<String>,
    /// download link → the asset's API link (`_apiUrls`), the fallback when a download fails.
    pub api_urls: HashMap<String, String>,
    /// The phone failure last logged (the same one is not logged every minute).
    pub phone_warning: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            latest: None,
            last_check: None,
            state: "idle".into(),
            error: None,
            phone_retry: vec![],
            phone_retry_for: None,
            api_urls: HashMap::new(),
            phone_warning: None,
        }
    }
}

impl Inner {
    /// `PhoneRetryPending`: phones still wait for the latest app after a failed install.
    pub fn phone_retry_pending(&self) -> bool {
        matches!((&self.latest, &self.phone_retry_for), (Some(l), Some(f)) if &l.version == f)
            && !self.phone_retry.is_empty()
    }
}

/// The check was cut short by the agent's stop (C# rethrew its cancellation).
#[derive(Debug, PartialEq)]
pub struct Stopped;

pub struct UpdateService {
    pub(super) cfg: Arc<ConfigStore>,
    pub(super) adb: Arc<crate::adb::AdbReverse>,
    pub(super) phone: Arc<PhoneSeenState>,
    pub(super) stop: Stop,
    pub(super) deps: UpdateDeps,
    /// `_gate`: one check, apply or install at a time (not reentrant).
    pub(super) gate: tokio::sync::Mutex<()>,
    pub(super) inner: Mutex<Inner>,
}

impl UpdateService {
    pub fn new(
        cfg: Arc<ConfigStore>,
        adb: Arc<crate::adb::AdbReverse>,
        phone: Arc<PhoneSeenState>,
        stop: Stop,
        deps: UpdateDeps,
    ) -> Arc<UpdateService> {
        Arc::new(UpdateService {
            cfg,
            adb,
            phone,
            stop,
            deps,
            gate: tokio::sync::Mutex::new(()),
            inner: Mutex::default(),
        })
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn app_version(&self) -> Option<String> {
        self.phone
            .app_version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `Status()`.
    pub fn status(&self) -> UpdateStatus {
        let app_version = self.app_version();
        let inner = self.lock();
        let latest = inner.latest.as_ref();
        let agent_update = latest
            .is_some_and(|l| newer(&l.version, &self.deps.agent_version) && l.agent_zip.is_some());
        let app_update = latest.is_some_and(|l| {
            l.apk.is_some()
                && (app_version.as_deref().is_some_and(|a| newer(&l.version, a))
                    || inner.phone_retry_pending())
        });
        UpdateStatus {
            agent_version: self.deps.agent_version.clone(),
            app_version,
            latest: latest.map(|l| l.version.clone()),
            release_url: latest.and_then(|l| l.url.clone()),
            agent_update,
            app_update,
            last_check: inner.last_check,
            state: inner.state.clone(),
            error: inner.error.clone(),
        }
    }

    /// `RunningInstalled`: this copy runs from the install folder (only that one replaces itself).
    pub fn installed(&self) -> bool {
        super::migration::running_from(&self.deps.exe, &self.deps.install_dir)
    }

    /// `/api/state`'s `update`: `{latest, app, agent}` while an update is out, else null.
    pub fn state(&self) -> Value {
        let s = self.status();
        if s.app_update || s.agent_update {
            serde_json::json!({ "latest": s.latest, "app": s.app_update, "agent": s.agent_update })
        } else {
            Value::Null
        }
    }

    /// `CheckAsync`: waits for the gate.
    pub async fn check(&self) -> Result<(), Stopped> {
        let _gate = self.gate.lock().await;
        self.check_core().await
    }

    /// Runs `work` unless the agent stops first.
    pub(super) async fn unless_stopped<T>(
        &self,
        work: impl std::future::Future<Output = T>,
    ) -> Result<T, Stopped> {
        tokio::select! {
            out = work => Ok(out),
            _ = self.stop.stopped() => Err(Stopped),
        }
    }

    /// `ApiStringAsync`: anonymous first; after a failure worth retrying, once more with the `gh` CLI's token.
    async fn api_string(&self, url: &str) -> Result<String, FetchError> {
        match self.deps.http.get_text(url, None).await {
            Err(e) if transient(&e, self.stop.is_stopped()) => {
                let Some(token) = (self.deps.gh_token)().await else {
                    return Err(e);
                };
                crate::log::info(
                    "UpdateService",
                    &format!(
                        "GitHub API without a token failed ({}); retrying with the gh CLI's token",
                        e.message()
                    ),
                );
                let token = is_github_api(url).then_some(token.as_str());
                self.deps.http.get_text(url, token).await
            }
            other => other,
        }
    }

    /// `CheckCoreAsync` (the caller holds the gate): the release to follow, its assets, and the status fields; a
    /// failure keeps the release found before and shows `Update check failed: …`.
    pub(super) async fn check_core(&self) -> Result<(), Stopped> {
        self.lock().state = "checking".into();
        let repo = self.cfg.get().updates.repo.clone();
        let pre_release = self.deps.agent_version.contains('-');
        let url = if pre_release {
            format!("{}/repos/{repo}/releases?per_page=30", self.deps.api_base)
        } else {
            format!("{}/repos/{repo}/releases/latest", self.deps.api_base)
        };
        let fetched = self.unless_stopped(self.api_string(&url)).await;
        let result = match fetched {
            Err(Stopped) => {
                self.lock().state = "idle".into();
                return Err(Stopped);
            }
            Ok(Err(e)) => Err(e.message().to_string()),
            Ok(Ok(text)) => serde_json::from_str::<Value>(&text)
                .map_err(|e| e.to_string())
                .and_then(|root| {
                    let release = if pre_release {
                        newest_published(&root)?
                    } else {
                        Some(&root)
                    };
                    match release {
                        None => Ok(None),
                        Some(release) => {
                            let urls = api_urls(release)?;
                            Ok(Some((select_assets(release, self.deps.rid)?, urls)))
                        }
                    }
                }),
        };
        let mut inner = self.lock();
        match result {
            Ok(found) => {
                match found {
                    Some((release, urls)) => {
                        inner.api_urls.extend(urls);
                        inner.latest = Some(release);
                    }
                    None => inner.latest = None,
                }
                inner.last_check = Some((self.deps.now)());
                inner.error = None;
            }
            Err(message) => inner.error = Some(format!("Update check failed: {message}")),
        }
        inner.state = "idle".into();
        Ok(())
    }

    /// The loop; stops with the agent.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            me.cleanup_old_files();
            if me
                .unless_stopped(tokio::time::sleep(Duration::from_secs(20)))
                .await
                .is_err()
            {
                return;
            }
            let mut next_check = 0i64;
            loop {
                if (me.deps.now)() >= next_check {
                    if me.check().await.is_err() {
                        return;
                    }
                    let failed = me
                        .lock()
                        .error
                        .as_deref()
                        .is_some_and(|e| e.starts_with("Update check failed"));
                    let wait = if failed { 15 * 60_000 } else { 6 * 3_600_000 };
                    next_check = (me.deps.now)() + wait;
                }
                if me.cfg.get().updates.auto {
                    me.apply().await;
                }
                if me
                    .unless_stopped(tokio::time::sleep(Duration::from_secs(60)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn cleanup_old_files(&self) {
        super::swap::cleanup(
            &self.deps.install_dir,
            &self.deps.downloads,
            std::time::SystemTime::now(),
        );
    }
}

/// The newest release that is not a draft (a token lists drafts too); None when there is none.
fn newest_published(list: &Value) -> Result<Option<&Value>, String> {
    let Value::Array(items) = list else {
        return Err(format!(
            "The requested operation requires an element of type 'Array', but the target element has type '{}'.",
            if list.is_object() { "Object" } else { "Other" }
        ));
    };
    let tag = |r: &Value| {
        r.get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches(['v', 'V'])
            .to_string()
    };
    let mut best: Option<(&Value, String)> = None;
    for release in items {
        if release.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let version = tag(release);
        if !parses(&version) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| newer(&version, b)) {
            best = Some((release, version));
        }
    }
    Ok(best.map(|(r, _)| r))
}

/// `_apiUrls` from a release: each asset's download link → its API link (both strings).
fn api_urls(release: &Value) -> Result<HashMap<String, String>, String> {
    let mut urls = HashMap::new();
    if let Value::Array(assets) = property(release, "assets")? {
        for asset in assets {
            if let (Some(Value::String(download)), Some(Value::String(api))) =
                (asset.get("browser_download_url"), asset.get("url"))
            {
                urls.insert(download.clone(), api.clone());
            }
        }
    }
    Ok(urls)
}

/// `/api/state`'s `update`: the status once the service exists (main builds the state sources before the state).
#[derive(Default)]
pub struct UpdateSlot(std::sync::OnceLock<Arc<UpdateService>>);

impl UpdateSlot {
    pub fn set(&self, service: Arc<UpdateService>) {
        let _ = self.0.set(service);
    }
}

impl crate::state::StateSource for UpdateSlot {
    fn snapshot(&self) -> Value {
        self.0.get().map_or(Value::Null, |s| s.state())
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
