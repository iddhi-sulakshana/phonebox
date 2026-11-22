//! The Claude quota check through its seams: a fake HTTP client and a credentials file of the test's own, so nothing
//! here reads the real token or reaches api.anthropic.com. The usage endpoint is rate limited per account and shared
//! with Claude Code, so these tests count the requests PhoneBox would send.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::quota::connect::{HttpFail, UsageHttp, UsageReply, UsageRequest};

const MINUTE: i64 = 60_000;
const USAGE: &str = r#"{"limits":[{"kind":"session","percent":42,"resets_at":"2099-01-01T00:00:00Z","severity":"normal"}]}"#;

type OnSend = Box<dyn Fn() + Send>;

enum Next {
    Reply(u16, String, Option<u64>),
    Fail(HttpFail),
}

/// The fake Anthropic: counts the requests, checks the URL, keeps the last bearer token.
struct FakeAnthropic {
    calls: AtomicUsize,
    last_token: Mutex<Option<String>>,
    next: Mutex<Next>,
    on_send: Mutex<Option<OnSend>>,
}

impl FakeAnthropic {
    fn new() -> Arc<Self> {
        Arc::new(FakeAnthropic {
            calls: AtomicUsize::new(0),
            last_token: Mutex::new(None),
            next: Mutex::new(Next::Reply(500, String::new(), None)),
            on_send: Mutex::new(None),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn respond(&self, status: u16, body: &str, retry_after_seconds: Option<u64>) {
        *self.next.lock().unwrap() = Next::Reply(status, body.into(), retry_after_seconds);
    }

    fn fail(&self, fail: HttpFail) {
        *self.next.lock().unwrap() = Next::Fail(fail);
    }

    fn on_send(&self, f: Option<OnSend>) {
        *self.on_send.lock().unwrap() = f;
    }

    fn http(self: &Arc<Self>) -> UsageHttp {
        let me = self.clone();
        Arc::new(move |req: UsageRequest| {
            me.calls.fetch_add(1, Ordering::SeqCst);
            let bearer = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .and_then(|(_, v)| v.strip_prefix("Bearer "))
                .map(str::to_string);
            *me.last_token.lock().unwrap() = bearer;
            if let Some(f) = me.on_send.lock().unwrap().as_ref() {
                f();
            }
            assert_eq!(req.url, "https://api.anthropic.com/api/oauth/usage");
            assert!(
                req.headers
                    .iter()
                    .any(|(k, v)| k == "anthropic-beta" && v == "oauth-2025-04-20")
            );
            let result = match &*me.next.lock().unwrap() {
                Next::Reply(status, body, retry) => Ok(UsageReply {
                    status: *status,
                    retry_after: retry.map(|s| s.to_string()),
                    body: body.clone(),
                }),
                Next::Fail(f) => Err(f.clone()),
            };
            Box::pin(async move { result })
        })
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    api: Arc<FakeAnthropic>,
    keychain: Arc<Mutex<Option<String>>>,
    t0: i64,
}

fn sign_in_json(token: &str, expires_at: i64) -> String {
    format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":{expires_at},"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}}}"#
    )
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
            api: FakeAnthropic::new(),
            keychain: Arc::new(Mutex::new(None)),
            t0: crate::server::now_ms(),
        }
    }

    fn creds(&self) -> PathBuf {
        self.dir.path().join("credentials.json")
    }

    fn sign_in(&self, token: &str, expires_at: i64) {
        std::fs::write(self.creds(), sign_in_json(token, expires_at)).unwrap();
    }

    fn agent_with(&self, keychain: KeychainReader) -> QuotaMonitor {
        QuotaMonitor {
            credentials_path: self.creds(),
            http: self.api.http(),
            read_keychain: Some(keychain),
            ..QuotaMonitor::new(None, self.dir.path(), self.dir.path())
        }
    }

    fn agent(&self) -> QuotaMonitor {
        let keychain = self.keychain.clone();
        self.agent_with(Arc::new(move || keychain.lock().unwrap().clone()))
    }

    fn schedule_path(&self) -> PathBuf {
        self.dir.path().join("claude-quota-schedule.json")
    }
}

fn claude(agent: &QuotaMonitor) -> QuotaProvider {
    let all: Vec<_> = agent
        .snapshot(crate::server::now_ms())
        .into_iter()
        .filter(|p| p.id == "claude")
        .collect();
    assert_eq!(all.len(), 1, "{all:?}");
    all.into_iter().next().unwrap()
}

fn single_percent(p: &QuotaProvider) -> f64 {
    assert_eq!(p.limits.len(), 1, "{p:?}");
    p.limits[0].percent
}

fn write(path: &Path, text: String) {
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn a_fresh_reading_is_not_fetched_again_when_the_agent_restarts() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(200, USAGE, None);
    f.agent().claude_tick(f.t0).await;
    assert_eq!(f.api.calls(), 1);

    let restarted = f.agent();
    restarted.load_claude_cache(f.t0 + MINUTE);
    restarted.claude_tick(f.t0 + MINUTE).await;
    assert_eq!(f.api.calls(), 1, "the reading is a minute old: no request");
    assert_eq!(single_percent(&claude(&restarted)), 42.0);

    restarted.claude_tick(f.t0 + 3 * MINUTE + 1).await;
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_running_agent_uses_the_reading_another_one_just_fetched() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(200, USAGE, None);
    let first = f.agent();
    let second = f.agent();
    first.claude_tick(f.t0).await;
    second.claude_tick(f.t0 + 10_000).await;
    assert_eq!(f.api.calls(), 1);
    assert_eq!(single_percent(&claude(&second)), 42.0);
}

#[tokio::test]
async fn a_retry_after_is_kept_across_a_restart() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(429, "{}", Some(230));
    f.agent().claude_tick(f.t0).await;

    let restarted = f.agent();
    restarted.load_claude_cache(f.t0 + MINUTE);
    restarted.claude_tick(f.t0 + MINUTE).await;
    restarted.claude_tick(f.t0 + 3 * MINUTE).await;
    assert_eq!(f.api.calls(), 1, "the server asked for 230 s");
    assert!(claude(&restarted).error.unwrap().contains("limiting"));

    f.api.respond(200, USAGE, None);
    restarted.claude_tick(f.t0 + 236_000).await;
    assert_eq!(f.api.calls(), 2);
    assert_eq!(claude(&restarted).error, None);
}

#[tokio::test]
async fn a_renewed_token_is_used_on_the_next_pass_after_an_expired_one() {
    let f = Fixture::new();
    f.sign_in("old", f.t0 - MINUTE);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    let mut t = f.t0;
    while t <= f.t0 + 4 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 0, "no request with an expired token");
    assert!(claude(&agent).error.unwrap().contains("expired"));

    f.sign_in("new", f.t0 + 3_600_000); // Claude Code renewed it
    agent.claude_tick(f.t0 + 4 * MINUTE + 30_000).await;
    agent.claude_tick(f.t0 + 5 * MINUTE).await;
    assert_eq!(f.api.calls(), 1, "used within a minute");
    assert_eq!(claude(&agent).error, None);
}

#[tokio::test]
async fn a_plan_without_a_token_says_claude_desktop_does_not_share_its_sign_in() {
    // what the Claude Desktop app leaves: the plan, no access token (#247)
    let f = Fixture::new();
    write(
        &f.creds(),
        r#"{"claudeAiOauth":{"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}"#
            .into(),
    );
    let agent = f.agent();
    agent.claude_tick(f.t0).await;
    assert_eq!(f.api.calls(), 0);
    assert_eq!(claude(&agent).error.as_deref(), Some(NO_CLI_SIGN_IN));
}

#[tokio::test]
async fn a_token_that_expires_within_a_minute_is_not_sent() {
    let f = Fixture::new();
    f.sign_in("almost", f.t0 + 20_000);
    f.agent().claude_tick(f.t0).await;
    assert_eq!(f.api.calls(), 0);
}

#[tokio::test]
async fn a_refused_token_is_not_sent_again_until_claude_code_renews_it() {
    let f = Fixture::new();
    f.sign_in("revoked", f.t0 + 7_200_000);
    f.api.respond(401, r#"{"error":"invalid token"}"#, None);
    let agent = f.agent();
    let mut t = f.t0;
    while t < f.t0 + 30 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 1);
    assert!(claude(&agent).error.is_some());

    f.sign_in("renewed", f.t0 + 7_200_000);
    f.api.respond(200, USAGE, None);
    agent.claude_tick(f.t0 + 30 * MINUTE).await;
    assert_eq!(f.api.calls(), 2);
    assert_eq!(claude(&agent).error, None);
}

#[tokio::test]
async fn a_refusal_is_not_permanent() {
    // a 401 from a proxy or an outage: the same token is tried again after half an hour
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(401, "", None);
    let agent = f.agent();
    let mut t = f.t0;
    while t <= f.t0 + 61 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 3, "at 0, 30 and 60 minutes");
}

#[tokio::test]
async fn a_forbidden_reply_backs_off_like_other_errors() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(403, "<html>challenge</html>", None);
    let agent = f.agent();
    let mut t = f.t0;
    while t <= f.t0 + 8 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 4, "at 0, 1, 3 and 7 minutes");
    assert!(claude(&agent).error.unwrap().contains("403"));
}

#[tokio::test]
async fn a_clock_set_back_does_not_stop_the_checks() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;

    // the clock was a day ahead and NTP corrected it
    let back = f.t0 - 24 * 60 * MINUTE;
    let mut t = back;
    while f.api.calls() < 2 && t < back + 3 * 60 * MINUTE {
        t += 30_000;
        agent.claude_tick(t).await;
    }
    assert_eq!(f.api.calls(), 2);
    assert!(
        (0..=3_605_000 + 60_000).contains(&(t - back)),
        "the longest wait, plus the 30 s passes on either side"
    );

    // and from then on the normal 3 minutes, with no leftover from the old schedule
    let mut u = t + 30_000;
    while u <= t + 3 * MINUTE + 30_000 {
        agent.claude_tick(u).await;
        u += 30_000;
    }
    assert_eq!(f.api.calls(), 3);
}

#[tokio::test]
async fn an_unreachable_network_is_retried_soon_without_showing_an_error() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;

    // the computer woke up and the network is not back yet
    f.api.fail(HttpFail::Connect("No route to host".into()));
    agent.claude_tick(f.t0 + 3 * MINUTE).await;
    assert_eq!(claude(&agent).error, None);
    assert_eq!(single_percent(&claude(&agent)), 42.0);

    f.api.respond(200, USAGE, None);
    agent.claude_tick(f.t0 + 3 * MINUTE + 30_000).await;
    assert_eq!(f.api.calls(), 3);
    assert_eq!(claude(&agent).error, None);
}

#[tokio::test]
async fn a_network_that_stays_down_is_reported_and_backs_off() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api
        .fail(HttpFail::Connect("No such host is known".into()));
    let agent = f.agent();
    let mut t = f.t0;
    while t <= f.t0 + 10 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert!(claude(&agent).error.is_some());
    // three quick retries 30 s apart, then the back-off (1, 2, 4 … minutes)
    assert!((5..=7).contains(&f.api.calls()), "{}", f.api.calls());
}

#[tokio::test]
async fn a_reply_without_limits_keeps_the_last_reading_and_the_cache() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;

    f.api.respond(200, r#"{"something":"new"}"#, None);
    agent.claude_tick(f.t0 + 3 * MINUTE).await;
    assert_eq!(single_percent(&claude(&agent)), 42.0);
    assert!(claude(&agent).error.is_some());

    let restarted = f.agent();
    restarted.load_claude_cache(f.t0 + 4 * MINUTE);
    assert_eq!(single_percent(&claude(&restarted)), 42.0);

    // an unknown reply format won't change by asking again soon
    let mut t = f.t0 + 3 * MINUTE + 30_000;
    while t < f.t0 + 18 * MINUTE {
        agent.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_html_error_page_with_status200_keeps_the_last_reading() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 3_600_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;

    f.api.respond(200, "<html>captive portal</html>", None);
    agent.claude_tick(f.t0 + 3 * MINUTE).await;
    assert_eq!(single_percent(&claude(&agent)), 42.0);
}

#[tokio::test]
async fn a_connect_failure_of_the_real_handler_is_a_connection_error() {
    // the quick retry relies on the real client reporting a failed connect (through the IPv4-first connect) as a
    // connection error
    let port = {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        free.local_addr().unwrap().port()
    };
    let http = crate::quota::connect::usage_http();
    let result = http(UsageRequest {
        url: format!("http://127.0.0.1:{port}/"),
        headers: vec![],
        body: None,
    })
    .await;
    assert!(matches!(result, Err(HttpFail::Connect(_))), "{result:?}");
}

#[tokio::test]
async fn an_expiry_that_is_not_a_whole_number_does_not_end_the_checks() {
    for expires_at in ["null", "\"2026-11-16T10:00:00Z\"", "1.5e300"] {
        // JsonElement.TryGetInt64 throws on a non-number; that used to end the whole usage loop
        let f = Fixture::new();
        write(
            &f.creds(),
            format!(r#"{{"claudeAiOauth":{{"accessToken":"token-1","expiresAt":{expires_at}}}}}"#),
        );
        f.api.respond(200, USAGE, None);
        let agent = f.agent();
        agent.claude_tick(f.t0).await;
        assert_eq!(
            f.api.calls(),
            1,
            "an unknown expiry is left to the server ({expires_at})"
        );
        assert_eq!(claude(&agent).error, None);
    }
}

#[tokio::test]
async fn a_refused_token_stays_refused_across_a_restart() {
    let f = Fixture::new();
    f.sign_in("revoked", f.t0 + 7_200_000);
    f.api.respond(401, "", None);
    f.agent().claude_tick(f.t0).await;

    let restarted = f.agent();
    let mut t = f.t0 + 2 * MINUTE;
    while t < f.t0 + 30 * MINUTE {
        restarted.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 1);
    assert!(claude(&restarted).error.is_some());

    f.sign_in("renewed", f.t0 + 7_200_000);
    f.api.respond(200, USAGE, None);
    restarted.claude_tick(f.t0 + 31 * MINUTE).await;
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_longer_wait_another_agent_got_meanwhile_is_kept() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    // while this request runs, another agent on the computer gets a 429 that asks for 230 s
    let (path, t0) = (f.schedule_path(), f.t0);
    f.api.on_send(Some(Box::new(move || {
        write(
            &path,
            format!(
                r#"{{"nextAt":{},"failures":1,"error":"Anthropic is limiting quota checks"}}"#,
                t0 + 230_000
            ),
        )
    })));
    agent.claude_tick(f.t0).await;
    f.api.on_send(None);
    agent.claude_tick(f.t0 + 200_000).await;
    assert_eq!(f.api.calls(), 1);
    agent.claude_tick(f.t0 + 231_000).await;
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_retry_after_counts_from_the_answer() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(429, "{}", Some(120));
    let agent = f.agent();
    f.api.on_send(Some(Box::new(|| {
        std::thread::sleep(std::time::Duration::from_secs(2))
    }))); // a slow answer
    agent.claude_tick(f.t0).await;
    f.api.on_send(None);
    agent.claude_tick(f.t0 + 125_500).await; // 120 + 5 s after the request went out, not after the answer
    assert_eq!(f.api.calls(), 1);
    agent.claude_tick(f.t0 + 128_000).await;
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_refusal_another_agent_got_meanwhile_is_not_overwritten() {
    let f = Fixture::new();
    f.sign_in("revoked", f.t0 + 7_200_000);
    f.api.respond(503, "", None);
    let agent = f.agent();
    // while this request runs, another agent sends the same token and gets a 401
    let (path, t0, print) = (f.schedule_path(), f.t0, fingerprint("revoked"));
    f.api.on_send(Some(Box::new(move || {
        write(
            &path,
            format!(
                r#"{{"nextAt":{},"failures":0,"refused":"{print}","refusedAt":{t0}}}"#,
                t0 + 60_000
            ),
        )
    })));
    agent.claude_tick(f.t0).await;
    f.api.on_send(None);

    let restarted = f.agent();
    let mut t = f.t0 + 10 * MINUTE;
    while t < f.t0 + 30 * MINUTE {
        restarted.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 1);
}

#[tokio::test]
async fn an_hour_long_wait_another_agent_got_is_kept_after_a_slow_answer() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    let (path, t0) = (f.schedule_path(), f.t0);
    f.api.on_send(Some(Box::new(move || {
        // a second into this pass another agent is told to wait an hour (+5 s); this answer then takes 2 s
        write(
            &path,
            format!(r#"{{"nextAt":{},"failures":1}}"#, t0 + 1_000 + 3_605_000),
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    })));
    agent.claude_tick(f.t0).await;
    f.api.on_send(None);
    agent.claude_tick(f.t0 + 10 * MINUTE).await;
    assert_eq!(f.api.calls(), 1);
}

#[tokio::test]
async fn a_newer_refusal_of_the_same_token_is_kept() {
    let f = Fixture::new();
    f.sign_in("revoked", f.t0 + 7_200_000);
    f.api.respond(401, "", None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;

    // half an hour later this agent tries the token again; meanwhile another agent gets a fresh 401 for it
    f.api.respond(503, "", None);
    let (path, t0, print) = (f.schedule_path(), f.t0, fingerprint("revoked"));
    f.api.on_send(Some(Box::new(move || {
        write(
            &path,
            format!(
                r#"{{"nextAt":{},"failures":0,"refused":"{print}","refusedAt":{}}}"#,
                t0 + 31 * MINUTE,
                t0 + 30 * MINUTE
            ),
        )
    })));
    agent.claude_tick(f.t0 + 30 * MINUTE).await;
    f.api.on_send(None);
    assert_eq!(f.api.calls(), 2);

    let restarted = f.agent();
    let mut t = f.t0 + 36 * MINUTE;
    while t < f.t0 + 60 * MINUTE {
        restarted.claude_tick(t).await;
        t += 30_000;
    }
    assert_eq!(f.api.calls(), 2);
}

#[tokio::test]
async fn a_restart_keeps_the_back_off_going() {
    let f = Fixture::new();
    f.sign_in("token-1", f.t0 + 7_200_000);
    f.api.respond(403, "", None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;
    agent.claude_tick(f.t0 + MINUTE).await; // second failure: next at 3 minutes

    let restarted = f.agent();
    restarted.claude_tick(f.t0 + 3 * MINUTE).await; // third failure: the next one waits 4 minutes
    restarted.claude_tick(f.t0 + 5 * MINUTE).await;
    assert_eq!(f.api.calls(), 3);
    restarted.claude_tick(f.t0 + 7 * MINUTE).await;
    assert_eq!(f.api.calls(), 4);
}

// macOS: Claude Code renews its sign-in in the Keychain. A credentials file left behind by another tool or an older
// version keeps a token that expired hours ago; it must not hide the renewed one.
#[tokio::test]
async fn a_stale_file_gives_way_to_the_renewed_keychain_sign_in() {
    let f = Fixture::new();
    f.sign_in("stale-file-token", f.t0 - 4 * 3_600_000);
    *f.keychain.lock().unwrap() = Some(sign_in_json("keychain-token", f.t0 + 3_600_000));
    f.api.respond(200, USAGE, None);
    let agent = f.agent();
    agent.claude_tick(f.t0).await;
    assert_eq!(f.api.calls(), 1);
    assert_eq!(
        f.api.last_token.lock().unwrap().as_deref(),
        Some("keychain-token")
    );
    assert_eq!(claude(&agent).error, None);
}

#[tokio::test]
async fn a_valid_file_is_used_without_reading_the_keychain() {
    let f = Fixture::new();
    f.sign_in("file-token", f.t0 + 3_600_000);
    let reads = Arc::new(AtomicUsize::new(0));
    let (counter, t0) = (reads.clone(), f.t0);
    let agent = f.agent_with(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(sign_in_json("keychain-token", t0 + 7_200_000))
    }));
    f.api.respond(200, USAGE, None);
    agent.claude_tick(f.t0).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "each Keychain read starts the security tool: not while the file is good"
    );
    assert_eq!(
        f.api.last_token.lock().unwrap().as_deref(),
        Some("file-token")
    );
}

#[tokio::test]
async fn when_both_are_expired_the_message_stays_and_nothing_is_sent() {
    let f = Fixture::new();
    f.sign_in("stale-file-token", f.t0 - 4 * 3_600_000);
    *f.keychain.lock().unwrap() = Some(sign_in_json("old-keychain-token", f.t0 - 3_600_000));
    let agent = f.agent();
    agent.claude_tick(f.t0).await;
    assert_eq!(f.api.calls(), 0);
    assert!(claude(&agent).error.unwrap().contains("expired"));
}

#[test]
fn the_sign_in_that_expires_later_is_used() {
    let t0 = crate::server::now_ms();
    for (file_hours, keychain_hours, expected) in [
        (-4, 1, "keychain"),  // the file expired, the Keychain was renewed
        (1, 2, "keychain"),   // (the caller reads the Keychain only once the file has expired)
        (-4, -2, "keychain"), // both expired: the later one, so the message says what Claude Code last saved
        (-1, -4, "file"),
    ] {
        let file = sign_in_json("file", t0 + file_hours * 3_600_000);
        let keychain = sign_in_json("keychain", t0 + keychain_hours * 3_600_000);
        let chosen = pick_credentials(Some(&file), Some(&keychain));
        assert_eq!(
            chosen,
            Some(if expected == "file" {
                file.as_str()
            } else {
                keychain.as_str()
            })
        );
    }
}

#[test]
fn a_file_without_a_token_or_no_keychain_answer_is_handled() {
    let keychain = sign_in_json("keychain", crate::server::now_ms() + 3_600_000);
    let desktop_only = r#"{"claudeAiOauth":{"subscriptionType":"max"}}"#;
    assert_eq!(
        pick_credentials(Some(desktop_only), Some(&keychain)),
        Some(keychain.as_str())
    );
    assert_eq!(
        pick_credentials(None, Some(&keychain)),
        Some(keychain.as_str())
    );
    assert_eq!(
        pick_credentials(Some(desktop_only), None),
        Some(desktop_only)
    );
    assert_eq!(
        pick_credentials(Some(desktop_only), Some("not json")),
        Some(desktop_only)
    );
    assert_eq!(pick_credentials(None, None), None);
}
