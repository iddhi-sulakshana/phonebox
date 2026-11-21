//! Port of `QuotaMonitor.cs`: quota usage of the AI subscriptions (Claude, Codex, Antigravity, and what CodexBar adds).

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod connect;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::codex_bar::{self, CodexBarError, QuotaOverview};
use crate::config::store::ConfigStore;
use crate::dotnet_text::{utf16_len, utf16_prefix};
use connect::UsageHttp;

pub(crate) const CATEGORY: &str = "PhoneBox.Agent.QuotaMonitor";

/// One quota window. `reset_description` is the provider's own reset text; `delta_percent` is used minus the expected
/// pace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaLimit {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub window_sec: Option<i64>,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub reset_description: Option<String>,
    #[serde(default)]
    pub delta_percent: Option<f64>,
    #[serde(default)]
    pub expected_percent: Option<f64>,
    #[serde(default)]
    pub pace_on_track: Option<bool>,
}

impl QuotaLimit {
    pub fn new(
        label: &str,
        percent: f64,
        resets_at: Option<i64>,
        window_sec: Option<i64>,
        severity: &str,
    ) -> Self {
        QuotaLimit {
            label: label.into(),
            percent,
            resets_at,
            window_sec,
            severity: severity.into(),
            reset_description: None,
            delta_percent: None,
            expected_percent: None,
            pace_on_track: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexUsage {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub total_input: Option<i64>,
    #[serde(default)]
    pub total_cached: Option<i64>,
    #[serde(default)]
    pub total_output: Option<i64>,
    #[serde(default)]
    pub total_reasoning: Option<i64>,
    #[serde(default)]
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub last_input: Option<i64>,
    #[serde(default)]
    pub last_output: Option<i64>,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub updated_at: i64,
}

fn local() -> String {
    "local".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaProvider {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub limits: Vec<QuotaLimit>,
    #[serde(default)]
    pub extra: Option<String>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub codex: Option<CodexUsage>,
    /// `"local"` or `"codexbar"`.
    #[serde(default = "local")]
    pub source: String,
}

impl QuotaProvider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        name: &str,
        plan: &str,
        limits: Vec<QuotaLimit>,
        extra: Option<String>,
        updated_at: Option<i64>,
        error: Option<String>,
    ) -> Self {
        QuotaProvider {
            id: id.into(),
            name: name.into(),
            plan: plan.into(),
            limits,
            extra,
            updated_at,
            error,
            codex: None,
            source: local(),
        }
    }
}

/// A window whose reset time has passed started over: the last reading (kept while the tool is not used) no longer
/// applies.
pub fn rolled(p: QuotaProvider, now: i64) -> QuotaProvider {
    let reset = |l: &QuotaLimit| l.resets_at.is_some_and(|r| r <= now);
    if !p.limits.iter().any(reset) {
        return p;
    }
    let limits: Vec<QuotaLimit> = p
        .limits
        .into_iter()
        .map(|l| {
            if reset(&l) {
                QuotaLimit::new(&l.label, 0.0, None, l.window_sec, "normal")
            } else {
                l
            }
        })
        .collect();
    const REACHED: &str = "Limit reached";
    // "Limit reached · Credits: 5" → "Credits: 5" once nothing is used up any more
    let extra = match p.extra {
        Some(x) if x.starts_with(REACHED) && !limits.iter().any(|l| l.percent >= 100.0) => {
            let cut = REACHED.len() + 3;
            (utf16_len(&x) > cut).then(|| x[utf16_prefix(&x, cut).len()..].to_string())
        }
        other => other,
    };
    QuotaProvider { limits, extra, ..p }
}

/// What the monitor knows; the Claude schedule fields join in with the schedule itself.
#[derive(Default)]
pub(crate) struct State {
    pub claude: Option<QuotaProvider>,
    pub codex: Option<QuotaProvider>,
    pub antigravity: Option<QuotaProvider>,
    pub external: Vec<QuotaProvider>,
    pub overview: Option<QuotaOverview>,
}

pub struct QuotaMonitor {
    pub(crate) cfg: Option<Arc<ConfigStore>>,
    /// `claude-quota.json` in the local data folder; `claude-quota-schedule.json` sits beside it.
    pub(crate) claude_cache_path: PathBuf,
    /// Claude Code's credentials file (read only).
    pub(crate) credentials_path: PathBuf,
    pub(crate) codex_sessions_root: PathBuf,
    pub(crate) http: UsageHttp,
    /// Claude Code's macOS Keychain item; None uses the `security` tool, and only for the default credentials path on
    /// macOS. Tests give a reader of their own so nothing touches the real Keychain.
    pub(crate) read_keychain: Option<claude::KeychainReader>,
    /// The Claude schedule; held for a whole pass (one pass at a time).
    pub(crate) claude_sched: tokio::sync::Mutex<claude::ClaudeSched>,
    pub(crate) state: Mutex<State>,
}

impl QuotaMonitor {
    /// The real files under `local_dir` and `home`, the real network.
    pub fn new(cfg: Option<Arc<ConfigStore>>, local_dir: &Path, home: &Path) -> Self {
        QuotaMonitor {
            cfg,
            claude_cache_path: local_dir.join("claude-quota.json"),
            credentials_path: home.join(".claude").join(".credentials.json"),
            codex_sessions_root: home.join(".codex").join("sessions"),
            http: connect::usage_http(),
            read_keychain: None,
            claude_sched: Default::default(),
            state: Mutex::new(State::default()),
        }
    }

    /// The providers to show at `now`: for Claude, Codex and Antigravity the local reading or CodexBar's, then every
    /// other provider CodexBar reports; windows whose reset has passed show as reset.
    pub fn snapshot(&self, now: i64) -> Vec<QuotaProvider> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let external = |id: &str| state.external.iter().find(|p| p.id == id);
        let core = [
            ("claude", &state.claude),
            ("codex", &state.codex),
            ("antigravity", &state.antigravity),
        ]
        .into_iter()
        .filter_map(|(id, local)| codex_bar::select(local.as_ref(), external(id), now));
        let others = state
            .external
            .iter()
            .filter(|p| !matches!(p.id.as_str(), "claude" | "codex" | "antigravity"))
            .filter_map(|p| codex_bar::select(None, Some(p), now));
        core.chain(others).map(|p| rolled(p, now)).collect()
    }

    /// True when an enabled page shows a quota widget with the cost overview on.
    pub fn is_overview_enabled(&self) -> bool {
        self.cfg
            .as_ref()
            .is_some_and(|c| c.get().show_quota_overview())
    }

    /// The cost overview while enabled and at most an hour old.
    pub fn overview(&self, now: i64) -> Option<QuotaOverview> {
        let overview = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .overview
            .clone();
        codex_bar::select_overview(overview, self.is_overview_enabled(), now)
    }

    /// Refreshes the cost overview once when enabled; without a new one, an overview older than an hour is dropped.
    /// True when a new overview came in.
    pub async fn refresh_overview_once<R, F>(
        &self,
        find_tool: impl FnOnce() -> Option<PathBuf>,
        read_cost: R,
        now: i64,
    ) -> bool
    where
        R: FnOnce(PathBuf) -> F,
        F: std::future::Future<Output = Result<Option<QuotaOverview>, CodexBarError>>,
    {
        if !self.is_overview_enabled() {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .overview = None;
            return false;
        }
        let mut latest = None;
        if let Some(path) = find_tool() {
            match read_cost(path).await {
                Ok(read) => latest = read,
                Err(e) => crate::log::debug(CATEGORY, &format!("CodexBar cost: {e}")),
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if latest.is_some() {
            state.overview = latest;
            return true;
        }
        if state
            .overview
            .as_ref()
            .is_some_and(|old| now - old.updated_at > codex_bar::OVERVIEW_STALE_MS)
        {
            state.overview = None;
        }
        false
    }

    /// One pass for Codex (blocking: it walks the sessions folder). A busy log keeps the last reading.
    pub fn codex_tick(&self) {
        let read = codex::read_codex(&self.codex_sessions_root);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match read {
            Ok(reading) => state.codex = reading,
            // the log is busy for a moment; it's read again in 30 s
            Err(e) if state.codex.as_ref().is_some_and(|c| !c.limits.is_empty()) => {
                crate::log::debug(CATEGORY, &format!("codex quota: {e}"));
            }
            Err(e) => {
                state.codex = Some(QuotaProvider::new(
                    "codex",
                    "Codex",
                    "Codex",
                    Vec::new(),
                    None,
                    None,
                    Some(e.to_string()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_bar::QuotaSubscriptionCost;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    fn overview(tokens: i64, cost: f64, updated_at: i64) -> QuotaOverview {
        QuotaOverview {
            total_tokens: tokens,
            total_cost: cost,
            currency_code: "USD".into(),
            window_days: 30,
            provider_count: 1,
            updated_at,
            subscriptions: vec![QuotaSubscriptionCost {
                id: "claude".into(),
                name: "Claude".into(),
                tokens,
                cost,
                currency_code: "USD".into(),
                session_cost: 0.0,
                session_tokens: 0,
            }],
            top_models: vec![],
        }
    }

    /// Writes the settings with a later write time, so the store reads them again.
    fn write_cfg(path: &Path, json: &str, bump: u64) {
        std::fs::write(path, json).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(bump))
            .unwrap();
    }

    fn codexbar() -> Option<PathBuf> {
        Some("codexbar".into())
    }

    #[tokio::test]
    async fn overview_skips_polling_unless_show_overview_enabled_including_stack_children() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        write_cfg(
            &cfg_path,
            r#"{"pages":[{"id":"ai","enabled":true,"widgets":[{"id":"q1","type":"codexbar","options":{}}]}]}"#,
            1,
        );
        let cfg = Arc::new(ConfigStore::new(cfg_path.clone()));
        let monitor = QuotaMonitor::new(Some(cfg.clone()), dir.path(), dir.path());
        let calls = AtomicUsize::new(0);
        let fake_read = |_: PathBuf| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(Some(overview(100, 1.5, 1000))) }
        };

        assert!(!cfg.get().show_quota_overview());
        assert!(
            !monitor
                .refresh_overview_once(codexbar, fake_read, 1000)
                .await
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        write_cfg(
            &cfg_path,
            r#"{"pages":[
              {"id":"home","enabled":true,"widgets":[]},
              {"id":"ai","enabled":false,"widgets":[{"id":"q1","type":"codexbar","options":{"showOverview":true}}]}
            ]}"#,
            2,
        );
        assert!(!cfg.get().show_quota_overview());
        assert!(
            !monitor
                .refresh_overview_once(codexbar, fake_read, 1000)
                .await
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        write_cfg(
            &cfg_path,
            r#"{"pages":[{"id":"ai","enabled":true,"widgets":[
              {"id":"s1","type":"stack","options":{"widgets":[{"id":"c1","type":"codexbar","enabled":true,"options":{"showOverview":true}}]}}
            ]}]}"#,
            3,
        );
        assert!(cfg.get().show_quota_overview());
        assert!(
            monitor
                .refresh_overview_once(codexbar, fake_read, 1000)
                .await
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(monitor.overview(2000).is_some());

        write_cfg(
            &cfg_path,
            r#"{"pages":[{"id":"ai","enabled":true,"widgets":[{"id":"q1","type":"codexbar","options":{"showOverview":true}}]}]}"#,
            4,
        );
        assert!(cfg.get().show_quota_overview());
        assert!(
            monitor
                .refresh_overview_once(codexbar, fake_read, 3000)
                .await
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn overview_expires_after_one_hour_without_successful_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        write_cfg(
            &cfg_path,
            r#"{"pages":[{"id":"ai","enabled":true,"widgets":[{"id":"q1","type":"codexbar","options":{"showOverview":true}}]}]}"#,
            1,
        );
        let monitor = QuotaMonitor::new(
            Some(Arc::new(ConfigStore::new(cfg_path))),
            dir.path(),
            dir.path(),
        );
        const T0: i64 = 1_000_000;
        assert!(
            monitor
                .refresh_overview_once(codexbar, |_| async { Ok(Some(overview(500, 5.0, T0))) }, T0)
                .await
        );

        let at59 = T0 + 59 * 60_000;
        assert!(
            !monitor
                .refresh_overview_once(|| None, |_| async { Ok(None) }, at59)
                .await
        );
        assert!(monitor.overview(at59).is_some());

        let at61 = T0 + 61 * 60_000;
        assert!(monitor.overview(at61).is_none());
        assert!(
            !monitor
                .refresh_overview_once(
                    codexbar,
                    |_| async { Err(CodexBarError::Run("timeout".into())) },
                    at61
                )
                .await
        );
        assert!(monitor.overview(T0).is_none());
    }

    #[test]
    fn codexbar_readings_join_the_local_ones_and_add_other_providers() {
        let now = 10_000_000;
        let dir = tempfile::tempdir().unwrap();
        let monitor = QuotaMonitor::new(None, dir.path(), dir.path());
        let limit = || vec![QuotaLimit::new("Weekly", 5.0, None, None, "normal")];
        let external = |id: &str| QuotaProvider {
            source: "codexbar".into(),
            ..QuotaProvider::new(id, id, id, limit(), None, Some(now), None)
        };
        {
            let mut state = monitor.state.lock().unwrap();
            state.claude = Some(QuotaProvider::new(
                "claude",
                "Claude",
                "Max",
                limit(),
                None,
                Some(now),
                None,
            ));
            state.external = vec![external("cursor"), external("codex"), external("claude")];
        }
        let shown: Vec<(String, String)> = monitor
            .snapshot(now)
            .into_iter()
            .map(|p| (p.id, p.source))
            .collect();
        assert_eq!(
            shown,
            [
                ("claude", "codexbar"),
                ("codex", "codexbar"),
                ("cursor", "codexbar")
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn quota_limit_whose_window_has_reset_is_not_reported_as_used_up() {
        let now = crate::server::now_ms();
        // Codex: the last rollout record said 100 % of the 5-hour window, resetting two hours ago; nothing was written
        // since (nobody runs Codex while the panel says the limit is reached)
        let codex = QuotaProvider::new(
            "codex",
            "Codex",
            "Plus",
            vec![
                QuotaLimit::new(
                    "5-hour",
                    100.0,
                    Some(now - 2 * 3_600_000),
                    Some(18000),
                    "critical",
                ),
                QuotaLimit::new(
                    "Weekly",
                    40.0,
                    Some(now + 3 * 86_400_000),
                    Some(604800),
                    "normal",
                ),
            ],
            Some("Limit reached".into()),
            Some(now - 7 * 3_600_000),
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        let monitor = QuotaMonitor::new(None, dir.path(), dir.path());
        monitor.state.lock().unwrap().codex = Some(codex);

        let shown = monitor
            .snapshot(now)
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        let session = shown.limits.iter().find(|l| l.label == "5-hour").unwrap();
        assert_eq!(
            session.percent, 0.0,
            "the window started over at its reset time"
        );
        assert_eq!(
            (session.resets_at, session.severity.as_str()),
            (None, "normal")
        );
        assert!(
            !shown
                .extra
                .as_deref()
                .unwrap_or("")
                .contains("Limit reached")
        );
        assert_eq!(
            shown
                .limits
                .iter()
                .find(|l| l.label == "Weekly")
                .unwrap()
                .percent,
            40.0
        );
    }

    #[test]
    fn a_reset_window_keeps_what_follows_limit_reached_and_drops_the_pace() {
        let now = 1_000_000;
        let mut limit = QuotaLimit::new("5-hour", 100.0, Some(now), Some(18000), "critical");
        limit.reset_description = Some("in 0m".into());
        limit.delta_percent = Some(3.0);
        limit.expected_percent = Some(97.0);
        limit.pace_on_track = Some(false);
        let p = QuotaProvider::new(
            "x",
            "X",
            "X",
            vec![limit.clone()],
            Some("Limit reached · Credits: 5".into()),
            None,
            None,
        );
        let r = rolled(p.clone(), now);
        assert_eq!(
            r.limits[0],
            QuotaLimit::new("5-hour", 0.0, None, Some(18000), "normal")
        );
        assert_eq!(r.extra.as_deref(), Some("Credits: 5"));
        let bare = rolled(
            QuotaProvider {
                extra: Some("Limit reached".into()),
                ..p.clone()
            },
            now,
        );
        assert_eq!(bare.extra, None);
        // still at 100 elsewhere: the text stays; nothing reset: the provider is untouched
        let still = QuotaProvider {
            limits: vec![
                limit.clone(),
                QuotaLimit::new("Weekly", 100.0, None, None, "critical"),
            ],
            ..p.clone()
        };
        assert_eq!(
            rolled(still, now).extra.as_deref(),
            Some("Limit reached · Credits: 5")
        );
        assert_eq!(rolled(p.clone(), now - 1), p);
    }
}
