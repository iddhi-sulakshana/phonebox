//! The TeamClaude source: which server and key, the summary the panel draws, and the refresh against a fake server.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::config::TeamClaudeConfig;
use crate::quota::QuotaMonitor;
use crate::quota::connect::{HttpFail, UsageHttp, UsageReply, UsageRequest};

fn cfg(url: &str, key: &str) -> TeamClaudeConfig {
    TeamClaudeConfig {
        url: url.into(),
        key: key.into(),
        ..Default::default()
    }
}

fn claude_code(base: &str, key_name: &str, key: &str) -> Value {
    json!({ "env": { "ANTHROPIC_BASE_URL": base, key_name: key }, "model": "opus" })
}

// ------------------------------------------------------------------------------------------------ resolve

#[test]
fn the_configured_server_and_key_win() {
    let t = resolve(
        Some(&cfg("https://tc.example.com", "tc-own")),
        Some(&claude_code(
            "https://other.example.com",
            "ANTHROPIC_API_KEY",
            "tc-cc",
        )),
    )
    .unwrap();
    assert_eq!(t.base, "https://tc.example.com");
    assert_eq!(t.key, "tc-own");
    assert_eq!(t.host, "tc.example.com");
}

#[test]
fn with_nothing_configured_claude_codes_server_and_key_are_used() {
    let t = resolve(
        None,
        Some(&claude_code(
            "https://tc.example.com/",
            "ANTHROPIC_API_KEY",
            "tc-cc",
        )),
    )
    .unwrap();
    assert_eq!(t.base, "https://tc.example.com");
    assert_eq!(t.key, "tc-cc");
    let bearer = resolve(
        None,
        Some(&claude_code(
            "https://tc.example.com",
            "ANTHROPIC_AUTH_TOKEN",
            "tc-tok",
        )),
    )
    .unwrap();
    assert_eq!(bearer.key, "tc-tok");
}

#[test]
fn a_blank_key_takes_claude_codes_only_for_the_same_server() {
    let cc = claude_code("https://tc.example.com", "ANTHROPIC_API_KEY", "tc-cc");
    let same = resolve(Some(&cfg("https://TC.example.com/", "")), Some(&cc)).unwrap();
    assert_eq!(same.key, "tc-cc");
    let elsewhere = resolve(Some(&cfg("https://elsewhere.example.com", "")), Some(&cc));
    assert!(
        elsewhere.unwrap_err().contains("key"),
        "Claude Code's key is never sent to another server"
    );
}

#[test]
fn anthropic_itself_is_not_a_teamclaude_server() {
    for base in ["https://api.anthropic.com", "https://anthropic.com/"] {
        assert!(
            resolve(
                None,
                Some(&claude_code(base, "ANTHROPIC_API_KEY", "sk-ant"))
            )
            .is_err(),
            "{base}"
        );
    }
    assert!(resolve(None, None).is_err());
    assert!(resolve(None, Some(&json!({ "env": {} }))).is_err());
}

#[test]
fn a_path_is_kept_for_a_proxy_under_a_prefix_and_junk_is_refused() {
    let t = resolve(Some(&cfg("https://gw.example.com/tc/", "k")), None).unwrap();
    assert_eq!(t.base, "https://gw.example.com/tc");
    assert!(resolve(Some(&cfg("not a url", "k")), None).is_err());
    assert!(resolve(Some(&cfg("ftp://tc.example.com", "k")), None).is_err());
}

// review: TeamClaude matches its routes on the exact path, so a pasted dashboard address or Claude Code's pinned form
// (`…/tc-acct/<pin>`, which TeamClaude forwards to Anthropic) must come down to the server itself
#[test]
fn a_dashboard_or_pinned_address_is_cut_to_the_server() {
    for (url, base) in [
        (
            "https://tc.example.com/teamclaude/dashboard",
            "https://tc.example.com",
        ),
        (
            "https://tc.example.com/teamrouter/dashboard",
            "https://tc.example.com",
        ),
        (
            "https://tc.example.com/tc-acct/abc123",
            "https://tc.example.com",
        ),
        ("https://tc.example.com/v1", "https://tc.example.com"),
        ("https://gw.example.com/tc/v1/", "https://gw.example.com/tc"),
        (
            "https://gw.example.com/tc/teamclaude/dashboard",
            "https://gw.example.com/tc",
        ),
        (
            "https://gw.example.com/teamclaudeish",
            "https://gw.example.com/teamclaudeish",
        ),
    ] {
        assert_eq!(
            resolve(Some(&cfg(url, "k")), None).unwrap().base,
            base,
            "{url}"
        );
    }
    let pinned = claude_code(
        "https://tc.example.com/tc-acct/pin1",
        "ANTHROPIC_API_KEY",
        "tc-cc",
    );
    let t = resolve(None, Some(&pinned)).unwrap();
    assert_eq!(
        (t.base.as_str(), t.key.as_str()),
        ("https://tc.example.com", "tc-cc")
    );
}

// review: TeamClaude lets loopback callers in without a key, and its local setup sets none
#[test]
fn a_local_teamclaude_needs_no_key() {
    for url in [
        "http://localhost:3456",
        "http://127.0.0.1:3456/",
        "http://[::1]:3456",
    ] {
        let t = resolve(Some(&cfg(url, "")), None).unwrap();
        assert_eq!(t.key, "", "{url}");
    }
    assert!(resolve(Some(&cfg("https://tc.example.com", "")), None).is_err());
}

// review: a widget that lists TeamClaude without a usable server says what to set
#[test]
fn an_unusable_setting_says_what_to_fix() {
    let none = resolve(None, None).unwrap_err();
    assert!(none.contains("Settings › TeamClaude"), "{none}");
    let scheme = resolve(Some(&cfg("tc.example.com", "k")), None).unwrap_err();
    assert!(scheme.contains("http://"), "{scheme}");
    let key = resolve(Some(&cfg("https://tc.example.com", "")), None).unwrap_err();
    assert!(key.contains("key"), "{key}");
}

// ------------------------------------------------------------------------------------------------ summarize

/// 24 hourly buckets; `user(i)` gives each user's tokens in bucket i.
fn series(users: &[(&str, u64, u64, u64)]) -> Value {
    let mut clients = serde_json::Map::new();
    for (name, input, output, cache) in users {
        let spread = |total: u64| -> Vec<u64> {
            let mut v = vec![0; 24];
            v[23] = total / 2;
            v[22] = total - total / 2;
            v
        };
        clients.insert(
            (*name).into(),
            json!({ "requests": vec![1; 24], "inputTokens": spread(*input), "outputTokens": spread(*output),
                    "cacheTokens": spread(*cache) }),
        );
    }
    json!({ "slotMs": 900_000, "bucketMs": 3_600_000, "buckets": 24, "end": 1_795_364_100_000_i64,
            "clients": clients })
}

/// TeamClaude's status: lifetime counters per client key (input counts the cache).
fn status(users: &[(&str, u64, u64)]) -> Value {
    let mut clients = serde_json::Map::new();
    for (name, input, output) in users {
        clients.insert(
            (*name).into(),
            json!({ "requests": 3, "connections": 1, "inputTokens": input, "outputTokens": output,
                    "cacheTokens": input / 2, "lastUsed": "2026-11-22T10:00:00Z", "windows": {}, "mix": [] }),
        );
    }
    json!({ "clients": clients, "accounts": [], "server": { "uptime": 1 } })
}

fn forecast(points: usize) -> Value {
    let past: Vec<Value> = (0..points)
        .map(|i| {
            json!([
                1_795_000_000_000_i64 + i as i64 * 60_000,
                0.3 + i as f64 * 1e-4
            ])
        })
        .collect();
    let projected: Vec<Value> = (0..points / 2)
        .map(|i| {
            json!([
                1_796_000_000_000_i64 + i as i64 * 900_000,
                0.4 - i as f64 * 1e-4
            ])
        })
        .collect();
    json!({ "generatedAt": 1_795_364_063_859_i64, "bucket": "unified7d", "confidence": "high", "spanHours": 24,
            "strategy": "soonest", "notForecast": [], "reason": null, "burnRate": 1.2, "pace": 0.767,
            "verdict": "on-pace", "threshold": 0.995, "fleetCapacity": 130, "poolDryAt": null, "dryForMs": null,
            "unusedPct": 0.1, "accounts": [], "users": [], "scoped": [],
            "series": { "past": past, "projected": projected } })
}

#[test]
fn the_day_is_added_up_across_users() {
    let v = summarize(
        Some(&series(&[("alice", 1000, 100, 900), ("bob", 500, 50, 400)])),
        None,
        7,
    );
    let t = v.tokens.unwrap();
    assert_eq!(
        (t.input, t.output, t.cache, t.total),
        (1500, 150, 1300, 1650)
    );
    assert_eq!(v.hourly.len(), 24);
    assert_eq!(v.hourly.iter().sum::<u64>(), 1650);
    assert_eq!(v.hourly[23], 500 + 50 + 250 + 25);
    assert_eq!(v.bucket_ms, Some(3_600_000));
    assert_eq!(v.end, Some(1_795_364_100_000));
    assert_eq!(v.updated_at, Some(7));
}

#[test]
fn the_hours_split_into_input_and_output() {
    let v = summarize(
        Some(&series(&[("alice", 1000, 100, 900), ("bob", 500, 50, 400)])),
        None,
        0,
    );
    assert_eq!(v.hourly_input.len(), 24);
    assert_eq!(v.hourly_output.len(), 24);
    assert_eq!(
        (v.hourly_input[23], v.hourly_output[23]),
        (500 + 250, 50 + 25)
    );
    assert_eq!(v.hourly_input.iter().sum::<u64>(), 1500);
    assert_eq!(v.hourly_output.iter().sum::<u64>(), 150);
}

#[test]
fn the_lifetime_top_five_come_from_the_status() {
    let s = status(&[
        ("gus", 5, 5),
        ("alice", 7000, 100),
        ("bob", 6000, 0),
        ("carol", 5000, 0),
        ("dave", 4000, 0),
        ("erin", 3000, 0),
        ("frank", 20, 0),
        ("idle", 0, 0),
    ]);
    let (users, others) = top_totals(&s);
    let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["alice", "bob", "carol", "dave", "erin"]);
    assert_eq!(users[0].tokens, 7100);
    assert!((users[0].share - 7100.0 / 25130.0).abs() < 1e-9);
    let others = others.unwrap();
    assert_eq!((others.count, others.tokens), (2, 30));
    assert_eq!(top_totals(&json!({})), (vec![], None));
}

#[test]
fn the_top_five_users_and_the_rest() {
    let users: Vec<(&str, u64, u64, u64)> = vec![
        ("gus", 10, 0, 0),
        ("alice", 700, 0, 0),
        ("bob", 600, 0, 0),
        ("carol", 500, 0, 0),
        ("dave", 400, 0, 0),
        ("erin", 300, 0, 0),
        ("frank", 20, 0, 0),
        ("idle", 0, 0, 0),
    ];
    let v = summarize(Some(&series(&users)), None, 0);
    let names: Vec<&str> = v.users.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["alice", "bob", "carol", "dave", "erin"]);
    assert_eq!(v.users[0].tokens, 700);
    assert!((v.users[0].share - 700.0 / 2530.0).abs() < 1e-9);
    let others = v.others.unwrap();
    assert_eq!(
        (others.count, others.tokens),
        (2, 30),
        "idle users are left out"
    );
}

// review: the server's bucket count sizes a buffer; a wrong one must not take the agent down
#[test]
fn a_huge_bucket_count_is_capped() {
    let mut s = series(&[("alice", 10, 1, 5)]);
    s["buckets"] = json!(1_000_000_000_000_u64);
    s["clients"]["alice"]["inputTokens"] = json!(vec![1u64; 5000]);
    let v = summarize(Some(&s), None, 0);
    assert_eq!(v.hourly.len(), 24 * 31);
    assert_eq!(v.tokens.unwrap().input, 24 * 31);
}

#[test]
fn no_users_is_an_empty_list() {
    let v = summarize(Some(&series(&[])), None, 0);
    assert!(v.users.is_empty());
    assert_eq!(v.others, None);
    assert_eq!(v.tokens.unwrap().total, 0);
}

#[test]
fn the_forecast_is_thinned_to_96_points_keeping_both_ends() {
    let v = summarize(None, Some(&forecast(1440)), 0);
    let f = v.forecast.unwrap();
    assert_eq!(f.verdict.as_deref(), Some("on-pace"));
    assert_eq!(f.confidence.as_deref(), Some("high"));
    assert!((f.pace - 0.767).abs() < 1e-9);
    assert_eq!(f.threshold, Some(0.995));
    assert_eq!(f.pool_dry_at, None);
    assert!((f.unused_pct - 0.1).abs() < 1e-9);
    assert_eq!(f.generated_at, Some(1_795_364_063_859));
    assert_eq!(f.past.len(), 96);
    assert_eq!(f.past[0].0, 1_795_000_000_000);
    assert_eq!(f.past[95].0, 1_795_000_000_000 + 1439 * 60_000);
    assert_eq!(f.projected.len(), 96);
    assert_eq!(v.tokens, None, "no series: no totals");
    let short = summarize(None, Some(&forecast(10)), 0).forecast.unwrap();
    assert_eq!((short.past.len(), short.projected.len()), (10, 5));
}

#[test]
fn a_forecast_without_capacity_keeps_its_reason() {
    let mut f = forecast(0);
    f["verdict"] = Value::Null;
    f["reason"] = json!("no-capacity");
    let v = summarize(None, Some(&f), 0).forecast.unwrap();
    assert_eq!(v.verdict, None);
    assert_eq!(v.reason.as_deref(), Some("no-capacity"));
}

// ------------------------------------------------------------------------------------------------ refresh

type Seen = Arc<Mutex<Vec<UsageRequest>>>;

/// A fake TeamClaude: `answer(path)` gives (status, body); every request is kept.
fn fake(answer: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> (UsageHttp, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let answer = Arc::new(answer);
    let http: UsageHttp = Arc::new(move |req: UsageRequest| {
        log.lock().unwrap().push(req.clone());
        let answer = answer.clone();
        Box::pin(async move {
            let path = req
                .url
                .split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(&req.url);
            let path = &path[path.find('/').unwrap_or(path.len())..];
            let (status, body) = answer(path);
            if status == 0 {
                return Err(HttpFail::Connect("refused".into()));
            }
            Ok(UsageReply {
                status,
                retry_after: None,
                body,
            })
        })
    });
    (http, seen)
}

fn monitor(http: UsageHttp, dir: &tempfile::TempDir) -> QuotaMonitor {
    QuotaMonitor {
        http,
        antigravity_https: None,
        ..QuotaMonitor::new(None, dir.path(), dir.path())
    }
}

fn target() -> Target {
    resolve(Some(&cfg("https://tc.example.com", "tc-key")), None).unwrap()
}

#[tokio::test]
async fn both_routes_are_asked_with_the_key_in_x_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let (http, seen) = fake(|path| match path {
        "/teamclaude/usage/series" => (200, series(&[("alice", 10, 1, 5)]).to_string()),
        "/teamclaude/forecast" => (200, forecast(4).to_string()),
        "/teamclaude/status" => (200, status(&[("alice", 900, 100)]).to_string()),
        _ => (404, String::new()),
    });
    let m = monitor(http, &dir);
    assert!(m.refresh_teamclaude(&target(), 1000).await);
    let mut urls: Vec<String> = seen.lock().unwrap().iter().map(|r| r.url.clone()).collect();
    urls.sort();
    assert_eq!(
        urls,
        [
            "https://tc.example.com/teamclaude/forecast",
            "https://tc.example.com/teamclaude/status",
            "https://tc.example.com/teamclaude/usage/series"
        ]
    );
    for r in seen.lock().unwrap().iter() {
        let header = |n: &str| {
            r.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(header("x-api-key"), Some("tc-key"));
        assert_eq!(header("authorization"), None);
        assert!(r.body.is_none());
    }
    let v = m.teamclaude().unwrap();
    assert_eq!(v.host, "tc.example.com");
    assert_eq!(v.error, None);
    assert_eq!(v.tokens.as_ref().unwrap().total, 11);
    assert!(v.forecast.is_some());
    assert_eq!(v.updated_at, Some(1000));
    assert!(
        !serde_json::to_string(&v).unwrap().contains("tc-key"),
        "the key is not in the view"
    );
}

#[tokio::test]
async fn a_refused_key_keeps_the_last_numbers_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = refuse.clone();
    let (http, _) = fake(move |path| {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return (401, r#"{"error":"bad key"}"#.into());
        }
        match path {
            "/teamclaude/usage/series" => (200, series(&[("alice", 10, 1, 5)]).to_string()),
            _ => (200, forecast(4).to_string()),
        }
    });
    let m = monitor(http, &dir);
    assert!(m.refresh_teamclaude(&target(), 1000).await);
    refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!m.refresh_teamclaude(&target(), 2000).await);
    let v = m.teamclaude().unwrap();
    assert_eq!(v.tokens.unwrap().total, 11, "the last good day stays");
    assert!(v.forecast.is_some());
    assert_eq!(v.updated_at, Some(1000));
    let error = v.error.unwrap();
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains("tc-key"));
}

#[tokio::test]
async fn one_route_failing_keeps_the_others_data() {
    let dir = tempfile::tempdir().unwrap();
    // an older TeamClaude without the forecast route: no error, no forecast
    let (http, _) = fake(|path| match path {
        "/teamclaude/usage/series" => (200, series(&[("alice", 10, 1, 5)]).to_string()),
        _ => (404, "Not Found".into()),
    });
    let m = monitor(http, &dir);
    assert!(m.refresh_teamclaude(&target(), 1000).await);
    let v = m.teamclaude().unwrap();
    assert_eq!(v.error, None);
    assert_eq!(v.forecast, None);
    assert_eq!(v.tokens.unwrap().total, 11);

    // the forecast down for a moment: the day's numbers and an error
    let (http, _) = fake(|path| match path {
        "/teamclaude/forecast" => (200, forecast(4).to_string()),
        _ => (0, String::new()),
    });
    let m2 = monitor(http, &dir);
    assert!(!m2.refresh_teamclaude(&target(), 1000).await);
    let v = m2.teamclaude().unwrap();
    assert!(v.forecast.is_some());
    assert_eq!(v.tokens, None);
    assert!(v.error.unwrap().contains("tc.example.com"));
}

#[tokio::test]
async fn nothing_to_show_clears_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let (http, _) = fake(|_| (200, series(&[]).to_string()));
    let m = monitor(http, &dir);
    m.refresh_teamclaude(&target(), 1).await;
    assert!(m.teamclaude().is_some());
    m.clear_teamclaude();
    assert_eq!(m.teamclaude(), None);
}

#[test]
fn the_target_needs_a_widget_that_lists_teamclaude() {
    use crate::config::store::ConfigStore;
    let dir = tempfile::tempdir().unwrap();
    // QuotaMonitor::new(cfg, local_dir, home): Claude Code's settings are home/.claude/settings.json
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    std::fs::write(
        dir.path().join(".claude").join("settings.json"),
        claude_code("https://tc.example.com", "ANTHROPIC_API_KEY", "tc-cc").to_string(),
    )
    .unwrap();
    let cfg_path = dir.path().join("config.json");
    let page = |providers: &str| {
        format!(
            r#"{{"pages":[{{"id":"ai","enabled":true,"widgets":[{{"id":"q1","type":"quotas","options":{{"providers":{providers}}}}}]}}]}}"#
        )
    };
    std::fs::write(&cfg_path, page(r#"["claude"]"#)).unwrap();
    let store = Arc::new(ConfigStore::new(cfg_path.clone()));
    let m = QuotaMonitor::new(Some(store.clone()), dir.path(), dir.path());
    assert_eq!(
        m.teamclaude_target(),
        None,
        "no widget lists TeamClaude: no requests"
    );

    std::fs::write(&cfg_path, page(r#"["teamclaude"]"#)).unwrap();
    let file = std::fs::File::options()
        .write(true)
        .open(&cfg_path)
        .unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    let t = m
        .teamclaude_target()
        .expect("the widget lists TeamClaude")
        .expect("Claude Code's server and key");
    assert_eq!(
        (t.base.as_str(), t.key.as_str()),
        ("https://tc.example.com", "tc-cc")
    );
}

#[tokio::test]
async fn an_address_without_teamclaude_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (http, _) = fake(|_| (404, "Not Found".into()));
    let m = monitor(http, &dir);
    assert!(!m.refresh_teamclaude(&target(), 1).await);
    let error = m.teamclaude().unwrap().error.unwrap();
    assert!(error.contains("No TeamClaude at tc.example.com"), "{error}");
}

#[tokio::test]
async fn a_keyless_local_server_gets_no_key_header() {
    let dir = tempfile::tempdir().unwrap();
    let (http, seen) = fake(|_| (200, series(&[]).to_string()));
    let m = monitor(http, &dir);
    let local = resolve(Some(&cfg("http://localhost:3456", "")), None).unwrap();
    assert!(m.refresh_teamclaude(&local, 1).await);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "the series, the forecast and the status");
    for r in seen.iter() {
        assert!(
            !r.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")),
            "{:?}",
            r.headers
        );
    }
}

#[tokio::test]
async fn a_widget_without_a_usable_server_shows_why_without_asking() {
    use crate::config::store::ConfigStore;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    std::fs::write(
        &cfg_path,
        r#"{"pages":[{"id":"ai","enabled":true,"widgets":[{"id":"q1","type":"quotas","options":{"providers":["teamclaude"]}}]}]}"#,
    )
    .unwrap();
    let (http, seen) = fake(|_| (200, String::new()));
    let m = QuotaMonitor {
        http,
        antigravity_https: None,
        ..QuotaMonitor::new(
            Some(Arc::new(ConfigStore::new(cfg_path))),
            dir.path(),
            dir.path(),
        )
    };
    let mut next = tokio::time::Instant::now();
    m.teamclaude_tick(&mut next).await;
    assert!(seen.lock().unwrap().is_empty(), "no server: no request");
    let v = m.teamclaude().expect("the card says why");
    assert!(v.error.unwrap().contains("Settings › TeamClaude"));
    assert_eq!(v.tokens, None);
}

// the status is 70 kB on a real team; the lifetime totals move slowly, so it is read every five minutes
#[tokio::test]
async fn the_status_is_read_every_five_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let (http, seen) = fake(|path| match path {
        "/teamclaude/usage/series" => (200, series(&[("alice", 10, 1, 5)]).to_string()),
        "/teamclaude/forecast" => (200, forecast(4).to_string()),
        "/teamclaude/status" => (
            200,
            status(&[("alice", 900, 100), ("bob", 50, 0)]).to_string(),
        ),
        _ => (404, String::new()),
    });
    let m = monitor(http, &dir);
    let asked = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.ends_with("/teamclaude/status"))
            .count()
    };
    assert!(m.refresh_teamclaude(&target(), 0).await);
    assert_eq!(asked(&seen), 1);
    assert_eq!(m.teamclaude().unwrap().total_users.len(), 2);
    assert!(m.refresh_teamclaude(&target(), 60_000).await);
    assert_eq!(asked(&seen), 1, "a minute later: not again");
    let v = m.teamclaude().unwrap();
    assert_eq!(v.total_users[0].name, "alice", "the last totals stay");
    assert_eq!(v.total_users[0].tokens, 1000);
    assert!(m.refresh_teamclaude(&target(), 300_000).await);
    assert_eq!(asked(&seen), 2, "five minutes later: again");
}
