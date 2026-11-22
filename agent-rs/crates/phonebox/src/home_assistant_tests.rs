//! `HomeAssistantTests`, `HomeAssistantStateTests` and the Home Assistant half of `IntegrationVisibilityTests`, against
//! a fake Home Assistant on loopback.

use super::*;
use crate::testutil::{FakeHttp, Reply};
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

/// Which entities the fake knows (the C# fixtures differ).
#[derive(Clone, Copy, PartialEq)]
enum Set {
    /// HomeAssistantTests: a lamp, a sensor, a lock and a switch no widget lists.
    Desk,
    /// HomeAssistantStateTests: a lock, a vacuum and a sensor.
    Locks,
    /// IntegrationVisibilityFixture: one lamp.
    Lamp,
}

struct Fake {
    token: String,
    set: Set,
    door: Mutex<String>,
    vacuum: Mutex<String>,
    /// "path body" of every service call.
    calls: Mutex<Vec<String>>,
    posts: AtomicUsize,
    fail_states: AtomicBool,
    abort_service_answers: AtomicBool,
    hold_states: AtomicBool,
    states_entered: Notify,
    release: Notify,
}

impl Fake {
    fn new(token: &str, set: Set) -> Arc<Fake> {
        Arc::new(Fake {
            token: token.into(),
            set,
            door: Mutex::new("locked".into()),
            vacuum: Mutex::new("docked".into()),
            calls: Mutex::new(Vec::new()),
            posts: AtomicUsize::new(0),
            fail_states: AtomicBool::new(false),
            abort_service_answers: AtomicBool::new(false),
            hold_states: AtomicBool::new(false),
            states_entered: Notify::new(),
            release: Notify::new(),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn paths(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|c| c.split(' ').next().unwrap().to_string())
            .collect()
    }

    fn set_door(&self, state: &str) {
        *self.door.lock().unwrap() = state.into();
    }

    fn states_body(&self) -> String {
        let door = self.door.lock().unwrap().clone();
        let vacuum = self.vacuum.lock().unwrap().clone();
        match self.set {
            Set::Desk => json!([
                {"entity_id":"light.desk","state":"on","attributes":{"friendly_name":"Desk lamp"},"last_changed":"2026-11-07T10:00:00+00:00"},
                {"entity_id":"sensor.temp","state":"21.43","attributes":{"friendly_name":"Office","unit_of_measurement":"°C","device_class":"temperature"}},
                {"entity_id":"lock.door","state":door,"attributes":{"friendly_name":"Door"}},
                {"entity_id":"switch.secret","state":"off","attributes":{"friendly_name":"Not in any widget"}},
            ]),
            Set::Locks => json!([
                {"entity_id":"lock.door","state":door,"attributes":{"friendly_name":"Door"}},
                {"entity_id":"vacuum.robot","state":vacuum,"attributes":{"friendly_name":"Robot"}},
                {"entity_id":"sensor.temp","state":"21.43","attributes":{"friendly_name":"Office"}},
            ]),
            Set::Lamp => json!([{"entity_id":"light.desk","state":"off","attributes":{"friendly_name":"Desk"}}]),
        }
        .to_string()
    }

    async fn start(self: &Arc<Fake>) -> FakeHttp {
        let fake = self.clone();
        FakeHttp::start(move |req| {
            let fake = fake.clone();
            async move {
                if req.method == "POST" {
                    fake.posts.fetch_add(1, Ordering::SeqCst);
                }
                if req.header("authorization") != Some(&format!("Bearer {}", fake.token)) {
                    return Reply::json(401, "{}");
                }
                if req.path == "/api/states" {
                    fake.states_entered.notify_one();
                    if fake.hold_states.load(Ordering::SeqCst) {
                        let _ =
                            tokio::time::timeout(Duration::from_secs(10), fake.release.notified())
                                .await;
                    }
                    if fake.fail_states.load(Ordering::SeqCst) {
                        return Reply::json(500, "{}");
                    }
                    return Reply::json(200, fake.states_body());
                }
                if req.path == "/api/" {
                    return Reply::json(200, r#"{"message":"API running."}"#);
                }
                fake.calls
                    .lock()
                    .unwrap()
                    .push(format!("{} {}", req.path, req.body));
                match req.path.as_str() {
                    "/api/services/lock/unlock" => fake.set_door("unlocked"),
                    "/api/services/lock/lock" => fake.set_door("locked"),
                    _ => {}
                }
                // the call was applied but the answer is a failure
                if fake.abort_service_answers.load(Ordering::SeqCst) {
                    return Reply::json(504, "{}");
                }
                Reply::json(200, "[]")
            }
        })
        .await
    }
}

fn store(dir: &tempfile::TempDir, config: Value) -> Arc<ConfigStore> {
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    Arc::new(ConfigStore::new(path))
}

/// HomeAssistantTests' client: one `ha` widget on page "home".
fn client(
    dir: &tempfile::TempDir,
    url: &str,
    control: bool,
    items: Option<Value>,
    page_enabled: bool,
) -> HomeAssistant {
    let options = match items {
        None => json!({"entities": "light.desk, sensor.temp, lock.door", "control": control}),
        Some(items) => json!({"entities": "switch.secret", "control": control, "items": items}),
    };
    HomeAssistant::new(store(
        dir,
        json!({
            "homeAssistant": {"url": url, "token": "secret"},
            "pages": [
                {"id": "home", "title": "Home", "enabled": page_enabled,
                 "widgets": [{"id": "ha", "type": "ha", "x": 0, "y": 0, "w": 6, "h": 3, "options": options}]},
                {"id": "other", "title": "Other", "enabled": true, "widgets": []},
            ],
        }),
    ))
}

fn ids(r: &HaStates) -> Vec<&str> {
    r.entities.iter().map(|e| e.id.as_str()).collect()
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

async fn desk() -> (tempfile::TempDir, Arc<Fake>, FakeHttp) {
    let fake = Fake::new("secret", Set::Desk);
    let server = fake.start().await;
    (tempfile::tempdir().unwrap(), fake, server)
}

// ---------------------------------------------------------------- HomeAssistantTests

#[tokio::test]
async fn states_are_filtered_to_widget_entities() {
    let (d, _fake, server) = desk().await;
    let ha = client(&d, &server.url, false, None, true);
    let r = ha
        .states(&strings(&["light.desk", "sensor.temp", "switch.secret"]))
        .await;
    assert!(r.configured);
    assert_eq!(ids(&r), ["light.desk", "sensor.temp"]); // switch.secret is not in any widget
    let temp = &r.entities[1];
    assert_eq!(temp.state, "21.43");
    assert_eq!(temp.unit.as_deref(), Some("°C"));
    assert_eq!(temp.device_class.as_deref(), Some("temperature"));
    assert_eq!(r.entities[0].name, "Desk lamp");
    assert_eq!(r.entities[0].changed, 1_794_045_600_000);
}

#[tokio::test]
async fn switching_needs_the_widgets_permission() {
    let (d, fake, server) = desk().await;
    let ha = client(&d, &server.url, false, None, true);
    assert!(ha.toggle("light.desk").await.is_some());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn switching_calls_the_right_service() {
    let (d, fake, server) = desk().await;
    let ha = client(&d, &server.url, true, None, true);
    ha.states(&strings(&["lock.door"])).await; // the lock's current state decides lock / unlock
    assert_eq!(ha.toggle("light.desk").await, None);
    assert_eq!(ha.toggle("lock.door").await, None);
    assert!(ha.toggle("sensor.temp").await.is_some()); // sensors cannot be switched
    assert!(ha.toggle("switch.secret").await.is_some()); // not in a widget
    assert_eq!(
        fake.calls(),
        [
            r#"/api/services/homeassistant/toggle {"entity_id":"light.desk"}"#,
            r#"/api/services/lock/unlock {"entity_id":"lock.door"}"#,
        ]
    );
    assert_eq!(
        ha.toggle("sensor.temp").await.as_deref(),
        Some("sensor entities cannot be switched")
    );
}

#[tokio::test]
async fn items_replace_the_legacy_entity_list() {
    let (d, _fake, server) = desk().await;
    let items = json!([{"id": "light.desk"}, {"id": "light.desk"}, {"id": "nodot"}, {"name": "no id"}, "junk"]);
    let ha = client(&d, &server.url, true, Some(items), true);
    let r = ha.states(&strings(&["light.desk", "switch.secret"])).await;
    assert_eq!(ids(&r), ["light.desk"]); // switch.secret is only in the ignored legacy string
}

#[tokio::test]
async fn a_non_empty_items_list_is_final_even_when_no_row_is_usable() {
    let (d, fake, server) = desk().await;
    let ha = client(
        &d,
        &server.url,
        true,
        Some(json!([{"id": "nodot"}, {"name": "no id"}])),
        true,
    );
    let r = ha.states(&strings(&["switch.secret"])).await; // listed only in the legacy string
    assert!(r.entities.is_empty());
    assert!(ha.toggle("switch.secret").await.is_some());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn the_cap_counts_usable_rows_not_all_rows() {
    let (d, _fake, server) = desk().await;
    // 300 unusable rows in front must not hide the valid one after them
    let mut rows: Vec<Value> = (0..300)
        .map(|i| json!({"id": format!("nodot{i}")}))
        .collect();
    rows.push(json!({"id": "light.desk"}));
    let ha = client(&d, &server.url, true, Some(Value::Array(rows)), true);
    let r = ha.states(&strings(&["light.desk"])).await;
    assert_eq!(ids(&r), ["light.desk"]);
}

#[test]
fn at_most_two_hundred_usable_rows_are_read() {
    let items: Vec<Value> = (0..250)
        .map(|i| json!({"id": format!("sensor.s{i}")}))
        .collect();
    let w: WidgetConfig =
        serde_json::from_value(json!({"type": "ha", "options": {"items": items}})).unwrap();
    assert_eq!(items_of(&w).len(), 200);
}

#[tokio::test]
async fn states_of_a_hidden_pages_widget_are_not_read() {
    let (d, _fake, server) = desk().await;
    let ha = client(
        &d,
        &server.url,
        true,
        Some(json!([{"id": "light.desk"}])),
        false,
    );
    assert!(
        ha.states(&strings(&["light.desk"]))
            .await
            .entities
            .is_empty()
    );
}

#[tokio::test]
async fn an_empty_items_list_falls_back_to_the_legacy_entity_string() {
    let (d, _fake, server) = desk().await;
    let ha = client(&d, &server.url, true, Some(json!([])), true);
    let r = ha.states(&strings(&["switch.secret", "light.desk"])).await;
    assert_eq!(ids(&r), ["switch.secret"]);
}

#[tokio::test]
async fn each_item_has_its_own_switching_permission() {
    let (d, fake, server) = desk().await;
    // light.desk inherits the widget's flag (on), lock.door opts out
    let items = json!([{"id": "light.desk"}, {"id": "lock.door", "control": false}, {"id": "scene.movie", "control": true}]);
    let ha = client(&d, &server.url, true, Some(items), true);
    ha.states(&strings(&["lock.door"])).await;
    assert_eq!(ha.toggle("light.desk").await, None);
    assert!(ha.toggle("lock.door").await.is_some());
    assert_eq!(
        fake.calls(),
        [r#"/api/services/homeassistant/toggle {"entity_id":"light.desk"}"#]
    );
}

#[tokio::test]
async fn an_item_can_allow_switching_when_the_widget_does_not() {
    let (d, _fake, server) = desk().await;
    let items = json!([{"id": "light.desk", "control": true}, {"id": "lock.door"}]);
    let ha = client(&d, &server.url, false, Some(items), true);
    ha.states(&strings(&["lock.door"])).await;
    assert_eq!(ha.toggle("light.desk").await, None);
    assert!(ha.toggle("lock.door").await.is_some()); // no flag of its own: the widget's (off) applies
}

#[tokio::test]
async fn switching_stops_when_the_page_is_hidden() {
    let (d, fake, server) = desk().await;
    let ha = client(
        &d,
        &server.url,
        true,
        Some(json!([{"id": "light.desk", "control": true}])),
        false,
    );
    assert!(ha.toggle("light.desk").await.is_some());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn the_entity_picker_lists_everything_home_assistant_knows() {
    let (d, _fake, server) = desk().await;
    let ha = client(&d, &server.url, false, None, true);
    let r = ha.list_entities().await;
    let all: Vec<&str> = r.entities.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        all,
        ["light.desk", "lock.door", "sensor.temp", "switch.secret"]
    );
    assert!(r.entities[0].switchable);
    assert!(!r.entities[2].switchable);
    assert_eq!(r.entities[0].name, "Desk lamp");
    assert_eq!(
        serde_json::to_value(&r.entities[0]).unwrap(),
        json!({"id": "light.desk", "name": "Desk lamp", "switchable": true})
    );
}

#[tokio::test]
async fn the_entity_picker_needs_a_configured_home_assistant() {
    let d = tempfile::tempdir().unwrap();
    let ha = HomeAssistant::new(store(&d, json!({})));
    let r = ha.list_entities().await;
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        json!({"configured": false, "error": NOT_CONFIGURED, "entities": []})
    );
}

#[tokio::test]
async fn test_reports_a_bad_token() {
    let (d, _fake, server) = desk().await;
    let ha = HomeAssistant::new(store(&d, json!({})));
    assert_eq!(ha.test(&server.url, "secret").await, "Connected");
    assert_eq!(
        ha.test(&server.url, "wrong").await,
        "The token was rejected"
    );
}

#[tokio::test]
async fn toggle_handles_unconfigured_settings_cold_lock_cache_and_widget_entity_edits() {
    let (d, fake, server) = desk().await;
    let uncfg = HomeAssistant::new(store(
        &d,
        json!({"pages":[{"id":"home","title":"Home","widgets":[{"id":"ha","type":"ha","x":0,"y":0,"w":6,"h":3,"options":{"entities":"light.desk","control":true}}]}]}),
    ));
    assert_eq!(
        uncfg.toggle("light.desk").await.as_deref(),
        Some(NOT_CONFIGURED)
    );

    fake.set_door("locked");
    let ha = client(&d, &server.url, true, None, true);
    assert_eq!(ha.toggle("lock.door").await, None); // a cold cache: the state is read first
    assert_eq!(ha.toggle("lock.door").await, None);
    assert_eq!(
        fake.calls(),
        [
            r#"/api/services/lock/unlock {"entity_id":"lock.door"}"#,
            r#"/api/services/lock/lock {"entity_id":"lock.door"}"#,
        ]
    );

    let config = |entities: &str| {
        json!({
            "homeAssistant": {"url": server.url, "token": "secret"},
            "pages": [{"id": "home", "title": "Home", "widgets": [{"id": "ha", "type": "ha", "x": 0, "y": 0, "w": 6, "h": 3,
                "options": {"entities": entities, "control": true}}]}],
        })
    };
    let cfg = store(&d, config("light.desk"));
    let cached = HomeAssistant::new(cfg.clone());
    cached.states(&strings(&["light.desk"])).await;
    cfg.save(&config("light.desk, sensor.temp").to_string(), None)
        .unwrap();
    // a widget that lists a new entity reads again at once (the cache is keyed by the widgets' entities)
    let updated = cached
        .states(&strings(&["light.desk", "sensor.temp"]))
        .await;
    assert_eq!(updated.entities[1].state, "21.43");
}

// ---------------------------------------------------------------- HomeAssistantStateTests

struct Two {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    a: Arc<Fake>,
    b: Arc<Fake>,
    a_server: FakeHttp,
    b_server: FakeHttp,
}

impl Two {
    async fn new() -> Two {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, json!({}));
        let (a, b) = (
            Fake::new("token-a", Set::Locks),
            Fake::new("token-b", Set::Locks),
        );
        let (a_server, b_server) = (a.start().await, b.start().await);
        Two {
            _dir: dir,
            store,
            a,
            b,
            a_server,
            b_server,
        }
    }

    fn configure(&self, b: bool, control: bool, token: Option<&str>) {
        let (server, fake) = if b {
            (&self.b_server, &self.b)
        } else {
            (&self.a_server, &self.a)
        };
        self.store
            .save(
                &json!({
                    "homeAssistant": {"url": server.url, "token": token.unwrap_or(&fake.token)},
                    "pages": [{"id": "home", "title": "Home", "widgets": [{"id": "ha", "type": "ha", "x": 0, "y": 0, "w": 6, "h": 3,
                        "options": {"entities": "lock.door, vacuum.robot, sensor.temp", "control": control}}]}],
                })
                .to_string(),
                None,
            )
            .unwrap();
    }

    fn client(&self) -> HomeAssistant {
        HomeAssistant::new(self.store.clone())
    }
}

#[tokio::test]
async fn a_vacuum_is_not_started_when_its_state_cannot_be_read() {
    let t = Two::new().await;
    t.a.fail_states.store(true, Ordering::SeqCst);
    *t.a.vacuum.lock().unwrap() = "cleaning".into();
    t.configure(false, true, None);
    assert!(t.client().toggle("vacuum.robot").await.is_some());
    assert!(t.a.calls().is_empty());
}

#[tokio::test]
async fn hiding_the_widget_during_the_state_read_cancels_the_call() {
    let t = Arc::new(Two::new().await);
    t.configure(false, true, None);
    t.a.hold_states.store(true, Ordering::SeqCst);
    let ha = Arc::new(t.client());
    let toggle = tokio::spawn({
        let ha = ha.clone();
        async move { ha.toggle("lock.door").await }
    });
    tokio::time::timeout(Duration::from_secs(5), t.a.states_entered.notified())
        .await
        .unwrap();
    t.configure(false, false, None);
    t.a.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(15), toggle)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_some());
    assert!(t.a.calls().is_empty());
}

#[tokio::test]
async fn a_lost_answer_still_makes_the_next_toggle_read_the_state() {
    let t = Two::new().await;
    t.configure(false, true, None);
    let ha = t.client();
    t.a.abort_service_answers.store(true, Ordering::SeqCst);
    assert!(ha.toggle("lock.door").await.is_some()); // applied, answer lost
    t.a.abort_service_answers.store(false, Ordering::SeqCst);
    assert_eq!(ha.toggle("lock.door").await, None);
    assert_eq!(
        t.a.paths(),
        ["/api/services/lock/unlock", "/api/services/lock/lock"]
    );
}

#[tokio::test]
async fn a_lock_toggle_reads_the_state_again_instead_of_the_widget_cache() {
    let t = Two::new().await;
    t.configure(false, true, None);
    let ha = t.client();
    ha.states(&strings(&["lock.door"])).await; // the widget poll caches "locked"
    t.a.set_door("unlocked"); // unlocked by hand right after
    assert_eq!(ha.toggle("lock.door").await, None);
    assert_eq!(t.a.paths(), ["/api/services/lock/lock"]);
}

#[tokio::test]
async fn a_lock_toggle_with_an_unknown_state_calls_nothing() {
    let t = Two::new().await;
    t.configure(false, true, None);
    t.a.set_door("unknown");
    assert_eq!(
        t.client().toggle("lock.door").await.as_deref(),
        Some(NO_STATE)
    );
    assert!(t.a.calls().is_empty());
}

#[tokio::test]
async fn a_lock_toggle_uses_the_state_of_the_server_it_calls() {
    let t = Two::new().await;
    t.b.set_door("unlocked");
    t.configure(false, true, None);
    let ha = t.client();
    ha.states(&strings(&["lock.door"])).await; // caches the first server's "locked"
    t.configure(true, true, None);
    assert_eq!(ha.toggle("lock.door").await, None);
    assert!(t.a.calls().is_empty());
    assert_eq!(t.b.paths(), ["/api/services/lock/lock"]);
}

#[tokio::test]
async fn a_failed_read_for_new_settings_shows_no_state_from_the_old_server() {
    let t = Two::new().await;
    t.configure(false, true, None);
    let ha = t.client();
    let before = ha.states(&strings(&["sensor.temp"])).await;
    assert_eq!(before.entities[0].state, "21.43");
    t.configure(true, true, Some("rejected")); // the new server refuses this token
    let after = ha.states(&strings(&["sensor.temp"])).await;
    assert!(after.error.is_some());
    assert_eq!(after.entities[0].state, "unknown");
    assert_eq!(
        serde_json::to_value(&after.entities[0]).unwrap(),
        json!({"id":"sensor.temp","state":"unknown","name":"sensor.temp","unit":null,"deviceClass":null,"icon":null,"changed":0})
    );
}

// ---------------------------------------------------------------- IntegrationVisibilityTests (Home Assistant half)

struct Visibility {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    fake: Arc<Fake>,
    server: FakeHttp,
    ha: HomeAssistant,
    docker: crate::docker_monitor::DockerMonitor,
}

impl Visibility {
    async fn new() -> Visibility {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, json!({}));
        let fake = Fake::new("fixture-only", Set::Lamp);
        let server = fake.start().await;
        Visibility {
            ha: HomeAssistant::new(store.clone()),
            docker: crate::docker_monitor::DockerMonitor::new(store.clone()),
            _dir: dir,
            store,
            fake,
            server,
        }
    }

    fn configure(&self, enabled: bool, control: bool, stack: bool, child_enabled: bool) {
        let mut widgets = json!([
            {"type": "docker", "enabled": child_enabled, "options": {"control": control}},
            {"type": "ha", "enabled": child_enabled, "options": {"entities": "light.desk", "control": control}},
        ]);
        if stack {
            widgets = json!([{"type": "stack", "options": {"widgets": widgets}}]);
        }
        self.store
            .save(
                &json!({
                    "homeAssistant": {"url": self.server.url, "token": "fixture-only"},
                    "pages": [
                        {"id": "home", "enabled": true, "widgets": [{"type": "clock"}]},
                        {"id": "integrations", "enabled": enabled, "widgets": widgets},
                    ],
                })
                .to_string(),
                None,
            )
            .unwrap();
    }

    fn mutations(&self) -> usize {
        self.fake.posts.load(Ordering::SeqCst)
    }

    async fn state_ids(&self) -> Vec<String> {
        self.ha
            .states(&strings(&["light.desk"]))
            .await
            .entities
            .into_iter()
            .map(|e| e.id)
            .collect()
    }
}

#[tokio::test]
async fn hidden_pages_grant_neither_docker_nor_home_assistant_control() {
    for stack in [false, true] {
        let f = Visibility::new().await;
        f.configure(false, true, stack, true);
        assert!(!f.docker.control_allowed());
        assert!(f.ha.toggle("light.desk").await.is_some());
        assert_eq!(f.mutations(), 0);
        assert!(f.state_ids().await.is_empty(), "stack {stack}");
    }
}

#[tokio::test]
async fn hiding_after_success_revokes_both_control_permissions() {
    for stack in [false, true] {
        let f = Visibility::new().await;
        f.configure(true, false, stack, true);
        assert!(!f.docker.control_allowed());
        assert!(f.ha.toggle("light.desk").await.is_some());
        assert_eq!(f.mutations(), 0);
        f.configure(true, true, stack, true);
        assert!(f.docker.control_allowed());
        assert_eq!(f.ha.toggle("light.desk").await, None);
        assert_eq!(f.mutations(), 1);
        f.configure(false, true, stack, true);
        assert!(!f.docker.control_allowed());
        assert!(f.ha.toggle("light.desk").await.is_some());
        assert_eq!(f.mutations(), 1, "stack {stack}");
    }
}

#[tokio::test]
async fn disabled_stack_children_grant_no_control_or_entity_access() {
    let f = Visibility::new().await;
    f.configure(true, true, true, false);
    assert!(!f.docker.control_allowed());
    assert!(f.ha.toggle("light.desk").await.is_some());
    assert_eq!(f.mutations(), 0);
    assert!(f.state_ids().await.is_empty());
}

// ---------------------------------------------------------------- SwitchAsync (the charge guard's switch)

#[tokio::test]
async fn the_charge_guards_switch_needs_no_widget_and_says_whether_it_was_sent() {
    let (d, fake, server) = desk().await;
    let ha = HomeAssistant::new(store(&d, json!({})));
    let settings = |url: &str, token: Option<&str>| HomeAssistantConfig {
        url: Some(url.to_string()),
        token: token.map(String::from),
        ..Default::default()
    };
    assert_eq!(
        ha.switch("switch.plug", true, &settings(&server.url, Some("secret")))
            .await,
        (None, true)
    );
    assert_eq!(
        ha.switch("switch.plug", false, &settings(&server.url, Some("secret")))
            .await,
        (None, true)
    );
    assert_eq!(
        fake.calls(),
        [
            r#"/api/services/homeassistant/turn_on {"entity_id":"switch.plug"}"#,
            r#"/api/services/homeassistant/turn_off {"entity_id":"switch.plug"}"#,
        ]
    );
    assert_eq!(
        ha.switch("switch.plug", true, &settings(&server.url, Some("wrong")))
            .await,
        (Some("Home Assistant answered 401".into()), true)
    );
    assert_eq!(
        ha.switch("plug", true, &settings(&server.url, Some("secret")))
            .await,
        (Some("not an entity id".into()), false)
    );
    assert_eq!(
        ha.switch("switch.plug", true, &settings("ftp://x", Some("secret")))
            .await,
        (
            Some("Home Assistant address must be an absolute HTTP or HTTPS URL".into()),
            false
        )
    );
}
