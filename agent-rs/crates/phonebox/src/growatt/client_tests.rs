//! The Growatt client against a fake growatt-server: sign-in, the Bearer token on the reads, a token that expires,
//! a wrong password, server errors, and readings that are not numbers.

use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::*;
use crate::growatt::fake::Server;
use crate::testutil::{FakeHttp, Reply, Req};

// ------------------------------------------------------------------------------------------------ the address

#[test]
fn the_address_gets_the_api_prefix_once() {
    assert_eq!(
        api_base("http://growatt.lan").as_deref(),
        Some("http://growatt.lan/api/v1")
    );
    assert_eq!(
        api_base("  http://growatt.lan/api/v1/ ").as_deref(),
        Some("http://growatt.lan/api/v1")
    );
    assert_eq!(
        api_base("https://g.example.com/api/v1").as_deref(),
        Some("https://g.example.com/api/v1")
    );
    assert_eq!(
        api_base("http://192.168.1.157:8082/").as_deref(),
        Some("http://192.168.1.157:8082/api/v1")
    );
    assert_eq!(
        api_base("http://host/solar").as_deref(),
        Some("http://host/solar/api/v1")
    );
}

#[test]
fn an_address_that_is_not_http_is_refused() {
    for bad in ["", "   ", "growatt.lan", "ftp://growatt.lan", "http://"] {
        assert_eq!(api_base(bad), None, "{bad:?}");
        assert!(GrowattClient::new(bad, "admin", "sun").is_err(), "{bad:?}");
    }
}

// ------------------------------------------------------------------------------------------------ sign-in and reads

#[tokio::test]
async fn it_signs_in_then_reads_the_live_status_with_the_bearer_token() {
    let srv = Server::start().await;
    let s = srv.client("sun").device_status().await.unwrap();

    let seen = srv.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/api/v1/auth/login")
    );
    assert!(
        seen[0]
            .header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[0].body).unwrap(),
        json!({"username": "admin", "password": "sun"})
    );
    assert_eq!(
        (seen[1].method.as_str(), seen[1].path.as_str()),
        ("GET", "/api/v1/growatt/device-status")
    );
    assert_eq!(seen[1].header("authorization"), Some("Bearer t1"));

    assert_eq!(s.pv_w, Some(1264.0));
    assert_eq!((s.pv1_w, s.pv2_w), (Some(604.0), Some(660.0)));
    assert_eq!((s.pv1_v, s.pv2_v), (Some(185.5), Some(198.0)));
    assert_eq!(s.battery_w, Some(-43.0)); // charging
    assert_eq!((s.battery_v, s.battery_pct), (Some(54.38), Some(75.0)));
    assert_eq!(
        (s.grid_w, s.grid_v, s.grid_hz),
        (Some(0.0), Some(232.2), Some(50.08))
    );
    assert_eq!(
        (s.load_w, s.load_va, s.load_pct),
        (Some(1270.0), Some(1389.0), Some(23.1))
    );
    assert_eq!((s.output_v, s.output_hz), (Some(230.1), Some(50.13)));
    assert_eq!(s.status, "12");
    assert_eq!(s.source, "grobro");
    assert_eq!(s.updated_at, 1_795_428_000_000);
}

#[tokio::test]
async fn the_totals_reuse_the_token() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    client.device_status().await.unwrap();
    let t = client.total_data().await.unwrap();

    assert_eq!(srv.logins(), 1);
    let reads = srv.reads();
    assert_eq!(reads[1].path, "/api/v1/growatt/total-data");
    assert_eq!(reads[1].header("authorization"), Some("Bearer t1"));

    assert_eq!(
        t.solar,
        Energy {
            today: Some(5.2),
            total: Some(1234.5)
        }
    );
    assert_eq!(
        t.discharged,
        Energy {
            today: Some(3.1),
            total: Some(800.0)
        }
    );
    assert_eq!(
        t.charged,
        Energy {
            today: Some(2.4),
            total: Some(760.2)
        }
    );
    assert_eq!(
        t.imported,
        Energy {
            today: Some(1.5),
            total: Some(300.0)
        }
    );
    assert_eq!(
        t.load,
        Energy {
            today: Some(7.9),
            total: Some(2100.7)
        }
    );
    assert_eq!(
        t.exported,
        Energy {
            today: Some(0.0),
            total: Some(0.0)
        }
    );
}

#[tokio::test]
async fn two_reads_at_once_sign_in_once() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    let (a, b) = tokio::join!(client.device_status(), client.total_data());
    a.unwrap();
    b.unwrap();
    assert_eq!(srv.logins(), 1);
}

#[tokio::test]
async fn an_expired_token_signs_in_again_once() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    client.device_status().await.unwrap();
    srv.refused.lock().unwrap().push("t1".into()); // the 30 days are over

    client.total_data().await.unwrap();
    assert_eq!(srv.logins(), 2);
    let reads = srv.reads();
    assert_eq!(reads.len(), 3);
    assert_eq!(reads[2].header("authorization"), Some("Bearer t2"));
    // and the new token stays
    client.device_status().await.unwrap();
    assert_eq!(srv.logins(), 2);
}

#[tokio::test]
async fn a_token_refused_again_after_signing_in_is_an_error_not_a_loop() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    client.device_status().await.unwrap();
    srv.refuse_all.store(true, Ordering::SeqCst);

    let err = client.total_data().await.unwrap_err();
    assert_eq!(
        err,
        "The Growatt server answered 401: Access denied: Invalid token"
    );
    assert_eq!(srv.logins(), 2);
    assert_eq!(srv.reads().len(), 3);
}

#[tokio::test]
async fn a_fresh_token_that_is_refused_is_not_retried() {
    let srv = Server::start().await;
    srv.refuse_all.store(true, Ordering::SeqCst);
    let err = srv.client("sun").device_status().await.unwrap_err();
    assert_eq!(
        err,
        "The Growatt server answered 401: Access denied: Invalid token"
    );
    assert_eq!(srv.logins(), 1);
    assert_eq!(srv.reads().len(), 1);
}

#[tokio::test]
async fn a_wrong_password_says_so_and_reads_nothing() {
    let srv = Server::start().await;
    let err = srv.client("nope").device_status().await.unwrap_err();
    assert_eq!(err, "The Growatt server refused the username or password");
    assert!(srv.reads().is_empty());
    assert!(!err.contains("nope"));
}

// ------------------------------------------------------------------------------------------------ failures

#[tokio::test]
async fn a_server_error_passes_its_message_on() {
    let fake = FakeHttp::start(|req: Req| async move {
        match req.path.as_str() {
            "/api/v1/auth/login" => Reply::json(
                200,
                r#"{"status":200,"message":"ok","data":{"token":"t1"}}"#,
            ),
            "/api/v1/growatt/device-status" => {
                Reply::json(500, r#"{"status":500,"message":"Growatt not connected"}"#)
            }
            _ => Reply {
                status: 502,
                content_type: "text/html",
                body: "<html>bad gateway</html>".into(),
            },
        }
    })
    .await;
    let client = GrowattClient::new(&fake.url, "admin", "sun").unwrap();
    assert_eq!(
        client.device_status().await.unwrap_err(),
        "The Growatt server answered 500: Growatt not connected"
    );
    assert_eq!(
        client.total_data().await.unwrap_err(),
        "The Growatt server answered 502 Bad Gateway"
    );
}

#[tokio::test]
async fn a_sign_in_reply_without_a_token_is_an_error() {
    let fake = FakeHttp::start(|_req: Req| async move {
        Reply::json(
            200,
            r#"{"status":200,"message":"Login successful","data":{"username":"admin"}}"#,
        )
    })
    .await;
    let client = GrowattClient::new(&fake.url, "admin", "sun").unwrap();
    assert_eq!(
        client.device_status().await.unwrap_err(),
        "The Growatt server's sign-in reply had no token"
    );
}

#[tokio::test]
async fn a_server_that_is_not_there_is_an_error_without_the_address_or_password() {
    let port = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let client = GrowattClient::new(&format!("http://127.0.0.1:{port}"), "admin", "sun").unwrap();
    let err = client.device_status().await.unwrap_err();
    assert!(!err.is_empty());
    assert!(!err.contains("127.0.0.1") && !err.contains("sun"), "{err}");
}

// ------------------------------------------------------------------------------------------------ the readings

#[test]
fn readings_that_are_not_numbers_are_missing() {
    let s = DeviceStatus::from_data(&json!({
        "panelPower": "", "ppv1": "abc", "ppv2": 12.5, "batPower": " -7 ", "capacity": null,
        "vBat": "NaN", "updatedAt": "yesterday"
    }));
    assert_eq!(s.pv_w, None);
    assert_eq!(s.pv1_w, None);
    assert_eq!(s.pv2_w, Some(12.5)); // a JSON number is fine too
    assert_eq!(s.battery_w, Some(-7.0));
    assert_eq!(s.battery_pct, None);
    assert_eq!(s.battery_v, None);
    assert_eq!(s.load_w, None);
    assert_eq!(s.updated_at, 0);
    assert_eq!(s.source, "");
    assert_eq!(s.status, "");
}

#[test]
fn the_views_serialize_in_camel_case() {
    let s =
        DeviceStatus::from_data(&json!({"panelPower": "10", "capacity": "50", "source": "cloud"}));
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["pvW"], json!(10.0));
    assert_eq!(v["batteryPct"], json!(50.0));
    assert_eq!(v["source"], json!("cloud"));
    assert_eq!(v["updatedAt"], json!(0));
    let t = TotalData::from_data(&json!({"epvToday": "1.5"}));
    let v = serde_json::to_value(&t).unwrap();
    assert_eq!(v["solar"], json!({"today": 1.5, "total": null}));
}

// ------------------------------------------------------------------------------------------------ settings and relogin

#[tokio::test]
async fn a_setting_is_read_with_the_token() {
    let srv = Server::start().await;
    assert_eq!(srv.client("sun").setting("ac-output-source").await, Ok(1));
    let reads = srv.reads();
    assert_eq!(
        (reads[0].method.as_str(), reads[0].path.as_str()),
        ("GET", "/api/v1/growatt/inverter-settings/ac-output-source")
    );
    assert_eq!(reads[0].header("authorization"), Some("Bearer t1"));
}

#[tokio::test]
async fn a_setting_is_written_as_a_value() {
    let srv = Server::start().await;
    srv.client("sun")
        .set_setting("max-charge-current", 15)
        .await
        .unwrap();
    let reads = srv.reads();
    assert_eq!(
        (reads[0].method.as_str(), reads[0].path.as_str()),
        (
            "POST",
            "/api/v1/growatt/inverter-settings/max-charge-current"
        )
    );
    assert!(
        reads[0]
            .header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&reads[0].body).unwrap(),
        json!({"value": 15})
    );
    assert_eq!(srv.settings.lock().unwrap()["max-charge-current"], 15);
}

#[tokio::test]
async fn a_refused_setting_passes_the_message_on() {
    let srv = Server::start().await;
    *srv.refuse_settings.lock().unwrap() = Some("Device is not a storage device".into());
    assert_eq!(
        srv.client("sun").set_setting("charge-source", 1).await,
        Err("The Growatt server answered 500: Device is not a storage device".into())
    );
}

#[tokio::test]
async fn relogin_reports_whether_the_server_reconnected() {
    let srv = Server::start().await;
    assert_eq!(srv.client("sun").relogin().await, Ok(true));
    assert_eq!(srv.count("POST", "/api/v1/growatt/relogin"), 1);
}

#[tokio::test]
async fn a_write_with_an_expired_token_signs_in_again_once() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    client.device_status().await.unwrap();
    srv.refused.lock().unwrap().push("t1".into());

    client.set_setting("charge-source", 2).await.unwrap();
    let posts: Vec<Req> = srv
        .reads()
        .into_iter()
        .filter(|q| q.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].header("authorization"), Some("Bearer t2"));
    assert_eq!(posts[0].body, posts[1].body);
    assert_eq!(srv.logins(), 2);
}

#[tokio::test]
async fn signing_in_now_checks_the_password() {
    let srv = Server::start().await;
    assert_eq!(
        srv.client("nope").sign_in().await,
        Err("The Growatt server refused the username or password".into())
    );
    let client = srv.client("sun");
    client.sign_in().await.unwrap();
    client.device_status().await.unwrap();
    assert_eq!(srv.logins(), 2); // the refused one and this one: the read reused the token
}

#[tokio::test]
async fn a_server_error_keeps_the_token() {
    let srv = Server::start().await;
    let client = srv.client("sun");
    srv.fail_status.store(true, Ordering::SeqCst);
    assert_eq!(
        client.device_status().await.unwrap_err(),
        "The Growatt server answered 500: Growatt not connected"
    );
    srv.fail_status.store(false, Ordering::SeqCst);
    client.device_status().await.unwrap();
    assert_eq!(srv.logins(), 1);
}
