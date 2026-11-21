//! Claude's quota: the sign-in (read only, never refreshed), the back-off, the reply, and the two files the agents on
//! this computer share (`claude-quota.json`, the last reading; `claude-quota-schedule.json`, when to ask next).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::{CATEGORY, QuotaLimit, QuotaMonitor, QuotaProvider};
use crate::app_paths::{read_all_text, write_all_text_private};
use crate::config::write::to_dotnet_json_value;
use crate::json_text::{Node, parse};

/// A cached reading older than this is not shown any more.
pub const CACHE_MAX_AGE_MS: i64 = 7 * 24 * 3600 * 1000;

/// The expiry of a Claude Code sign-in with an access token (0 when it has none), or None without one.
pub fn sign_in_expiry(json: Option<&str>) -> Option<i64> {
    let root = parse(json.filter(|j| !j.is_empty())?, false).ok()?;
    let oauth = root.get("claudeAiOauth").filter(|o| o.is_object())?;
    if oauth.text("accessToken").is_none_or(str::is_empty) {
        return None;
    }
    Some(oauth.long("expiresAt").unwrap_or(0))
}

/// Of the credentials file and the Keychain item, the sign-in whose token expires later. Claude Code on macOS keeps and
/// renews its sign-in in the Keychain, so a file another tool left behind can hold a token that expired hours ago.
/// Without a usable Keychain sign-in the file is returned unchanged.
pub fn pick_credentials<'a>(file: Option<&'a str>, keychain: Option<&'a str>) -> Option<&'a str> {
    let file_expiry = sign_in_expiry(file);
    let Some(keychain_expiry) = sign_in_expiry(keychain) else {
        return file;
    };
    let Some(file_expiry) = file_expiry else {
        return keychain;
    };
    // 0 is "no expiry given": it is never later than a known one, and a known one never beats it
    if file_expiry == 0 {
        return file;
    }
    if keychain_expiry == 0 || keychain_expiry > file_expiry {
        keychain
    } else {
        file
    }
}

/// When to check next: 3 minutes after a success; after a failure 1, 2, 4 … minutes (at most 15), and never sooner
/// than a Retry-After (at most an hour, plus 5 s).
pub fn claude_delay(ok: bool, failures: i32, retry_after: Option<Duration>) -> Duration {
    if ok {
        return Duration::from_secs(3 * 60);
    }
    let backoff = Duration::from_secs(60 * 15u64.min(1 << (failures.clamp(1, 10) - 1)));
    let Some(wait) = retry_after else {
        return backoff;
    };
    // a little past the server's time
    let wait = Duration::from_secs_f64(wait.as_secs_f64().clamp(0.0, 3600.0) + 5.0);
    wait.max(backoff)
}

/// `Retry-After` as seconds, or as an HTTP date less `now_ms` (never below zero); None when absent or unreadable.
pub fn retry_after(header: Option<&str>, now_ms: i64) -> Option<Duration> {
    let value = header?.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // delta-seconds, an int as .NET reads it
        return value
            .parse::<i32>()
            .ok()
            .map(|s| Duration::from_secs(s.unsigned_abs().into()));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let left = date.timestamp_millis() - now_ms;
    Some(Duration::from_millis(left.max(0).unsigned_abs()))
}

pub fn plan_label(sub: Option<&str>, tier: Option<&str>) -> String {
    static MAX_TIER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"max_(\d+x)").expect("pattern"));
    let s = match sub {
        Some("team") => "Team",
        Some("max") => "Max",
        Some("pro") => "Pro",
        Some("enterprise") => "Enterprise",
        None => "",
        Some(other) => other,
    };
    if let Some(m) = tier.and_then(|t| MAX_TIER.captures(t)) {
        return format!("{s} · Max {}", &m[1])
            .trim_matches([' ', '·'])
            .to_string();
    }
    if s.is_empty() {
        "Claude".into()
    } else {
        s.into()
    }
}

pub fn parse_claude_limits(root: &Node) -> Vec<QuotaLimit> {
    let mut list = Vec::new();
    if let Some(Node::Array(limits)) = root.get("limits") {
        // an entry that is not an object made C# throw ("Could not fetch quota: …"); here it is left out
        for l in limits.iter().filter(|l| l.is_object()) {
            let kind = l.text("kind");
            let model = l
                .get("scope")
                .filter(|s| s.is_object())
                .and_then(|s| s.get("model"))
                .filter(|m| m.is_object())
                .and_then(|m| m.text("display_name"));
            let (label, window) = match kind {
                Some("session") => ("5-hour session".to_string(), 5 * 3600),
                Some("weekly_all") => ("Weekly".to_string(), 7 * 86400),
                Some("weekly_scoped") => {
                    (format!("Weekly · {}", model.unwrap_or("model")), 7 * 86400)
                }
                _ => (kind.unwrap_or("Limit").to_string(), 0),
            };
            list.push(QuotaLimit::new(
                &label,
                l.num("percent").unwrap_or(0.0),
                iso(l.text("resets_at")),
                (window > 0).then_some(window),
                l.text("severity").unwrap_or("normal"),
            ));
        }
    }
    if list.is_empty() {
        // the older format: five_hour / seven_day
        for (key, label, window) in [
            ("five_hour", "5-hour session", 18000),
            ("seven_day", "Weekly", 604800),
        ] {
            if let Some(o) = root.get(key).filter(|o| o.is_object()) {
                list.push(QuotaLimit::new(
                    label,
                    o.num("utilization").unwrap_or(0.0),
                    iso(o.text("resets_at")),
                    Some(window),
                    "normal",
                ));
            }
        }
    }
    list
}

/// `Iso`: `DateTimeOffset.TryParse` with no style, so a time without an offset is local time.
fn iso(text: Option<&str>) -> Option<i64> {
    crate::dotnet_text::parse_date_ms(text?, false)
}

pub fn claude_extra(root: &Node) -> Option<String> {
    let extra = root.get("extra_usage").filter(|x| x.is_object())?;
    if extra.get("is_enabled") != Some(&Node::Bool(true)) {
        return None;
    }
    // the custom format "0": rounded half away from zero, as f64::round does
    Some(format!(
        "Extra usage on · {}%",
        extra.num("utilization").unwrap_or(0.0).round()
    ))
}

/// SHA-256 of the token's UTF-8, upper-case hex, first 16 characters: what is kept of a refused token.
pub fn fingerprint(token: &str) -> String {
    let hash = Sha256::digest(token.as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02X}")).collect()
}

/// The shared schedule: when the next request may go out, the message to show until then, and the fingerprint (never
/// the token) of a token Anthropic refused, with when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSchedule {
    #[serde(default)]
    pub next_at: i64,
    #[serde(default)]
    pub failures: i32,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub refused: Option<String>,
    #[serde(default)]
    pub refused_at: i64,
}

impl QuotaMonitor {
    pub fn claude_schedule_path(&self) -> PathBuf {
        let stem = self
            .claude_cache_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.claude_cache_path
            .with_file_name(format!("{stem}-schedule.json"))
    }

    /// A restart (an update) shows the last reading with its age instead of nothing.
    pub fn load_claude_cache(&self, now: i64) {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claude
            .is_some()
        {
            return;
        }
        // limits whose reset has passed show as reset (see snapshot)
        let cached = self.read_claude_cache(now);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.claude.is_none() {
            state.claude = cached;
        }
    }

    /// The cached reading, without an error, or None when there is none, it's broken or it's older than a week.
    pub(crate) fn read_claude_cache(&self, now: i64) -> Option<QuotaProvider> {
        let cached: QuotaProvider = read_shared(&self.claude_cache_path, "claude quota cache")?;
        let at = cached.updated_at?;
        if cached.id != "claude" || cached.limits.is_empty() || now - at > CACHE_MAX_AGE_MS {
            return None;
        }
        Some(QuotaProvider {
            error: None,
            ..cached
        })
    }

    pub fn save_claude_cache(&self, reading: &QuotaProvider) {
        write_shared(&self.claude_cache_path, &shared_text(reading));
    }

    #[allow(dead_code)] // until the Claude schedule uses it
    pub(crate) fn read_claude_schedule(&self) -> Option<ClaudeSchedule> {
        read_shared(&self.claude_schedule_path(), "claude quota schedule")
    }

    #[allow(dead_code)] // until the Claude schedule uses it
    pub(crate) fn save_claude_schedule(&self, schedule: &ClaudeSchedule) {
        write_shared(&self.claude_schedule_path(), &shared_text(schedule));
    }
}

/// `ConfigStore.WriteOpts` text: indented, relaxed escaping, nulls left out.
fn shared_text<T: Serialize>(value: &T) -> String {
    fn without_nulls(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(members) => serde_json::Value::Object(
                members
                    .into_iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k, without_nulls(v)))
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(without_nulls).collect())
            }
            other => other,
        }
    }
    to_dotnet_json_value(&without_nulls(
        serde_json::to_value(value).expect("quota JSON"),
    ))
}

/// A shared file read with `WriteOpts` (case-sensitive keys, strict JSON); None when it is missing or unreadable.
fn read_shared<T: serde::de::DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    if !path.is_file() {
        return None;
    }
    let read = read_all_text(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()));
    match read {
        Ok(value) => Some(value),
        Err(e) => {
            crate::log::debug(CATEGORY, &format!("{what}: {e}"));
            None
        }
    }
}

/// Atomic replace through a temporary file of this writer's own (`{path}.{guid}.tmp`, owner-only), so two agents can't
/// trip over each other.
fn write_shared(path: &Path, text: &str) {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("the system random source");
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.tmp",
        id.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        write_all_text_private(&tmp, text)?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        crate::log::debug(CATEGORY, &format!("claude quota files: {e}"));
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NL: &str = if cfg!(windows) { "\r\n" } else { "\n" };
    const MIN: u64 = 60;

    #[test]
    fn healthy_checks_run_every_three_minutes() {
        assert_eq!(claude_delay(true, 0, None), Duration::from_secs(3 * MIN));
    }

    #[test]
    fn failures_back_off_up_to_fifteen_minutes() {
        for (failures, minutes) in [(1, 1), (2, 2), (3, 4), (4, 8), (5, 15), (40, 15)] {
            assert_eq!(
                claude_delay(false, failures, None),
                Duration::from_secs(minutes * MIN),
                "{failures}"
            );
        }
    }

    #[test]
    fn a_retry_after_longer_than_the_back_off_wins() {
        // the 429 seen on 2026-11-15 asked for 230 s, while the old code asked again after 60 s
        assert_eq!(
            claude_delay(false, 1, Some(Duration::from_secs(230))),
            Duration::from_secs(235)
        );
    }

    #[test]
    fn the_back_off_wins_over_a_short_retry_after_and_a_huge_one_is_capped() {
        assert_eq!(
            claude_delay(false, 3, Some(Duration::from_secs(10))),
            Duration::from_secs(4 * MIN)
        );
        assert_eq!(
            claude_delay(false, 1, Some(Duration::from_secs(2 * 86400))),
            Duration::from_secs(3605)
        );
    }

    #[test]
    fn retry_after_is_read_as_seconds_or_as_a_date() {
        let now = crate::server::now_ms();
        let http_date = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
        };
        assert_eq!(
            retry_after(Some("230"), now),
            Some(Duration::from_secs(230))
        );
        let left = retry_after(Some(&http_date(now + 120_000)), now)
            .unwrap()
            .as_secs_f64();
        assert!((100.0..=121.0).contains(&left), "{left}");
        assert_eq!(
            retry_after(Some(&http_date(now - 120_000)), now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after(None, now), None);
        for bad in ["", "soon", "-5", "1.5", "99999999999"] {
            assert_eq!(retry_after(Some(bad), now), None, "{bad}");
        }
    }

    fn monitor(dir: &Path) -> QuotaMonitor {
        QuotaMonitor::new(None, dir, dir)
    }

    fn reading(updated_at: i64, resets_at: Option<i64>) -> QuotaProvider {
        QuotaProvider::new(
            "claude",
            "Claude",
            "Max",
            vec![QuotaLimit::new(
                "5-hour",
                42.0,
                resets_at,
                Some(18_000),
                "normal",
            )],
            None,
            Some(updated_at),
            None,
        )
    }

    #[test]
    fn a_restart_shows_the_last_reading_with_its_age() {
        let now = crate::server::now_ms();
        let dir = tempfile::tempdir().unwrap();
        monitor(dir.path()).save_claude_cache(&reading(now - 600_000, Some(now + 3_600_000)));
        let restarted = monitor(dir.path());
        restarted.load_claude_cache(now);
        let claude = restarted
            .snapshot(now)
            .into_iter()
            .find(|p| p.id == "claude")
            .unwrap();
        assert_eq!(claude.limits.len(), 1);
        assert_eq!(claude.limits[0].percent, 42.0);
        assert_eq!(claude.updated_at, Some(now - 600_000));
        assert_eq!(claude.error, None);
    }

    #[test]
    fn a_reading_older_than_a_week_or_a_broken_file_is_ignored() {
        let now = crate::server::now_ms();
        let dir = tempfile::tempdir().unwrap();
        let m = monitor(dir.path());
        m.save_claude_cache(&reading(now - 8 * 24 * 3600 * 1000, None));
        m.load_claude_cache(now);
        assert!(!m.snapshot(now).iter().any(|p| p.id == "claude"));

        std::fs::write(&m.claude_cache_path, "{ not json").unwrap();
        m.load_claude_cache(now);
        assert!(!m.snapshot(now).iter().any(|p| p.id == "claude"));
    }

    #[test]
    fn a_window_that_reset_since_the_reading_shows_as_reset() {
        let now = crate::server::now_ms();
        let dir = tempfile::tempdir().unwrap();
        let m = monitor(dir.path());
        m.save_claude_cache(&reading(now - 7_200_000, Some(now - 60_000)));
        m.load_claude_cache(now);
        let claude = m
            .snapshot(now)
            .into_iter()
            .find(|p| p.id == "claude")
            .unwrap();
        assert_eq!(claude.limits[0].percent, 0.0);
    }

    #[test]
    fn the_cache_file_round_trips_with_the_csharp_format() {
        // JsonSerializer.Serialize(reading, ConfigStore.WriteOpts): camelCase, indented, relaxed escaping, no nulls
        let csharp = [
            "{",
            "  \"id\": \"claude\",",
            "  \"name\": \"Claude\",",
            "  \"plan\": \"Max · Max 20x\",",
            "  \"limits\": [",
            "    {",
            "      \"label\": \"5-hour session\",",
            "      \"percent\": 42.5,",
            "      \"resetsAt\": 1795000000000,",
            "      \"windowSec\": 18000,",
            "      \"severity\": \"normal\"",
            "    },",
            "    {",
            "      \"label\": \"Weekly · Opus\",",
            "      \"percent\": 7,",
            "      \"windowSec\": 604800,",
            "      \"severity\": \"warning\"",
            "    }",
            "  ],",
            "  \"extra\": \"Extra usage on · 12%\",",
            "  \"updatedAt\": 1794990000000,",
            "  \"source\": \"local\"",
            "}",
        ]
        .join(NL);
        let dir = tempfile::tempdir().unwrap();
        let m = monitor(dir.path());
        std::fs::write(&m.claude_cache_path, &csharp).unwrap();
        let read = m
            .read_claude_cache(1_794_990_001_000)
            .expect("a C#-written cache reads");
        assert_eq!(read.plan, "Max · Max 20x");
        assert_eq!(
            read.limits[1],
            QuotaLimit::new("Weekly · Opus", 7.0, None, Some(604800), "warning")
        );
        m.save_claude_cache(&read);
        assert_eq!(
            std::fs::read_to_string(&m.claude_cache_path).unwrap(),
            csharp
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&m.claude_cache_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        // the keys are case-sensitive (WriteOpts reads that way too), and a reading of another provider is not ours
        std::fs::write(
            &m.claude_cache_path,
            csharp.replace("\"limits\"", "\"Limits\""),
        )
        .unwrap();
        assert_eq!(m.read_claude_cache(1_794_990_001_000), None);
        std::fs::write(
            &m.claude_cache_path,
            csharp.replace("\"claude\"", "\"codex\""),
        )
        .unwrap();
        assert_eq!(m.read_claude_cache(1_794_990_001_000), None);

        let schedule = [
            "{",
            "  \"nextAt\": 1795000000000,",
            "  \"failures\": 2,",
            "  \"refusedAt\": 0",
            "}",
        ]
        .join(NL);
        assert_eq!(
            m.claude_schedule_path(),
            dir.path().join("claude-quota-schedule.json")
        );
        std::fs::write(m.claude_schedule_path(), &schedule).unwrap();
        let read = m.read_claude_schedule().unwrap();
        assert_eq!(
            read,
            ClaudeSchedule {
                next_at: 1_795_000_000_000,
                failures: 2,
                error: None,
                refused: None,
                refused_at: 0
            }
        );
        m.save_claude_schedule(&read);
        assert_eq!(
            std::fs::read_to_string(m.claude_schedule_path()).unwrap(),
            schedule
        );
        let full = ClaudeSchedule {
            error: Some("Wait \"here\"".into()),
            refused: Some("ABCDEF0123456789".into()),
            refused_at: 5,
            ..read
        };
        m.save_claude_schedule(&full);
        assert_eq!(m.read_claude_schedule(), Some(full));
        std::fs::write(m.claude_schedule_path(), "{\"nextAt\": \"soon\"}").unwrap();
        assert_eq!(m.read_claude_schedule(), None);
    }

    #[test]
    fn the_sign_in_and_the_newer_credentials_are_read_without_the_token() {
        let creds = |token: &str, expires: &str| {
            format!(
                "{{\"claudeAiOauth\":{{\"accessToken\":\"{token}\"{expires},\"subscriptionType\":\"max\"}}}}"
            )
        };
        assert_eq!(sign_in_expiry(None), None);
        assert_eq!(sign_in_expiry(Some("")), None);
        assert_eq!(sign_in_expiry(Some("{ not json")), None);
        assert_eq!(sign_in_expiry(Some("{}")), None);
        assert_eq!(sign_in_expiry(Some(&creds("", ""))), None);
        assert_eq!(sign_in_expiry(Some("{\"claudeAiOauth\":[]}")), None);
        assert_eq!(sign_in_expiry(Some(&creds("t", ""))), Some(0));
        assert_eq!(
            sign_in_expiry(Some(&creds("t", ",\"expiresAt\":1.5"))),
            Some(0)
        );
        assert_eq!(
            sign_in_expiry(Some(&creds("t", ",\"expiresAt\":\"5\""))),
            Some(0)
        );
        assert_eq!(
            sign_in_expiry(Some(&creds("t", ",\"expiresAt\":1795000000000"))),
            Some(1_795_000_000_000)
        );

        let early = creds("a", ",\"expiresAt\":100");
        let late = creds("b", ",\"expiresAt\":200");
        let never = creds("c", "");
        assert_eq!(
            pick_credentials(Some(&early), Some("{}")),
            Some(early.as_str()),
            "no Keychain sign-in: the file"
        );
        assert_eq!(pick_credentials(None, None), None);
        assert_eq!(
            pick_credentials(Some("{"), Some(&early)),
            Some(early.as_str()),
            "no file sign-in: the Keychain"
        );
        assert_eq!(
            pick_credentials(Some(&never), Some(&late)),
            Some(never.as_str()),
            "the file never expires"
        );
        assert_eq!(
            pick_credentials(Some(&early), Some(&never)),
            Some(never.as_str())
        );
        assert_eq!(
            pick_credentials(Some(&early), Some(&late)),
            Some(late.as_str())
        );
        assert_eq!(
            pick_credentials(Some(&late), Some(&early)),
            Some(late.as_str())
        );
        assert_eq!(
            pick_credentials(Some(&late), Some(&late)),
            Some(late.as_str()),
            "a tie keeps the file"
        );

        assert_eq!(fingerprint("fake-token-ü"), "40EBD57148614EB8");
    }

    #[test]
    fn plans_limits_and_extra_usage_read_like_the_csharp_agent() {
        for (sub, tier, label) in [
            (Some("max"), Some("default_claude_max_20x"), "Max · Max 20x"),
            (None, Some("max_5x"), "Max 5x"),
            (None, None, "Claude"),
            (Some(""), None, "Claude"),
            (Some("pro"), Some("default"), "Pro"),
            (Some("team"), None, "Team"),
            (Some("enterprise"), None, "Enterprise"),
            (Some("weird"), None, "weird"),
        ] {
            assert_eq!(plan_label(sub, tier), label, "{sub:?} {tier:?}");
        }

        let reply = parse(
            r#"{"limits":[
                {"kind":"session","percent":12.5,"resets_at":"2026-11-21T10:00:00Z","severity":"warning"},
                {"kind":"weekly_all","percent":"3"},
                {"kind":"weekly_scoped","scope":{"model":{"display_name":"Opus"}},"percent":3},
                {"kind":"weekly_scoped","scope":{"model":"Opus"}},
                {"kind":"other","percent":1e400},
                {},
                5, null, "session"
              ],
              "five_hour":{"utilization":99},
              "extra_usage":{"is_enabled":true,"utilization":12.5}}"#,
            false,
        )
        .unwrap();
        let at = crate::dotnet_text::parse_date_ms("2026-11-21T10:00:00Z", false);
        assert_eq!(
            parse_claude_limits(&reply),
            vec![
                QuotaLimit::new("5-hour session", 12.5, at, Some(18_000), "warning"),
                QuotaLimit::new("Weekly", 0.0, None, Some(604_800), "normal"),
                QuotaLimit::new("Weekly · Opus", 3.0, None, Some(604_800), "normal"),
                QuotaLimit::new("Weekly · model", 0.0, None, Some(604_800), "normal"),
                QuotaLimit::new("other", 0.0, None, None, "normal"),
                QuotaLimit::new("Limit", 0.0, None, None, "normal"),
            ]
        );
        assert_eq!(
            claude_extra(&reply).as_deref(),
            Some("Extra usage on · 13%"),
            "rounded half away from zero"
        );

        let legacy = parse(
            r#"{"limits":[],"five_hour":{"utilization":50,"resets_at":"2026-11-21T10:00:00Z"},"seven_day":{"utilization":10}}"#,
            false,
        )
        .unwrap();
        assert_eq!(
            parse_claude_limits(&legacy),
            vec![
                QuotaLimit::new("5-hour session", 50.0, at, Some(18_000), "normal"),
                QuotaLimit::new("Weekly", 10.0, None, Some(604_800), "normal"),
            ]
        );
        assert_eq!(claude_extra(&legacy), None);
        for off in [
            r#"{"extra_usage":{"is_enabled":false}}"#,
            r#"{"extra_usage":{"is_enabled":"true"}}"#,
        ] {
            assert_eq!(claude_extra(&parse(off, false).unwrap()), None);
        }
        assert_eq!(
            claude_extra(&parse(r#"{"extra_usage":{"is_enabled":true}}"#, false).unwrap())
                .as_deref(),
            Some("Extra usage on · 0%")
        );
    }
}
