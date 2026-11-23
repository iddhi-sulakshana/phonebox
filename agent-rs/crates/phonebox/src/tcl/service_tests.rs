//! The TCL service against the fake cloud: what it reads and when, what it keeps on errors, and which actions reach
//! which units.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::*;
use crate::config::store::ConfigStore;
use crate::tcl::fake::{ACCOUNT, Cloud, PASSWORD};

fn store(dir: &tempfile::TempDir, config: Value) -> Arc<ConfigStore> {
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    Arc::new(ConfigStore::new(path))
}

fn config(control: Option<bool>) -> Value {
    let options = control.map_or(json!({}), |c| json!({ "control": c }));
    json!({
        "tcl": { "username": ACCOUNT, "password": PASSWORD },
        "pages": [{ "id": "ac", "enabled": true, "widgets": [{ "id": "t1", "type": "tcl", "options": options }] }],
    })
}

fn service_for(cloud: &Cloud, cfg: Arc<ConfigStore>) -> Tcl {
    let clock = cloud.clock.clone();
    Tcl::with_endpoints(
        cfg,
        cloud.endpoints(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
}

async fn service(control: Option<bool>) -> (tempfile::TempDir, Cloud, Tcl) {
    let cloud = Cloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let tcl = service_for(&cloud, store(&dir, config(control)));
    (dir, cloud, tcl)
}

fn shadow_reads(cloud: &Cloud) -> Vec<String> {
    cloud
        .iot()
        .into_iter()
        .filter(|q| q.method == "GET")
        .map(|q| q.path)
        .collect()
}

fn publishes(cloud: &Cloud) -> Vec<(String, Value)> {
    cloud
        .iot()
        .into_iter()
        .filter(|q| q.method == "POST")
        .map(|q| {
            let body: Value = serde_json::from_str(&q.body).unwrap();
            (q.path, body["state"]["desired"].clone())
        })
        .collect()
}

#[tokio::test]
async fn nothing_is_sent_without_settings_or_a_widget() {
    let cloud = Cloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(&dir, json!({}));
    let tcl = service_for(&cloud, cfg.clone());
    let v = tcl.view().await;
    assert!(!v.configured);
    assert_eq!(v.error.as_deref(), Some(NOT_CONFIGURED));
    cfg.save(
        &json!({ "tcl": { "username": ACCOUNT, "password": PASSWORD } }).to_string(),
        None,
    )
    .unwrap();
    let v = tcl.view().await;
    assert!(v.configured);
    assert_eq!(v.error.as_deref(), Some(NO_WIDGET));
    assert!(v.units.is_empty());
    assert!(cloud.seen().is_empty());
}

#[tokio::test]
async fn the_view_lists_units_with_their_state() {
    let (_d, cloud, tcl) = service(Some(true)).await;
    let v = tcl.view().await;
    assert!(v.configured && v.control);
    assert_eq!(v.error, None);
    assert_eq!(
        v.units,
        [
            TclUnit {
                id: "ac1".into(),
                name: "Shop front".into(),
                online: true,
                power: Some(true),
                mode: Some(1),
                fan: Some(2),
                generator: Some(0),
                target: Some(26.0),
                inside: Some(29.5),
                outside: Some(34.0),
            },
            // a unit that reports only some fields
            TclUnit {
                id: "ac2".into(),
                name: "Back room".into(),
                online: true,
                power: Some(false),
                inside: Some(31.2),
                ..TclUnit::default()
            },
            TclUnit {
                id: "ac3".into(),
                name: "ac3".into(),
                ..TclUnit::default()
            },
        ]
    );
    // the offline unit's shadow is not read
    assert_eq!(
        shadow_reads(&cloud),
        ["/iot/things/ac1/shadow", "/iot/things/ac2/shadow"]
    );
    let j = serde_json::to_value(&v).unwrap();
    assert_eq!(j["units"][0]["inside"], json!(29.5));
    assert_eq!(j["units"][1]["target"], Value::Null);
    assert_eq!(j["control"], json!(true));
}

#[tokio::test]
async fn reads_share_the_list_and_the_states() {
    let (_d, cloud, tcl) = service(None).await;
    tcl.view().await;
    tcl.view().await;
    assert_eq!(cloud.count("/dev/v3/user/get_things"), 1);
    assert_eq!(shadow_reads(&cloud).len(), 2);
    tcl.expire_states().await;
    tcl.view().await;
    assert_eq!(cloud.count("/dev/v3/user/get_things"), 1);
    assert_eq!(shadow_reads(&cloud).len(), 4);
}

#[tokio::test]
async fn an_error_keeps_the_last_units() {
    let (_d, cloud, tcl) = service(None).await;
    let good = tcl.view().await;
    cloud.s.list_fails.store(true, Ordering::SeqCst);
    cloud.s.iot_403.store(100, Ordering::SeqCst);
    tcl.expire().await;
    let v = tcl.view().await;
    assert_eq!(
        v.error.as_deref(),
        Some("TCL's device list refused the session: token invalid")
    );
    assert_eq!(v.units, good.units);
}

#[tokio::test]
async fn an_unknown_or_offline_unit_is_refused() {
    let (_d, cloud, tcl) = service(Some(true)).await;
    assert_eq!(
        tcl.act("ac9", "power", "on").await,
        Err("There is no such unit".into())
    );
    assert_eq!(
        tcl.act("ac3", "power", "on").await,
        Err("ac3 is offline".into())
    );
    assert_eq!(
        tcl.act("ac1", "temp", "40").await,
        Err("The temperature must be a whole number from 16 to 31".into())
    );
    assert!(publishes(&cloud).is_empty());
}

#[tokio::test]
async fn all_units_means_every_online_unit() {
    let (_d, cloud, tcl) = service(Some(true)).await;
    assert_eq!(tcl.act("all", "power", "on").await, Ok(2));
    let mut sent = publishes(&cloud);
    sent.sort_by(|a, b| a.0.cmp(&b.0));
    let desired = json!({"powerSwitch": 1, "targetTemperature": 26, "targetCelsiusDegree": 26});
    assert_eq!(
        sent,
        [
            (
                "/iot/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate".to_string(),
                desired.clone()
            ),
            (
                "/iot/topics/%24aws%2Fthings%2Fac2%2Fshadow%2Fupdate".to_string(),
                desired
            ),
        ]
    );
    // one unit's power-on also asks for the health mode, as the dashboard's card does
    assert_eq!(tcl.act("ac1", "power", "on").await, Ok(1));
    assert_eq!(publishes(&cloud)[2].1["healthy"], json!(1));
}

#[tokio::test]
async fn actions_need_a_widget_that_allows_them() {
    for control in [None, Some(false)] {
        let (_d, cloud, tcl) = service(control).await;
        assert_eq!(
            tcl.act("all", "generator", "2").await,
            Err(NOT_ALLOWED.into())
        );
        assert!(cloud.seen().is_empty());
    }
}

#[tokio::test]
async fn after_an_action_the_states_are_read_again() {
    let (_d, cloud, tcl) = service(Some(true)).await;
    tcl.view().await;
    assert_eq!(shadow_reads(&cloud).len(), 2);
    tcl.act("ac1", "generator", "2").await.unwrap();
    tcl.view().await;
    assert_eq!(shadow_reads(&cloud).len(), 4);
}

#[tokio::test]
async fn the_settings_test_signs_in_with_the_typed_values() {
    let (_d, _cloud, tcl) = service(None).await;
    assert_eq!(tcl.test(ACCOUNT, PASSWORD).await, "Connected · 3 units");
    assert_eq!(
        tcl.test(ACCOUNT, "nope").await,
        "TCL refused the sign-in: password error"
    );
    assert_eq!(tcl.test("", "").await, "Enter the TCL account and password");
}
