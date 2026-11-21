//! Port of `QuotaMonitor.cs`: quota usage of the AI subscriptions (Claude, Codex, Antigravity, and what CodexBar adds).

pub mod claude;
pub mod connect;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

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
    #[allow(dead_code)] // until the CodexBar refresh fills it
    pub external: Vec<QuotaProvider>,
}

pub struct QuotaMonitor {
    #[allow(dead_code)] // until the overview reads it
    pub(crate) cfg: Option<Arc<ConfigStore>>,
    /// `claude-quota.json` in the local data folder; `claude-quota-schedule.json` sits beside it.
    pub(crate) claude_cache_path: PathBuf,
    /// Claude Code's credentials file (read only).
    pub(crate) credentials_path: PathBuf,
    #[allow(dead_code)] // until the Codex reader uses it
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

    /// The providers to show at `now` (local readings; CodexBar's join in with it).
    pub fn snapshot(&self, now: i64) -> Vec<QuotaProvider> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        [&state.claude, &state.codex, &state.antigravity]
            .into_iter()
            .flatten()
            .map(|p| rolled(p.clone(), now))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
