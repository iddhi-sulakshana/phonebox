//! The Growatt service: what it reads and when, what it keeps on errors, new settings, and which actions it allows.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::*;
use crate::config::store::ConfigStore;
use crate::growatt::fake::Server;

fn store(dir: &tempfile::TempDir, config: Value) -> Arc<ConfigStore> {
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    Arc::new(ConfigStore::new(path))
}

/// The server at `url` with `password`, and a Growatt widget on an enabled page (with `control` when given).
fn config(url: &str, password: &str, control: Option<bool>) -> Value {
    let options = control.map_or(json!({}), |c| json!({ "control": c }));
    json!({
        "growatt": { "url": url, "password": password },
        "pages": [{ "id": "solar", "enabled": true, "widgets": [{ "id": "g1", "type": "growatt", "options": options }] }],
    })
}

async fn service(control: Option<bool>) -> (tempfile::TempDir, Server, Growatt) {
    let srv = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(&dir, config(&srv.fake.url, "sun", control));
    (dir, srv, Growatt::new(cfg))
}

#[tokio::test]
async fn nothing_is_sent_without_settings_or_a_widget() {
    let srv = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(&dir, json!({}));
    let g = Growatt::new(cfg.clone());
    let v = g.view().await;
    assert!(!v.configured);
    assert_eq!(v.error.as_deref(), Some(NOT_CONFIGURED));
    assert!(v.status.is_none() && v.totals.is_none());

    cfg.save(
        &json!({ "growatt": { "url": srv.fake.url, "password": "sun" } }).to_string(),
        None,
    )
    .unwrap();
    let v = g.view().await;
    assert!(v.configured);
    assert_eq!(v.error.as_deref(), Some(NO_WIDGET));
    assert_eq!(g.settings().await.error.as_deref(), Some(NO_WIDGET));
    assert!(srv.seen().is_empty());
}

#[tokio::test]
async fn the_view_has_the_readings_the_totals_and_the_host() {
    let (_d, srv, g) = service(Some(true)).await;
    let v = g.view().await;
    assert!(v.configured && v.control);
    assert_eq!(v.error, None);
    let port = srv.fake.url.rsplit(':').next().unwrap();
    assert_eq!(v.host, format!("127.0.0.1:{port}"));
    let s = v.status.unwrap();
    assert_eq!(
        (s.pv_w, s.battery_pct, s.source.as_str()),
        (Some(1264.0), Some(75.0), "grobro")
    );
    assert_eq!(v.totals.unwrap().solar.today, Some(5.2));
    // the phone's JSON
    let j = serde_json::to_value(g.view().await).unwrap();
    assert_eq!(j["status"]["pvW"], json!(1264.0));
    assert_eq!(j["totals"]["load"]["total"], json!(2100.7));
    assert_eq!(j["control"], json!(true));
}

#[tokio::test]
async fn reads_within_two_seconds_share_one_request() {
    let (_d, srv, g) = service(None).await;
    g.view().await;
    g.view().await;
    assert_eq!(srv.count("GET", "/api/v1/growatt/device-status"), 1);
    assert_eq!(srv.count("GET", "/api/v1/growatt/total-data"), 1);
    // past the status cache the readings are read again, the totals (5 minutes) are not
    g.expire_status().await;
    g.view().await;
    assert_eq!(srv.count("GET", "/api/v1/growatt/device-status"), 2);
    assert_eq!(srv.count("GET", "/api/v1/growatt/total-data"), 1);
}

#[tokio::test]
async fn an_error_keeps_the_last_readings() {
    let (_d, srv, g) = service(None).await;
    let good = g.view().await;
    srv.fail_status.store(true, Ordering::SeqCst);
    g.expire().await;
    let v = g.view().await;
    assert_eq!(
        v.error.as_deref(),
        Some("The Growatt server answered 500: Growatt not connected")
    );
    assert_eq!(v.status, good.status);
    assert_eq!(v.totals, good.totals);
}

#[tokio::test]
async fn new_settings_get_a_new_client() {
    let (_d, first, g) = service(None).await;
    g.view().await;
    let second = Server::start().await;
    g.cfg
        .save(&config(&second.fake.url, "sun", None).to_string(), None)
        .unwrap();
    let v = g.view().await;
    assert_eq!(v.error, None);
    // the new server's first request is its own sign-in: the first server's token never reached it
    let seen = second.seen();
    assert_eq!(seen[0].path, "/api/v1/auth/login");
    assert!(seen[0].header("authorization").is_none());
    assert_eq!(second.logins(), 1);
    assert_eq!(first.logins(), 1);
    // a changed password signs in again too
    g.cfg
        .save(&config(&second.fake.url, "nope", None).to_string(), None)
        .unwrap();
    g.expire().await;
    assert_eq!(
        g.view().await.error.as_deref(),
        Some("The Growatt server refused the username or password")
    );
}

#[tokio::test]
async fn actions_need_a_widget_that_allows_them() {
    for control in [None, Some(false)] {
        let (_d, srv, g) = service(control).await;
        assert_eq!(g.set("charge-source", "1").await, Err(NOT_ALLOWED.into()));
        assert_eq!(g.relogin().await, Err(NOT_ALLOWED.into()));
        assert!(
            srv.seen()
                .iter()
                .all(|q| q.method != "POST" || q.path == "/api/v1/auth/login")
        );
        assert_eq!(srv.count("POST", "/api/v1/growatt/relogin"), 0);
    }
}

#[tokio::test]
async fn values_outside_the_range_are_refused() {
    let (_d, srv, g) = service(Some(true)).await;
    for (name, value) in [
        ("ac-output-source", "4"),
        ("charge-source", "-1"),
        ("max-charge-current", "101"),
        ("max-charge-current", "abc"),
        ("max-charge-current", "2.5"),
        ("nope", "1"),
    ] {
        assert!(g.set(name, value).await.is_err(), "{name}={value}");
    }
    assert!(srv.seen().is_empty());
    g.set("max-charge-current", " 30 ").await.unwrap();
    let posts: Vec<_> = srv
        .reads()
        .into_iter()
        .filter(|q| q.method == "POST")
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&posts[0].body).unwrap(),
        json!({"value": 30})
    );
    assert_eq!(setting_range("ac-output-source"), Some(0..=3));
    assert_eq!(setting_range("charge-source"), Some(0..=2));
    assert_eq!(setting_range("max-charge-current"), Some(0..=100));
    assert_eq!(setting_range("anything"), None);
}

#[tokio::test]
async fn the_settings_are_read_once_and_a_write_updates_them() {
    let (_d, srv, g) = service(Some(true)).await;
    let s = g.settings().await;
    assert_eq!(s.error, None);
    assert_eq!(
        (s.ac_output_source, s.charge_source, s.max_charge_current),
        (Some(1), Some(0), Some(30))
    );
    let gets = |srv: &Server| {
        srv.reads()
            .iter()
            .filter(|q| q.method == "GET" && q.path.contains("inverter-settings"))
            .count()
    };
    assert_eq!(gets(&srv), 3);
    g.settings().await;
    assert_eq!(gets(&srv), 3);
    g.set("charge-source", "2").await.unwrap();
    assert_eq!(g.settings().await.charge_source, Some(2));
    assert_eq!(gets(&srv), 3);
    let j = serde_json::to_value(g.settings().await).unwrap();
    assert_eq!(
        j,
        json!({"error": null, "acOutputSource": 1, "chargeSource": 2, "maxChargeCurrent": 30})
    );
}

#[tokio::test]
async fn relogin_asks_the_server_and_reads_again() {
    let (_d, srv, g) = service(Some(true)).await;
    g.view().await;
    assert_eq!(g.relogin().await, Ok(true));
    assert_eq!(srv.count("POST", "/api/v1/growatt/relogin"), 1);
    g.view().await;
    assert_eq!(srv.count("GET", "/api/v1/growatt/device-status"), 2);
}

#[tokio::test]
async fn the_settings_test_signs_in_with_the_typed_values() {
    let (_d, srv, g) = service(None).await;
    assert_eq!(g.test(&srv.fake.url, "", "sun").await, "Connected");
    assert_eq!(
        g.test(&srv.fake.url, "admin", "nope").await,
        "The Growatt server refused the username or password"
    );
    assert_eq!(
        g.test("growatt.lan", "admin", "sun").await,
        "The Growatt address must be an http or https URL"
    );
}
