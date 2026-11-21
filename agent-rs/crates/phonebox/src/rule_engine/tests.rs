//! `RuleTests.cs` (13 of its rule rows), `RuleWindowTests`, `ShortcutRuleFollowUpTests` (the two rule ones),
//! `CodexBarTests.QuotaAboveRuleKeepsFiringForCachedLimitsWithErrorAndSkipsOmittedProviders` and the five rule tests of
//! `AuditRegressionTests`. The release time is waited out on a test clock instead of with sleeps.

use std::sync::atomic::AtomicI64;

use jiff::civil::date;

use super::*;
use crate::config::normalize::normalize_days;
use crate::github_monitor::{GhPr, GhSnapshot};
use crate::quota::{QuotaLimit, QuotaProvider};

const START: i64 = 1_800_000_000_000; // the agent's start, unix ms

fn ticks(h: i64, m: i64) -> i64 {
    (h * 60 + m) * 60 * 10_000_000
}

fn clock_ticks(text: &str) -> i64 {
    let (h, m) = text.split_once(':').unwrap();
    ticks(h.parse().unwrap(), m.parse().unwrap())
}

#[test]
fn days() {
    for (days, day, expected) in [
        (None, Weekday::Sunday, true),
        (Some("mon,tue,wed,thu,fri"), Weekday::Friday, true),
        (Some("mon,tue,wed,thu,fri"), Weekday::Saturday, false),
        (Some("sat,sun"), Weekday::Sunday, true),
    ] {
        assert_eq!(on_day(days, day), expected, "{days:?} {day:?}");
    }
}

#[test]
fn day_lists_are_cleaned() {
    for (input, expected) in [
        ("weekdays", Some("mon,tue,wed,thu,fri")),
        ("Sun, sat", Some("sat,sun")),
        ("weekdays weekends", None), // every day
        ("funday", None),
        ("", None),
    ] {
        assert_eq!(normalize_days(Some(input)).as_deref(), expected, "{input}");
    }
}

#[test]
fn days_and_hours_combine() {
    let r = RuleConfig {
        days: Some("mon,tue,wed,thu,fri".into()),
        during: Some("09:00-18:00".into()),
        ..RuleConfig::default()
    };
    assert!(allowed(&r, date(2026, 11, 9).at(10, 0, 0, 0))); // Monday 10:00
    assert!(!allowed(&r, date(2026, 11, 9).at(19, 0, 0, 0))); // Monday 19:00
    assert!(!allowed(&r, date(2026, 11, 8).at(10, 0, 0, 0))); // Sunday
    assert!(allowed(
        &RuleConfig::default(),
        date(2026, 11, 8).at(3, 0, 0, 0)
    ));
}

#[test]
fn pushed_values_compare() {
    for (json, op, arg, expected) in [
        ("29.5", ">", "28", true),
        ("27", ">", "28", false),
        ("\"12\"", "<", "20", true), // a number sent as text
        ("\"open\"", "=", "OPEN", true),
        ("\"closed\"", "!=", "open", true),
        ("{\"value\":31,\"sub\":\"living room\"}", ">", "30", true),
        ("\"warm\"", ">", "20", false), // not a number
        ("28", ">=", "28", true),       // the boundary counts for >= and <=, not for > and <
        ("27.9", ">=", "28", false),
        ("28", ">", "28", false),
        ("20", "<=", "20", true),
        ("\"20.5\"", "<=", "20", false),
        ("20", "<", "20", false),
        ("{\"value\":30,\"sub\":\"living room\"}", ">=", "30", true),
        ("\"warm\"", "<=", "20", false),
    ] {
        let d: Value = serde_json::from_str(json).unwrap();
        assert_eq!(compare(&d, op, Some(arg)), expected, "{json} {op} {arg}");
    }
}

fn pushed(at: i64) -> PushItem {
    PushItem {
        data: json!(21.5),
        at,
        expires_at: None,
    }
}

#[test]
fn stale_holds_once_a_value_is_older_than_the_limit() {
    let now = START + 3_600_000;
    assert!(!stale("temp", Some(&pushed(now - 30_000)), Some("60"), now, START).0); // fresh
    assert_eq!(
        stale(
            "temp",
            Some(&pushed(now - 5 * 60_000)),
            Some("60"),
            now,
            START
        ),
        (true, "temp: no update for 5 min".to_string())
    );
    assert!(stale("temp", Some(&pushed(now - 60_000)), Some("60"), now, START).0); // exactly the limit
}

#[test]
fn stale_gives_the_sender_time_after_the_agent_starts() {
    // missing (never pushed, deleted or expired): not stale during the first N seconds, stale after them
    assert!(!stale("temp", None, Some("60"), START + 59_000, START).0);
    assert_eq!(
        stale("temp", None, Some("60"), START + 60_000, START),
        (true, "temp: no value".to_string())
    );
    // a value kept from before a restart counts from the start too, but reports its real age
    let old = pushed(START - 2 * 3_600_000);
    assert!(!stale("temp", Some(&old), Some("60"), START + 10_000, START).0);
    assert_eq!(
        stale("temp", Some(&old), Some("60"), START + 60_000, START).1,
        "temp: no update for 2 h"
    );
}

#[test]
fn stale_without_a_valid_time_never_holds() {
    for arg in [
        None,
        Some(""),
        Some("soon"),
        Some("0"),
        Some("-5"),
        Some("Infinity"),
        Some("NaN"),
    ] {
        assert!(
            !stale("temp", None, arg, START + 86_400_000, START).0,
            "{arg:?}"
        );
    }
}

#[test]
fn stale_ignores_a_match_that_cannot_be_a_push_key() {
    for key in [
        "https://calendar.example/feed.ics?token=secret",
        "two words",
    ] {
        assert_eq!(
            stale(key, None, Some("60"), START + 86_400_000, START),
            (false, String::new())
        );
    }
}

fn store_with(json: &str) -> (tempfile::TempDir, Arc<ConfigStore>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, json).unwrap();
    let store = Arc::new(ConfigStore::new(path));
    (dir, store)
}

#[test]
fn stale_times_are_cleaned() {
    for (arg, expected) in [
        ("\"90\"", "90"),
        ("\"2.4\"", "2"),
        ("\"999999\"", "86400"),
        ("\"soon\"", "60"),
        ("\"Infinity\"", "60"),
        ("null", "60"),
    ] {
        let (_d, cfg) = store_with(&format!(
            r#"{{ "rules": [ {{ "when": "push", "match": "temp", "op": "stale", "arg": {arg}, "then": "notify" }}, {{ "when": "push", "match": "temp", "op": ">=", "arg": "30", "then": "notify" }} ] }}"#
        ));
        let rules = cfg.get().rules.clone();
        assert_eq!(rules[0].op.as_deref(), Some("stale"));
        assert_eq!(rules[0].arg.as_deref(), Some(expected), "{arg}");
        assert_eq!(rules[1].op.as_deref(), Some(">="));
        assert_eq!(rules[1].arg.as_deref(), Some("30")); // other operators keep their value as typed
    }
}

#[test]
fn time_windows_require_valid_endpoints() {
    for (window, h, m, expected) in [
        ("08:30-typo", 8, 30, false),
        ("08:30-09:30-extra", 8, 30, false),
        ("08:30-1.01:00", 8, 30, false),
        ("08:30", 8, 30, true),
        ("22:00-07:00", 0, 0, true),
        ("22:00-24:00", 23, 0, true),
        ("24:00-07:00", 0, 0, true),
    ] {
        assert_eq!(in_window(window, ticks(h, m)), expected, "{window}");
    }
}

#[test]
fn rule_options_are_clamped() {
    let (_d, cfg) = store_with(
        r#"{ "rules": [ { "when": "always", "then": "shortcut", "priority": 42, "forSec": -5, "cooldownMin": 99999, "countdownSec": 99, "days": "weekdays", "op": "~", "during": "  " } ] }"#,
    );
    let config = cfg.get();
    let r = &config.rules[0];
    assert_eq!(config.rules.len(), 1);
    assert_eq!(r.priority, 9);
    assert_eq!(r.for_sec, 0);
    assert_eq!(r.cooldown_min, 1440);
    assert_eq!(r.countdown_sec, Some(30));
    assert_eq!(r.days.as_deref(), Some("mon,tue,wed,thu,fri"));
    assert_eq!(r.op, None);
    assert_eq!(r.during, None);
}

/// `RuleWindowTests.TimeWindows`.
#[test]
fn time_windows() {
    for (window, now, expected) in [
        ("22:00-07:00", "23:30", true),
        ("22:00-07:00", "06:59", true),
        ("22:00-07:00", "07:00", false),
        ("09:00-17:00", "12:00", true),
        ("09:00-17:00", "17:00", false),
        ("08:30", "08:30", true),
        ("08:30", "08:31", false),
        ("nonsense", "08:30", false),
        // "until midnight" written as 24:00 holds all evening, not only its first minute
        ("18:00-24:00", "23:30", true),
        ("18:00-24:00", "20:00", true),
        ("18:00-24:00", "17:59", false),
        ("18:00-24:00", "00:00", false),
    ] {
        assert_eq!(
            in_window(window, clock_ticks(now)),
            expected,
            "{window} at {now}"
        );
    }
}

/// Fixed answers for the conditions the tests use.
#[derive(Default)]
struct FakeInputs {
    front: Mutex<Option<String>>,
    github: Mutex<Option<GhSnapshot>>,
}

impl RuleInputs for FakeInputs {
    fn front_app(&self) -> Option<String> {
        lock(&self.front).clone()
    }
    fn github(&self) -> Option<GhSnapshot> {
        lock(&self.github).clone()
    }
}

/// An engine over a settings file and a push store in a temporary folder, with a monotonic clock the test moves.
struct Rig {
    dir: tempfile::TempDir,
    cfg: Arc<ConfigStore>,
    push: Arc<PushStore>,
    engine: Arc<RuleEngine>,
    mono: Arc<AtomicI64>,
}

impl Rig {
    fn new(settings: &str, inputs: Arc<dyn RuleInputs>) -> Rig {
        Rig::with(settings, inputs, None)
    }

    fn with(settings: &str, inputs: Arc<dyn RuleInputs>, warn: Option<Warn>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, settings).unwrap();
        let cfg = Arc::new(ConfigStore::new(path));
        let push = PushStore::new(&dir.path().join("data.json"));
        let mono = Arc::new(AtomicI64::new(1_000_000));
        let clock = RuleClock {
            mono_ms: {
                let mono = mono.clone();
                Arc::new(move || mono.load(Ordering::SeqCst))
            },
            unix_ms: Arc::new(crate::server::now_ms),
            local: Arc::new(|| jiff::Zoned::now().datetime()),
        };
        let mut engine = RuleEngine::new(cfg.clone(), push.clone(), None, inputs, clock);
        if let Some(warn) = warn {
            engine = engine.with_warn(warn);
        }
        Rig {
            dir,
            cfg,
            push,
            engine,
            mono,
        }
    }

    fn release(&self) {
        self.mono
            .fetch_add(RELEASE_SEC * 1000 + 300, Ordering::SeqCst);
    }

    fn save(&self, settings: &str) {
        self.cfg.save(settings, None).unwrap();
    }

    fn notify_text(&self, seq: i64) -> String {
        let events = self.push.since(seq);
        let notify: Vec<_> = events.iter().filter(|e| e.kind == "notify").collect();
        assert_eq!(notify.len(), 1, "{events:?}");
        notify[0].data.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn has(&self, key: &str) -> bool {
        self.push.snapshot().contains_key(key)
    }
}

#[test]
fn stale_rule_fires_for_an_expired_value_and_ends_when_the_value_comes_back() {
    let rig = Rig::new(
        r#"{ "rules": [ { "id": "quiet", "name": "Quiet", "when": "push", "match": "a3-vibration", "op": "stale", "arg": "5",
                          "then": "notify", "text": "{detail}", "whileActive": false } ] }"#,
        Arc::new(FakeInputs::default()),
    );
    // pushed with a one-second ttl, then expired: the store no longer has it
    rig.push.set("a3-vibration", json!(0.01), Some(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert!(rig.push.get("a3-vibration").is_none());

    // the grace counts from here, not from before the sleep
    rig.engine.set_started_at_ms(crate::server::now_ms());
    rig.engine.evaluate(); // just started: the sender still has time
    assert!(rig.engine.active().is_empty());

    rig.engine
        .set_started_at_ms(rig.engine.started_at_ms() - 10_000);
    let seq = rig.push.seq();
    rig.engine.evaluate();
    assert_eq!(rig.engine.active(), ["quiet"]);
    assert_eq!(rig.notify_text(seq), "a3-vibration: no value");

    rig.push.set("a3-vibration", json!(0.01), None).unwrap();
    rig.engine.evaluate();
    assert_eq!(rig.engine.active(), ["quiet"]); // a rule ends once its condition stays off a while
    rig.release();
    rig.engine.evaluate();
    assert!(rig.engine.active().is_empty());
}

fn unique(prefix: &str) -> String {
    // the remembered toggle state is process-wide, keyed by the shortcut's id
    format!(
        "{prefix}-{}",
        crate::server::now_ms() * 1000 + rand_suffix()
    )
}

fn rand_suffix() -> i64 {
    static N: AtomicI64 = AtomicI64::new(0);
    N.fetch_add(1, Ordering::SeqCst) % 1000
}

fn lamp_of(rig: &Rig) -> crate::config::Shortcut {
    rig.cfg.get().shortcuts[0].clone()
}

fn lamp_state(rig: &Rig) -> Option<bool> {
    crate::shortcuts::state(&lamp_of(rig), Some(&rig.push), None)
}

#[test]
fn while_active_shortcut_rules_keep_already_on_toggles_on_and_review_requested_counts_only_matching_non_drafts()
 {
    let lamp = unique("lamp");
    let inputs = Arc::new(FakeInputs::default());
    let pr = |repo: &str, number: i32, title: &str, author: &str, draft: bool| GhPr {
        repo: repo.into(),
        number,
        title: title.into(),
        author: Some(author.into()),
        draft,
        review: None,
        ci: None,
        updated: 0,
    };
    *lock(&inputs.github) = Some(GhSnapshot {
        user: Some("me".into()),
        notifications: 0,
        review_requests: vec![
            pr("org/target", 42, "Ready", "alice", false),
            pr("org/target", 43, "Draft", "bob", true),
            pr("org/other", 99, "Other", "eve", false),
        ],
        my_prs: vec![],
        updated_at: Some(0),
        error: None,
    });
    let rig = Rig::new(
        &format!(
            r#"{{
              "shortcuts": [{{ "id": "{lamp}", "label": "Lamp", "cmd": "builtin:macro", "toggle": true }}],
              "rules": [
                {{ "id": "call", "name": "Call", "when": "push", "match": "mic", "then": "shortcut", "shortcut": "{lamp}", "whileActive": true }},
                {{ "id": "rev", "name": "Rev", "when": "review-requested", "match": "org/target", "then": "notify", "text": "{{detail}}" }}
              ]
            }}"#
        ),
        inputs,
    );
    assert_eq!(
        crate::shortcuts::run(&rig.cfg.get(), &lamp, Some(&rig.push), None),
        None
    );
    assert_eq!(lamp_state(&rig), Some(true));

    rig.push.set("mic", json!(true), None).unwrap();
    let seq = rig.push.seq();
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(true));
    assert_eq!(rig.notify_text(seq), "org/target#42");

    rig.engine.pause(5);
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(false));
}

fn held_settings(lamp: &str, lamp_json: &str, rules: &[(&str, bool)]) -> String {
    let rules: Vec<String> = rules
        .iter()
        .map(|(id, enabled)| {
            format!(
                r#"{{ "id": "{id}", "name": "{id}", "enabled": {enabled}, "when": "push", "match": "mic", "then": "shortcut", "shortcut": "{lamp}", "whileActive": true }}"#
            )
        })
        .collect();
    format!(
        r#"{{ "shortcuts": [{lamp_json}], "rules": [{}] }}"#,
        rules.join(",")
    )
}

/// Two "while active" rules hold the same toggle on; the first one to end must leave it on for the other.
#[test]
fn a_toggle_held_by_two_rules_stays_on_until_the_last_one_ends() {
    let lamp = unique("held-lamp");
    let lamp_json =
        format!(r#"{{ "id": "{lamp}", "label": "Lamp", "cmd": "builtin:macro", "toggle": true }}"#);
    let rig = Rig::new("{}", Arc::new(FakeInputs::default()));
    rig.save(&held_settings(
        &lamp,
        &lamp_json,
        &[("a", true), ("b", true)],
    ));
    assert_eq!(lamp_state(&rig), Some(false));

    rig.push.set("mic", json!(true), None).unwrap();
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(true));

    rig.save(&held_settings(
        &lamp,
        &lamp_json,
        &[("a", true), ("b", false)],
    )); // b ends, a still holds
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(true));

    rig.save(&held_settings(
        &lamp,
        &lamp_json,
        &[("a", false), ("b", false)],
    )); // the last holder ends
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(false));
}

/// Switching a held toggle back off can fail; that must show up in the log instead of being dropped.
#[test]
fn a_failed_release_is_logged() {
    let lamp = unique("release-lamp");
    let lamp_json = format!(
        r#"{{ "id": "{lamp}", "label": "Lamp", "cmd": "builtin:macro", "toggle": true, "offCmd": "builtin:keys", "offArgs": "not_a_key" }}"#
    );
    let lines: Arc<Mutex<Vec<String>>> = Default::default();
    let warn: Warn = {
        let lines = lines.clone();
        Arc::new(move |line| lock(&lines).push(line.to_string()))
    };
    let rig = Rig::with("{}", Arc::new(FakeInputs::default()), Some(warn));
    rig.save(&held_settings(&lamp, &lamp_json, &[("a", true)]));

    rig.push.set("mic", json!(true), None).unwrap();
    rig.engine.evaluate();
    assert_eq!(lamp_state(&rig), Some(true));

    rig.save(&held_settings(&lamp, &lamp_json, &[("a", false)]));
    rig.engine.evaluate();
    let lines = lock(&lines);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("rule a") && l.contains("back off")),
        "{lines:?}"
    );
}

/// The quota monitor's own view: a cached reading with an error still counts, a provider CodexBar omitted does not.
struct QuotaInputs(Arc<crate::quota::QuotaMonitor>);

impl RuleInputs for QuotaInputs {
    fn quotas(&self) -> Vec<QuotaProvider> {
        self.0.snapshot(crate::server::now_ms())
    }
}

#[test]
fn quota_above_rule_keeps_firing_for_cached_limits_with_error_and_skips_omitted_providers() {
    let home = tempfile::tempdir().unwrap();
    let quotas = Arc::new(crate::quota::QuotaMonitor::new(
        None,
        home.path(),
        home.path(),
    ));
    {
        let mut state = lock(&quotas.state);
        let now = crate::server::now_ms();
        state.claude = Some(QuotaProvider::new(
            "claude",
            "Claude",
            "Max",
            vec![QuotaLimit::new(
                "5-hour session",
                92.0,
                None,
                Some(18000),
                "warning",
            )],
            None,
            Some(now),
            Some("Could not fetch quota (HTTP 500)".into()),
        ));
        let mut cursor = QuotaProvider::new(
            "cursor",
            "Cursor",
            "Pro",
            vec![],
            None,
            Some(now),
            Some("CodexBar omitted this provider".into()),
        );
        cursor.source = "codexbar".into();
        state.external = vec![cursor];
    }
    let rig = Rig::new(
        r#"{"rules":[
          {"id":"q-claude","name":"Claude High","when":"quota-above","match":"claude","value":80,"then":"push","key":"q_claude","pushValue":"hot","whileActive":true},
          {"id":"q-cursor","name":"Cursor High","when":"quota-above","match":"cursor","value":80,"then":"push","key":"q_cursor","pushValue":"hot","whileActive":true}
        ]}"#,
        Arc::new(QuotaInputs(quotas)),
    );
    rig.engine.evaluate();
    let active = rig.engine.active();
    assert!(active.contains(&"q-claude".to_string()));
    assert!(!active.contains(&"q-cursor".to_string()));
    assert!(rig.has("q_claude"));
    assert!(!rig.has("q_cursor"));
}

// ---- AuditRegressionTests, F5: rules after an edit or a pause

fn on_air(name: &str, when: &str, m: Option<&str>) -> String {
    let m = m.map(|m| format!(r#","match":"{m}""#)).unwrap_or_default();
    format!(
        r#"{{"rules":[{{"id":"meeting","name":"{name}","when":"{when}"{m},"then":"push","key":"onair","pushValue":"live","whileActive":true,"cooldownMin":30}}]}}"#
    )
}

#[test]
fn an_edit_or_a_pause_does_not_leave_a_while_active_rule_off_for_its_rest_time() {
    let rig = Rig::new(
        &on_air("Meeting", "always", None),
        Arc::new(FakeInputs::default()),
    );
    rig.engine.evaluate();
    assert!(rig.has("onair"));
    rig.save(&on_air("Meeting (renamed)", "always", None));
    rig.engine.evaluate();
    assert!(rig.has("onair"));
    rig.engine.pause(1);
    rig.engine.evaluate();
    assert!(!rig.has("onair"));
    rig.engine.pause(0);
    rig.engine.evaluate();
    assert!(rig.has("onair"));
}

#[test]
fn the_rest_time_still_holds_when_the_condition_really_ends_and_comes_back() {
    let rig = Rig::new(
        &on_air("Meeting", "push", Some("trigger")),
        Arc::new(FakeInputs::default()),
    );
    rig.push.set("trigger", json!(true), None).unwrap();
    rig.engine.evaluate();
    assert!(rig.has("onair"));
    rig.push.remove("trigger");
    rig.engine.evaluate();
    rig.release();
    rig.engine.evaluate();
    assert!(!rig.has("onair"));
    rig.push.set("trigger", json!(true), None).unwrap();
    rig.engine.evaluate();
    assert!(!rig.has("onair"));
}

#[test]
fn an_app_in_front_rule_follows_the_foreground_app() {
    let inputs = Arc::new(FakeInputs::default());
    let rig = Rig::new(
        r#"{"rules":[{"id":"focus","name":"Focus","when":"app-in-front","match":"code","then":"goto","page":"work","whileActive":true}]}"#,
        inputs.clone(),
    );
    *lock(&inputs.front) = Some("Visual Studio Code".into());
    rig.engine.evaluate();
    assert!(rig.push.since(0).iter().any(|e| e.kind == "goto"));
    assert!(rig.engine.active().contains(&"focus".to_string()));
    *lock(&inputs.front) = Some("Safari".into());
    rig.engine.evaluate();
    rig.release();
    rig.engine.evaluate();
    assert!(rig.push.since(0).iter().any(|e| e.kind == "back"));
    assert!(!rig.engine.active().contains(&"focus".to_string()));
}

#[test]
fn a_pushed_off_value_does_not_turn_a_push_rule_on_in_any_case() {
    for (body, on) in [
        ("on", true),
        ("off", false),
        ("OFF", false),   // MQTT / Home Assistant style
        ("False", false), // PowerShell's $false as text
        ("No", false),
    ] {
        let rig = Rig::new(
            &on_air("TV", "push", Some("tv")),
            Arc::new(FakeInputs::default()),
        );
        rig.push
            .set("tv", crate::push_store::parse_value(body), None)
            .unwrap();
        rig.engine.evaluate();
        assert_eq!(
            rig.engine.active().contains(&"meeting".to_string()),
            on,
            "{body}"
        );
        // the same key read by a toggle shortcut agrees
        let sc = crate::config::Shortcut {
            id: "s".into(),
            state_key: Some("tv".into()),
            ..crate::config::Shortcut::default()
        };
        assert_eq!(
            crate::shortcuts::state(&sc, Some(&rig.push), None),
            Some(on)
        );
    }
}

#[test]
fn a_push_rule_judges_a_json_payload_by_its_value() {
    for (json, on) in [
        (r#"{"value":"on","sub":"living room"}"#, true),
        (r#"{"title":"Sensor"}"#, true), // no value: an object still counts as on
        (r#"{"value":"off","sub":"living room"}"#, false), // README › Webhooks payload shape
        (r#"{"value":false}"#, false),
        (r#"{"value":0,"unit":"W"}"#, false),
    ] {
        let rig = Rig::new(
            &on_air("TV", "push", Some("tv")),
            Arc::new(FakeInputs::default()),
        );
        rig.push
            .set("tv", serde_json::from_str(json).unwrap(), None)
            .unwrap();
        rig.engine.evaluate();
        assert_eq!(
            rig.engine.active().contains(&"meeting".to_string()),
            on,
            "{json}"
        );
    }
}

#[test]
fn a_pause_is_capped_at_a_day_and_zero_resumes() {
    let rig = Rig::new("{}", Arc::new(FakeInputs::default()));
    assert_eq!(rig.engine.paused_until(), None);
    let before = crate::server::now_ms();
    rig.engine.pause(99_999);
    let until = rig.engine.paused_until().unwrap();
    assert!(until >= before + 1440 * 60_000 && until <= crate::server::now_ms() + 1440 * 60_000);
    rig.engine.pause(0);
    assert_eq!(rig.engine.paused_until(), None);
    let _ = &rig.dir;
}

#[test]
fn the_state_lists_the_rules_that_hold_and_the_pause_once_the_engine_is_in() {
    use crate::state::StateSource;
    let slot = Arc::new(RulesSlot::default());
    let (active, paused) = (ActiveRules(slot.clone()), RulesPaused(slot.clone()));
    assert_eq!(active.snapshot(), json!([]));
    assert_eq!(paused.snapshot(), Value::Null);
    let rig = Rig::new(
        r#"{"rules":[{"id":"on","name":"On","when":"always","then":"notify"}]}"#,
        Arc::new(FakeInputs::default()),
    );
    slot.set(rig.engine.clone());
    rig.engine.evaluate();
    assert_eq!(active.snapshot(), json!(["on"]));
    rig.engine.pause(10);
    assert_eq!(paused.snapshot(), json!(rig.engine.paused_until().unwrap()));
}
