//! TeamClaude, the multi-account Claude proxy (a Rust-only source): the fleet's last day of traffic per user
//! (`GET /teamclaude/usage/series`) and its weekly forecast (`GET /teamclaude/forecast`), summarized for the AI quotas
//! widget. The key goes in `x-api-key` (the proxy refuses it as a bearer); it never reaches the phone or the log.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::QuotaMonitor;
use super::connect::{HttpFail, UsageReply, UsageRequest};
use crate::config::TeamClaudeConfig;

/// How often the panel's numbers are refreshed while a widget shows them; sooner again after a failure.
pub const POLL: Duration = Duration::from_secs(60);
pub const RETRY: Duration = Duration::from_secs(30);
/// The most points of each forecast line sent to the phone (the server keeps a sample a minute).
const MAX_POINTS: usize = 96;
/// Users listed by name; the rest are added up.
const TOP: usize = 5;

/// Where the numbers come from: the server's base URL (scheme, host and any path prefix, no trailing slash), the key
/// and the host to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub base: String,
    pub key: String,
    pub host: String,
}

/// `scheme://authority[/path]` without a trailing slash and with the host in lower case, for http and https only.
fn base_url(raw: &str) -> Option<(String, String)> {
    let uri: http::Uri = raw.trim().parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = uri.authority()?;
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = authority
        .port()
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let path = uri.path().trim_end_matches('/');
    Some((format!("{scheme}://{host}{port}{path}"), host))
}

fn is_anthropic(host: &str) -> bool {
    host == "anthropic.com" || host.ends_with(".anthropic.com")
}

fn non_blank(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The server and key: the settings' own, else Claude Code's (`env.ANTHROPIC_BASE_URL` with `ANTHROPIC_API_KEY` or
/// `ANTHROPIC_AUTH_TOKEN` in `~/.claude/settings.json`) when it goes through a proxy rather than Anthropic. Claude
/// Code's key is used only for Claude Code's own server; None when there is no server or no key for it.
pub fn resolve(cfg: Option<&TeamClaudeConfig>, claude_settings: Option<&Value>) -> Option<Target> {
    let env = claude_settings.and_then(|s| s.get("env"));
    let claude_code = non_blank(env.and_then(|e| e.get("ANTHROPIC_BASE_URL")))
        .and_then(base_url)
        .filter(|(_, host)| !is_anthropic(host));
    let claude_code_key = non_blank(env.and_then(|e| e.get("ANTHROPIC_API_KEY")))
        .or_else(|| non_blank(env.and_then(|e| e.get("ANTHROPIC_AUTH_TOKEN"))));
    let own_url = cfg.map(|c| c.url.trim()).filter(|u| !u.is_empty());
    let (base, host) = match own_url {
        Some(url) => base_url(url)?,
        None => claude_code.clone()?,
    };
    let own_key = cfg.map(|c| c.key.trim()).filter(|k| !k.is_empty());
    let key = match own_key {
        Some(k) => k,
        None if claude_code.as_ref().is_some_and(|(b, _)| *b == base) => claude_code_key?,
        None => return None,
    };
    Some(Target {
        base,
        key: key.to_string(),
        host,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokenTotals {
    /// input + output (the input counts the cache reads and writes, as TeamClaude books it)
    pub total: u64,
    pub input: u64,
    pub output: u64,
    pub cache: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TeamUser {
    pub name: String,
    pub tokens: u64,
    /// of the day's tokens, 0–1
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TeamOthers {
    pub count: usize,
    pub tokens: u64,
}

/// The fleet's weekly forecast: used fractions 0–1 over time (`[ms, used]`), the dry line and the verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamForecast {
    /// "on-pace", "overusing" or "underusing"; None when there is nothing to forecast (see `reason`)
    pub verdict: Option<String>,
    pub reason: Option<String>,
    pub confidence: Option<String>,
    /// a week's capacity per week
    pub pace: f64,
    pub threshold: Option<f64>,
    pub pool_dry_at: Option<i64>,
    /// capacity left at the resets, % of one week
    pub unused_pct: f64,
    pub generated_at: Option<i64>,
    pub past: Vec<(i64, f64)>,
    pub projected: Vec<(i64, f64)>,
}

/// What the panel's TeamClaude card draws (`/api/quotas` → `teamclaude`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamView {
    pub name: String,
    pub host: String,
    /// when the last reading arrived
    pub updated_at: Option<i64>,
    pub error: Option<String>,
    /// the last 24 hours; None until the usage series has arrived
    pub tokens: Option<TokenTotals>,
    /// tokens per hour, oldest first
    pub hourly: Vec<u64>,
    pub bucket_ms: Option<i64>,
    /// the end of the newest hour
    pub end: Option<i64>,
    pub users: Vec<TeamUser>,
    pub others: Option<TeamOthers>,
    pub forecast: Option<TeamForecast>,
}

impl TeamView {
    pub fn named() -> Self {
        TeamView {
            name: "TeamClaude".into(),
            ..Default::default()
        }
    }
}

fn numbers(v: Option<&Value>) -> Vec<u64> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(|n| n.as_u64().unwrap_or(0)).collect())
        .unwrap_or_default()
}

/// Evenly spaced points, both ends kept.
fn thin(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    if points.len() <= MAX_POINTS {
        return points.to_vec();
    }
    let last = points.len() - 1;
    (0..MAX_POINTS)
        .map(|i| points[(i * last + (MAX_POINTS - 1) / 2) / (MAX_POINTS - 1)])
        .collect()
}

fn line(v: Option<&Value>) -> Vec<(i64, f64)> {
    let points: Vec<(i64, f64)> = v
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_i64()?, p.get(1)?.as_f64()?)))
                .collect()
        })
        .unwrap_or_default();
    thin(&points)
}

fn read_forecast(f: &Value) -> TeamForecast {
    let text = |k: &str| f.get(k).and_then(Value::as_str).map(String::from);
    TeamForecast {
        verdict: text("verdict"),
        reason: text("reason"),
        confidence: text("confidence"),
        pace: f.get("pace").and_then(Value::as_f64).unwrap_or(0.0),
        threshold: f.get("threshold").and_then(Value::as_f64),
        pool_dry_at: f.get("poolDryAt").and_then(Value::as_i64),
        unused_pct: f.get("unusedPct").and_then(Value::as_f64).unwrap_or(0.0),
        generated_at: f.get("generatedAt").and_then(Value::as_i64),
        past: line(f.pointer("/series/past")),
        projected: line(f.pointer("/series/projected")),
    }
}

/// The card's numbers from the two replies (either may be missing), read at `now`.
pub fn summarize(series: Option<&Value>, forecast: Option<&Value>, now: i64) -> TeamView {
    let mut view = TeamView {
        updated_at: Some(now),
        forecast: forecast.map(read_forecast),
        ..TeamView::named()
    };
    let Some(series) = series else {
        return view;
    };
    let mut totals = TokenTotals::default();
    let mut hourly =
        vec![0u64; series.get("buckets").and_then(Value::as_u64).unwrap_or(0) as usize];
    let mut users: Vec<(String, u64)> = Vec::new();
    if let Some(clients) = series.get("clients").and_then(Value::as_object) {
        for (name, c) in clients {
            let (input, output, cache) = (
                numbers(c.get("inputTokens")),
                numbers(c.get("outputTokens")),
                numbers(c.get("cacheTokens")),
            );
            let len = input.len().max(output.len());
            if hourly.len() < len {
                hourly.resize(len, 0);
            }
            for (i, slot) in hourly.iter_mut().enumerate().take(len) {
                *slot += input.get(i).copied().unwrap_or(0) + output.get(i).copied().unwrap_or(0);
            }
            let (i, o) = (input.iter().sum::<u64>(), output.iter().sum::<u64>());
            totals.input += i;
            totals.output += o;
            totals.cache += cache.iter().sum::<u64>();
            if i + o > 0 {
                users.push((name.clone(), i + o));
            }
        }
    }
    totals.total = totals.input + totals.output;
    users.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let share = |t: u64| {
        if totals.total == 0 {
            0.0
        } else {
            t as f64 / totals.total as f64
        }
    };
    let rest = users.split_off(users.len().min(TOP));
    view.users = users
        .into_iter()
        .map(|(name, tokens)| TeamUser {
            share: share(tokens),
            name,
            tokens,
        })
        .collect();
    view.others = (!rest.is_empty()).then(|| TeamOthers {
        count: rest.len(),
        tokens: rest.iter().map(|(_, t)| t).sum(),
    });
    view.tokens = Some(totals);
    view.hourly = hourly;
    view.bucket_ms = series.get("bucketMs").and_then(Value::as_i64);
    view.end = series.get("end").and_then(Value::as_i64);
    view
}

/// A reply's JSON, or why there is none. A missing route (404) is `Ok(None)`: an older TeamClaude without it.
fn read_reply(reply: Result<UsageReply, HttpFail>, host: &str) -> Result<Option<Value>, String> {
    match reply {
        Ok(r) if r.status == 200 => serde_json::from_str(&r.body)
            .map(Some)
            .map_err(|_| format!("{host} sent something other than TeamClaude's figures")),
        Ok(r) if r.status == 404 => Ok(None),
        Ok(r) if r.status == 401 || r.status == 403 => {
            Err(format!("TeamClaude refused the key (HTTP {})", r.status))
        }
        Ok(r) => Err(format!("TeamClaude answered HTTP {}", r.status)),
        Err(HttpFail::Connect(_)) => Err(format!("Could not reach {host}")),
        Err(HttpFail::Other(e)) => Err(format!("TeamClaude: {e}")),
    }
}

impl QuotaMonitor {
    /// The server and key while an enabled page's quotas widget lists TeamClaude; None otherwise (no requests then).
    pub fn teamclaude_target(&self) -> Option<Target> {
        let cfg = self.cfg.as_ref()?.get();
        if !cfg.shows_teamclaude() {
            return None;
        }
        let settings = std::fs::read_to_string(&self.claude_settings_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        resolve(cfg.team_claude.as_ref(), settings.as_ref())
    }

    /// One reading of both routes. A failed route keeps what it gave last time, with the reason in `error`; true when
    /// neither failed.
    pub async fn refresh_teamclaude(&self, target: &Target, now: i64) -> bool {
        let get = |path: &str| {
            (self.http)(UsageRequest {
                url: format!("{}{path}", target.base),
                headers: vec![
                    ("x-api-key".into(), target.key.clone()),
                    ("Accept".into(), "application/json".into()),
                ],
                body: None,
            })
        };
        let (series, forecast) =
            tokio::join!(get("/teamclaude/usage/series"), get("/teamclaude/forecast"));
        let series = read_reply(series, &target.host);
        let forecast = read_reply(forecast, &target.host);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let last = state.teamclaude.take().filter(|v| v.host == target.host);
        let mut view = summarize(
            series.as_ref().ok().and_then(Option::as_ref),
            forecast.as_ref().ok().and_then(Option::as_ref),
            now,
        );
        view.host = target.host.clone();
        if series.is_err()
            && let Some(last) = &last
        {
            view.tokens = last.tokens.clone();
            view.hourly = last.hourly.clone();
            view.bucket_ms = last.bucket_ms;
            view.end = last.end;
            view.users = last.users.clone();
            view.others = last.others.clone();
        }
        if forecast.is_err() {
            view.forecast = last.as_ref().and_then(|l| l.forecast.clone());
        }
        let error = series.as_ref().err().or(forecast.as_ref().err()).cloned();
        if series.is_err() && forecast.is_err() {
            view.updated_at = last.as_ref().and_then(|l| l.updated_at);
        }
        view.error = error;
        let ok = view.error.is_none();
        state.teamclaude = Some(view);
        ok
    }

    /// The last reading, if a widget shows TeamClaude.
    pub fn teamclaude(&self) -> Option<TeamView> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .teamclaude
            .clone()
    }

    /// No widget shows TeamClaude (or there is no server): the card's numbers go.
    pub fn clear_teamclaude(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .teamclaude = None;
    }
}

#[cfg(test)]
#[path = "teamclaude_tests.rs"]
mod tests;
