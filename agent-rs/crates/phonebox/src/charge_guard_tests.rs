//! `ChargeGuardTests` (8), `ChargeRecoveryTests` (11), `ChargeGuardSwitchTests` (2) and
//! `MonitorAuditTests.ChargeGuardKeepsTryingToRestoreThePowerAfterAFailedSwitchOn`, against a Home Assistant fake on
//! loopback that applies a `turn_off` and can hold its answer until the test lets it go.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use super::*;
use crate::testutil::{FakeHttp, Reply};

const NOW: i64 = 1_000_000_000;

fn c() -> ChargeConfig {
    ChargeConfig {
        enabled: true,
        min: 50,
        max: 80,
        ..ChargeConfig::default()
    }
}

fn shortcuts() -> ChargeConfig {
    ChargeConfig {
        method: "shortcuts".into(),
        ..c()
    }
}

fn at(level: i32, plugged: bool, age_ms: i64) -> PhoneBatteryReport {
    PhoneBatteryReport {
        level,
        plugged,
        temp: None,
        at: NOW - age_ms,
        source: None,
        usb: None,
    }
}

#[test]
fn stops_charging_at_the_top() {
    assert_eq!(
        decide(&c(), Some(&at(80, true, 0)), NOW, None, 0),
        Some("off")
    );
}

#[test]
fn starts_charging_at_the_bottom() {
    assert_eq!(
        decide(
            &c(),
            Some(&at(50, false, 0)),
            NOW,
            Some("off"),
            NOW - 60_000
        ),
        Some("on")
    );
}

#[test]
fn leaves_the_power_alone_in_between() {
    assert_eq!(
        decide(&c(), Some(&at(65, true, 0)), NOW, Some("on"), NOW - 60_000),
        None
    );
    assert_eq!(
        decide(
            &c(),
            Some(&at(65, false, 0)),
            NOW,
            Some("off"),
            NOW - 60_000
        ),
        None
    );
}

/// Shortcuts may switch the charger inside a rooted phone, which then stays plugged in while not charging.
#[test]
fn shortcuts_start_charging_at_the_bottom_while_plugged_in() {
    let s = shortcuts();
    assert_eq!(
        decide(&s, Some(&at(50, true, 0)), NOW, Some("off"), NOW - 60_000),
        Some("on")
    );
    assert_eq!(decide(&s, Some(&at(40, true, 0)), NOW, None, 0), Some("on")); // after an agent restart
    assert_eq!(
        decide(&s, Some(&at(45, true, 0)), NOW, Some("on"), NOW - 60_000),
        None
    ); // just switched: wait
    // still low later: again
    assert_eq!(
        decide(
            &s,
            Some(&at(45, true, 0)),
            NOW,
            Some("on"),
            NOW - 11 * 60_000
        ),
        Some("on")
    );
    // in between: hold
    assert_eq!(
        decide(
            &s,
            Some(&at(65, true, 0)),
            NOW,
            Some("off"),
            NOW - 11 * 60_000
        ),
        None
    );
}

#[test]
fn a_plug_is_not_switched_on_while_the_phone_is_plugged_in() {
    assert_eq!(decide(&c(), Some(&at(40, true, 0)), NOW, None, 0), None);
}

#[test]
fn repeats_a_switch_that_did_not_take_only_after_a_while() {
    assert_eq!(
        decide(&c(), Some(&at(85, true, 0)), NOW, Some("off"), NOW - 60_000),
        None
    ); // just switched: wait
    // still charging later: again
    assert_eq!(
        decide(
            &c(),
            Some(&at(85, true, 0)),
            NOW,
            Some("off"),
            NOW - 11 * 60_000
        ),
        Some("off")
    );
}

#[test]
fn never_leaves_a_silent_phone_without_power() {
    assert_eq!(
        decide(
            &c(),
            Some(&at(70, false, 11 * 60_000)),
            NOW,
            Some("off"),
            NOW - 20 * 60_000
        ),
        Some("on")
    );
    assert_eq!(decide(&c(), None, NOW, Some("off"), 0), Some("on"));
    assert_eq!(decide(&c(), None, NOW, Some("on"), 0), None);
}

#[test]
fn limits_are_kept_apart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{ "charge": { "enabled": true, "min": 70, "max": 60, "method": "magic" } }"#,
    )
    .unwrap();
    let charge = ConfigStore::new(path).get().charge.clone();
    assert_eq!(charge.min, 70);
    assert_eq!(charge.max, 75);
    assert_eq!(charge.method, "ha");
}

#[test]
fn a_report_from_before_a_clock_rollback_cannot_disable_the_supply() {
    const NOW: i64 = 2_000_000;
    let c = ChargeConfig {
        min: 50,
        max: 80,
        ..ChargeConfig::default()
    };
    for (offset, last, expected) in [
        (60_000, None, None),
        (60_000, Some("off"), Some("on")),
        (0, None, Some("off")),
        (-60_000, Some("off"), None),
        (-600_000, None, Some("off")),
        (-600_001, Some("off"), Some("on")),
    ] {
        let reading = PhoneBatteryReport {
            level: 95,
            plugged: true,
            temp: None,
            at: NOW + offset,
            source: None,
            usb: None,
        };
        assert_eq!(
            decide(&c, Some(&reading), NOW, last, NOW - 10_000),
            expected,
            "{offset} {last:?}"
        );
    }
}

/// A one-shot signal (C#'s TaskCompletionSource).
#[derive(Clone)]
struct Signal(Arc<tokio::sync::watch::Sender<bool>>);

impl Signal {
    fn new() -> Signal {
        Signal(Arc::new(tokio::sync::watch::channel(false).0))
    }
    fn set(&self) {
        self.0.send_replace(true);
    }
    async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|set| *set).await;
    }
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(8), f)
        .await
        .expect("timed out")
}

/// The Home Assistant side: two plugs, every call and its credential, and the knobs the tests turn.
struct Plugs {
    power: Mutex<HashMap<String, bool>>,
    calls: Mutex<Vec<String>>,
    authorizations: Mutex<Vec<String>>,
    fail_on: AtomicI32,
    fail_off_response: AtomicBool,
    required_authorization: Mutex<Option<String>>,
    off_entered: Signal,
    release_off: Signal,
}

/// `ChargeRecoveryFixture`: the production guard, settings and HTTP path against the fake plugs.
struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    guard: Arc<ChargeGuard>,
    plugs: Arc<Plugs>,
    server: FakeHttp,
}

impl Fixture {
    /// `gate_off`: a `turn_off` answer waits until [`Plugs::release_off`].
    async fn new(gate_off: bool) -> Fixture {
        let plugs = Arc::new(Plugs {
            power: Mutex::new(HashMap::from([
                ("switch.a".to_string(), true),
                ("switch.b".to_string(), true),
            ])),
            calls: Default::default(),
            authorizations: Default::default(),
            fail_on: AtomicI32::new(0),
            fail_off_response: AtomicBool::new(false),
            required_authorization: Default::default(),
            off_entered: Signal::new(),
            release_off: Signal::new(),
        });
        if !gate_off {
            plugs.release_off.set();
        }
        let server = {
            let plugs = plugs.clone();
            FakeHttp::start(move |req| {
                let plugs = plugs.clone();
                async move {
                    let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                    let id = body["entity_id"].as_str().unwrap_or("").to_string();
                    let on = req.path.ends_with("turn_on");
                    lock(&plugs.calls).push(format!("{}:{id}", if on { "on" } else { "off" }));
                    let authorization = req.header("authorization").unwrap_or("").to_string();
                    lock(&plugs.authorizations).push(authorization.clone());
                    if let Some(required) = lock(&plugs.required_authorization).clone()
                        && authorization != required
                    {
                        return Reply::json(401, "{}");
                    }
                    let fail = on
                        && plugs
                            .fail_on
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                (n > 0).then_some(n - 1)
                            })
                            .is_ok();
                    if !fail {
                        lock(&plugs.power).insert(id, on);
                    }
                    if !on {
                        plugs.off_entered.set();
                        let _ =
                            tokio::time::timeout(Duration::from_secs(8), plugs.release_off.wait())
                                .await;
                    }
                    let failed = fail || (!on && plugs.fail_off_response.load(Ordering::SeqCst));
                    Reply::json(if failed { 500 } else { 200 }, "[]")
                }
            })
            .await
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ConfigStore::new(dir.path().join("config.json")));
        let push = PushStore::new(&dir.path().join("data.json"));
        let guard = Arc::new(ChargeGuard::new(
            store.clone(),
            Arc::new(HomeAssistant::new(store.clone())),
            push,
            None,
            Arc::new(|| None), // no battery report reaches the guard in these tests
        ));
        Fixture {
            _dir: dir,
            store,
            guard,
            plugs,
            server,
        }
    }

    fn endpoint(&self) -> String {
        self.server.url.clone()
    }

    fn configure(&self, entity: &str, enabled: bool) {
        self.configure_at(entity, enabled, &self.endpoint(), "fixture-only");
    }

    fn configure_at(&self, entity: &str, enabled: bool, endpoint: &str, token: &str) {
        self.store
            .save(
                &json!({
                    "homeAssistant": { "url": endpoint, "token": token },
                    "charge": { "enabled": enabled, "method": "ha", "entity": entity },
                })
                .to_string(),
                None,
            )
            .unwrap();
    }

    fn settings(&self) -> ChargeConfig {
        self.store.get().charge.clone()
    }

    /// `Guard.Switch(Settings, action)` started now: the settings and the server are taken before it waits.
    fn switch(&self, action: &'static str) -> tokio::task::JoinHandle<Option<String>> {
        let (guard, settings) = (self.guard.clone(), self.settings());
        let url = self.store.get().home_assistant.url.clone();
        tokio::spawn(async move { guard.switch_from(&settings, action, url).await })
    }

    async fn switch_now(&self, action: &'static str) -> Option<String> {
        within(self.switch(action)).await.unwrap()
    }

    async fn check(&self) {
        within(self.guard.check()).await;
    }

    fn power(&self, id: &str) -> bool {
        lock(&self.plugs.power).get(id).copied().unwrap_or(true)
    }

    fn calls(&self) -> Vec<String> {
        lock(&self.plugs.calls).clone()
    }

    fn authorizations(&self) -> Vec<String> {
        lock(&self.plugs.authorizations).clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.plugs.release_off.set();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_after_a_settings_change_restores_the_original_plug() {
    let f = Fixture::new(true).await;
    f.configure("switch.a", true);
    let off = f.switch("off");
    within(f.plugs.off_entered.wait()).await;
    f.configure("switch.b", false);
    f.plugs.release_off.set();
    assert_eq!(within(off).await.unwrap(), None);
    f.check().await;
    assert!(f.power("switch.a"));
    assert_eq!(f.calls(), ["off:switch.a", "on:switch.a"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_uses_the_original_server_and_credential() {
    let origin = Fixture::new(false).await;
    let replacement = Fixture::new(false).await;
    origin.configure("switch.a", true);
    assert_eq!(origin.switch_now("off").await, None);
    origin.configure_at(
        "switch.a",
        false,
        &replacement.endpoint(),
        "replacement-only",
    );
    origin.check().await;
    assert!(origin.power("switch.a"));
    assert!(replacement.calls().is_empty());
    assert_eq!(
        origin.authorizations(),
        ["Bearer fixture-only", "Bearer fixture-only"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_restore_stays_pending_and_blocks_another_off_target() {
    let f = Fixture::new(false).await;
    f.configure("switch.a", true);
    assert_eq!(f.switch_now("off").await, None);
    f.configure("switch.b", true);
    f.plugs.fail_on.store(1, Ordering::SeqCst);
    assert!(f.switch_now("off").await.is_some());
    assert!(!f.power("switch.a"));
    assert!(f.power("switch.b"));
    assert_eq!(f.switch_now("off").await, None);
    assert!(f.power("switch.a"));
    assert!(!f.power("switch.b"));
    f.configure("switch.b", false);
    f.check().await;
    assert!(f.power("switch.b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_local_address_does_not_invent_an_off_target() {
    let f = Fixture::new(false).await;
    f.configure_at("switch.a", true, "invalid", "fixture-only");
    assert!(f.switch_now("off").await.is_some());
    assert!(f.calls().is_empty());
    f.configure("switch.b", true);
    assert_eq!(f.switch_now("off").await, None);
    assert_eq!(f.calls(), ["off:switch.b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ambiguous_off_response_still_requires_recovery() {
    let f = Fixture::new(false).await;
    f.plugs.fail_off_response.store(true, Ordering::SeqCst);
    f.configure("switch.a", true);
    assert!(f.switch_now("off").await.is_some());
    f.configure("switch.b", false);
    f.check().await;
    assert!(f.power("switch.a"));
    assert_eq!(f.calls(), ["off:switch.a", "on:switch.a"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_check_recovers_the_off_whose_response_completes_after_replacement() {
    let f = Fixture::new(true).await;
    f.configure("switch.a", true);
    let off = f.switch("off");
    within(f.plugs.off_entered.wait()).await;
    f.configure("switch.b", false);
    let check = {
        let guard = f.guard.clone();
        tokio::spawn(async move { guard.check().await })
    };
    f.plugs.release_off.set();
    assert_eq!(within(off).await.unwrap(), None);
    within(check).await.unwrap();
    assert!(f.power("switch.a"));
    assert_eq!(f.calls(), ["off:switch.a", "on:switch.a"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_off_targets_recover_the_old_supply_before_taking_another() {
    let f = Fixture::new(true).await;
    f.configure("switch.a", true);
    let first = f.switch("off");
    within(f.plugs.off_entered.wait()).await;
    f.configure("switch.b", true);
    let second = f.switch("off");
    f.plugs.release_off.set();
    assert_eq!(within(first).await.unwrap(), None);
    assert_eq!(within(second).await.unwrap(), None);
    assert!(f.power("switch.a"));
    assert!(!f.power("switch.b"));
    assert_eq!(f.calls(), ["off:switch.a", "on:switch.a", "off:switch.b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn rotated_credentials_can_restore_the_same_supply() {
    let f = Fixture::new(false).await;
    f.configure("switch.a", true);
    assert_eq!(f.switch_now("off").await, None);
    f.configure_at("switch.a", false, &f.endpoint(), "rotated-fixture-only");
    f.check().await;
    assert!(f.power("switch.a"));
    assert_eq!(
        f.authorizations().last().map(String::as_str),
        Some("Bearer rotated-fixture-only")
    );
    let before = f.calls().len();
    f.check().await;
    assert_eq!(f.calls().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_connection_token_does_not_leave_the_owned_supply_off() {
    for (enabled, manual) in [(false, false), (true, false), (true, true)] {
        let f = Fixture::new(false).await;
        *lock(&f.plugs.required_authorization) = Some("Bearer fixture-only".into());
        f.configure("switch.a", true);
        assert_eq!(f.switch_now("off").await, None);
        assert!(!f.power("switch.a"));

        f.configure_at("switch.a", enabled, &f.endpoint(), "");
        if manual {
            assert_eq!(f.switch_now("on").await, None);
        } else {
            f.check().await;
        }

        assert_eq!(
            f.calls(),
            ["off:switch.a", "on:switch.a"],
            "{enabled} {manual}"
        );
        assert!(f.power("switch.a"));
        assert_eq!(
            f.authorizations(),
            ["Bearer fixture-only", "Bearer fixture-only"]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_does_not_carry_the_old_supply_state_into_the_replacement() {
    let f = Fixture::new(false).await;
    f.configure("switch.a", true);
    assert_eq!(f.switch_now("off").await, None);
    f.configure("switch.b", false);
    f.check().await;
    f.configure("switch.b", true);
    let status = f.guard.status();
    const NOW: i64 = 1_000_000_000;
    let low = PhoneBatteryReport {
        level: 10,
        plugged: false,
        temp: None,
        at: NOW,
        source: None,
        usb: None,
    };
    assert_eq!(
        decide(
            &f.settings(),
            Some(&low),
            NOW,
            status["power"].as_str(),
            NOW
        ),
        Some("on")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_test_does_not_switch_a_supply_the_settings_replaced() {
    let f = Fixture::new(true).await;
    f.configure("switch.a", true);
    let first = f.switch("off");
    within(f.plugs.off_entered.wait()).await;
    let queued = f.switch("off"); // asked for switch.a, waits behind the first switch
    f.configure("switch.b", true);
    f.plugs.release_off.set();
    assert_eq!(within(first).await.unwrap(), None);
    assert!(within(queued).await.unwrap().is_some());
    assert_eq!(f.calls(), ["off:switch.a"]);
    assert!(f.power("switch.b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_test_on_for_another_supply_does_not_hide_the_tracked_off_supply() {
    // No battery report reaches the guard in this test, so only the "lost the phone" rule can switch power back on.
    let f = Fixture::new(false).await;
    f.configure("switch.a", true);
    assert_eq!(f.switch_now("off").await, None);
    f.configure("switch.b", true);
    assert_eq!(f.switch_now("on").await, None);
    f.configure("switch.a", true);
    f.check().await;
    assert!(f.power("switch.a"));
    assert_eq!(f.calls(), ["off:switch.a", "on:switch.b", "on:switch.a"]);
}

/// `MonitorAuditTests.ChargeGuardKeepsTryingToRestoreThePowerAfterAFailedSwitchOn`.
#[tokio::test(flavor = "multi_thread")]
async fn charge_guard_keeps_trying_to_restore_the_power_after_a_failed_switch_on() {
    let reachable = Arc::new(AtomicBool::new(true));
    let server = {
        let reachable = reachable.clone();
        FakeHttp::start(move |_req| {
            let reachable = reachable.clone();
            async move {
                Reply::json(
                    if reachable.load(Ordering::SeqCst) {
                        200
                    } else {
                        503
                    },
                    "",
                )
            }
        })
        .await
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ConfigStore::new(dir.path().join("config.json")));
    store
        .save(
            &json!({
                "homeAssistant": { "url": server.url, "token": "secret" },
                "charge": { "enabled": true, "min": 50, "max": 80, "method": "ha", "entity": "switch.phone_plug" },
            })
            .to_string(),
            None,
        )
        .unwrap();
    let guard = ChargeGuard::new(
        store.clone(),
        Arc::new(HomeAssistant::new(store.clone())),
        PushStore::new(&dir.path().join("data.json")),
        None,
        Arc::new(|| None),
    );
    let charge = store.get().charge.clone();
    assert_eq!(guard.switch(&charge, "off").await, None); // 80 % reached: the plug is switched off
    reachable.store(false, Ordering::SeqCst); // Home Assistant restarts / the network drops
    assert!(guard.switch(&charge, "on").await.is_some()); // the phone went silent: "on" is attempted and fails
    drop(server);

    let status = guard.status();
    let power = status["power"].as_str().map(str::to_string);
    let switched_at = status["switchedAt"].as_i64().unwrap();
    // the plug is still off, so the guard has to keep asking for "on" (phone silent: no report)
    assert_eq!(
        decide(
            &charge,
            None,
            switched_at + 30_000,
            power.as_deref(),
            switched_at
        ),
        Some("on")
    );
}
