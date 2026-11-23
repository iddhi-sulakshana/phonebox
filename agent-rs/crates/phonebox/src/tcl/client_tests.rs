//! The TCL cloud client against the fake cloud: the sign-in chain, the device list, the signed shadow read and
//! publish, renewals, and what it refuses.

use std::sync::atomic::Ordering;

use serde_json::{Map, Value, json};

use super::*;
use crate::tcl::fake::{ACCOUNT, Cloud, PASSWORD};
use crate::tcl::sigv4::md5_hex;

fn props(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test]
async fn signing_in_runs_the_whole_chain_once() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    client.devices().await.unwrap();
    assert_eq!(
        cloud.paths(),
        [
            "/account/login",
            "/v3/global/cloud_url_get",
            "/v3/auth/refresh_tokens",
            "/cognito",
            "/dev/v3/user/get_things"
        ]
    );
    let seen = cloud.seen();
    // the account sign-in, as TCL's Android app sends it
    let login = &seen[0];
    assert_eq!(login.query, "clientId=54148614");
    assert_eq!(login.header("th_platform"), Some("android"));
    assert_eq!(login.header("th_version"), Some("4.8.1"));
    assert_eq!(login.header("th_appbulid"), Some("830"));
    assert_eq!(login.header("user-agent"), Some("Android"));
    assert_eq!(
        serde_json::from_str::<Value>(&login.body).unwrap(),
        json!({"username": ACCOUNT, "password": md5_hex(PASSWORD), "equipment": 2, "osType": 1,
            "clientVersion": "4.8.1", "osVersion": "6.0", "deviceModel": "AndroidAndroid SDK built for x86",
            "captchaRule": 2, "channel": "app"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[1].body).unwrap(),
        json!({"ssoId": "sso-user", "ssoToken": "base1", "appId": APP_ID})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[2].body).unwrap(),
        json!({"ssoToken": "base1", "userId": "sso-user", "appId": APP_ID})
    );
    let cognito = &seen[3];
    assert_eq!(
        cognito.header("x-amz-target"),
        Some("AWSCognitoIdentityService.GetCredentialsForIdentity")
    );
    assert_eq!(
        cognito.header("content-type"),
        Some("application/x-amz-json-1.1")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&cognito.body).unwrap(),
        json!({"IdentityId": "eu-central-1:identity", "Logins": {"cognito-identity.amazonaws.com": "cog1"}})
    );
    let list = &seen[4];
    assert_eq!(list.body, "{}");
    assert_eq!(list.header("accesstoken"), Some("saas1"));
    assert_eq!(list.header("countrycode"), Some("LK"));
    assert_eq!(list.header("platform"), Some("android"));
    assert_eq!(list.header("appversion"), Some("5.4.1"));
    assert_eq!(list.header("thomeversion"), Some("4.8.1"));
    assert_eq!(list.header("accept-language"), Some("en"));
    let (timestamp, nonce) = (
        list.header("timestamp").unwrap(),
        list.header("nonce").unwrap(),
    );
    assert_eq!(timestamp, crate::tcl::fake::T0.to_string());
    assert_eq!(nonce.len(), 12);
    assert!(
        nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    );
    assert_eq!(
        list.header("sign"),
        Some(md5_hex(&format!("{timestamp}{nonce}saas1")).as_str())
    );
    // the session is kept
    client.devices().await.unwrap();
    assert_eq!(cloud.count("/account/login"), 1);
    assert_eq!(cloud.count("/cognito"), 1);
    assert_eq!(cloud.count("/dev/v3/user/get_things"), 2);
}

#[tokio::test]
async fn devices_have_names_and_online_flags() {
    let cloud = Cloud::start().await;
    let list = cloud.client(PASSWORD).devices().await.unwrap();
    assert_eq!(
        list,
        [
            TclDevice {
                id: "ac1".into(),
                name: "Shop front".into(),
                online: true
            },
            TclDevice {
                id: "ac2".into(),
                name: "Back room".into(),
                online: true
            },
            TclDevice {
                id: "ac3".into(),
                name: "ac3".into(),
                online: false
            },
        ]
    );
}

#[tokio::test]
async fn the_shadow_read_is_signed_for_iotdata() {
    let cloud = Cloud::start().await;
    let reported = cloud.client(PASSWORD).reported("ac1").await.unwrap();
    assert_eq!(reported["currentTemperature"], json!(29.5));
    let iot = cloud.iot();
    assert_eq!(
        (iot[0].method.as_str(), iot[0].path.as_str()),
        ("GET", "/iot/things/ac1/shadow")
    );
    let auth = iot[0].header("authorization").unwrap();
    assert!(
        auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKID1/20261123/eu-central-1/iotdata/aws4_request, "
        ),
        "{auth}"
    );
    assert_eq!(iot[0].header("x-amz-security-token"), Some("ST1"));
    assert_eq!(iot[0].header("x-amz-date"), Some("20261123T100000Z"));
}

#[tokio::test]
async fn a_publish_sends_the_desired_state_with_the_celsius_copy() {
    let cloud = Cloud::start().await;
    cloud
        .client(PASSWORD)
        .publish(
            "ac1",
            props(json!({"powerSwitch": 1, "targetTemperature": 26})),
        )
        .await
        .unwrap();
    let iot = cloud.iot();
    let p = &iot[0];
    assert_eq!(p.method, "POST");
    assert_eq!(
        p.path,
        "/iot/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate"
    );
    assert_eq!(p.query, "qos=1");
    let body: Value = serde_json::from_str(&p.body).unwrap();
    assert_eq!(
        body["state"]["desired"],
        json!({"powerSwitch": 1, "targetTemperature": 26, "targetCelsiusDegree": 26})
    );
    assert_eq!(
        body["clientToken"],
        json!(format!("mobile_{}", crate::tcl::fake::T0 / 1000))
    );
    // without a temperature there is no celsius copy
    cloud
        .client(PASSWORD)
        .publish("ac1", props(json!({"generatorMode": 2})))
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&cloud.iot()[1].body).unwrap();
    assert_eq!(body["state"]["desired"], json!({"generatorMode": 2}));
}

#[tokio::test]
async fn an_old_session_is_renewed_from_the_base_token() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    client.devices().await.unwrap();
    cloud.advance(24 * 60_000);
    client.devices().await.unwrap();
    assert_eq!(cloud.count("/cognito"), 1);
    cloud.advance(2 * 60_000); // 26 minutes: the SaaS token is near its end
    client.devices().await.unwrap();
    assert_eq!(cloud.count("/account/login"), 1);
    assert_eq!(cloud.count("/v3/global/cloud_url_get"), 2);
    assert_eq!(cloud.count("/v3/auth/refresh_tokens"), 2);
    assert_eq!(cloud.count("/cognito"), 2);
    assert_eq!(
        cloud.seen().last().unwrap().header("accesstoken"),
        Some("saas2")
    );
}

#[tokio::test]
async fn a_refused_iot_call_renews_and_retries_once() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    client.reported("ac1").await.unwrap();
    cloud.s.iot_403.store(1, Ordering::SeqCst);
    client.reported("ac1").await.unwrap();
    assert_eq!(cloud.count("/cognito"), 2);
    assert_eq!(
        cloud.iot().last().unwrap().header("x-amz-security-token"),
        Some("ST2")
    );
    assert_eq!(cloud.count("/account/login"), 1);

    cloud.s.iot_403.store(2, Ordering::SeqCst);
    let before = cloud.iot().len();
    let err = client.reported("ac1").await.unwrap_err();
    assert_eq!(err, "AWS IoT refused TCL's credentials (HTTP 403)");
    assert_eq!(cloud.iot().len(), before + 2);
}

#[tokio::test]
async fn a_refused_device_list_renews_and_retries_once() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    client.devices().await.unwrap();
    cloud.s.dead_saas.lock().unwrap().push("saas1".into());
    client.devices().await.unwrap();
    assert_eq!(cloud.count("/v3/auth/refresh_tokens"), 2);
    cloud.s.list_fails.store(true, Ordering::SeqCst);
    let before = cloud.count("/dev/v3/user/get_things");
    assert_eq!(
        client.devices().await.unwrap_err(),
        "TCL's device list refused the session: token invalid"
    );
    assert_eq!(cloud.count("/dev/v3/user/get_things"), before + 2);
}

#[tokio::test]
async fn a_dead_base_token_signs_in_with_the_password_again() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    client.devices().await.unwrap();
    cloud.s.dead_bases.lock().unwrap().push("base1".into());
    cloud.advance(26 * 60_000);
    client.devices().await.unwrap();
    assert_eq!(cloud.count("/account/login"), 2);
    let lookups: Vec<Value> = cloud
        .seen()
        .iter()
        .filter(|q| q.path == "/v3/global/cloud_url_get")
        .map(|q| serde_json::from_str(&q.body).unwrap())
        .collect();
    assert_eq!(lookups.last().unwrap()["ssoToken"], json!("base2"));
}

#[tokio::test]
async fn a_wrong_password_says_so() {
    let cloud = Cloud::start().await;
    let err = cloud.client("nope").devices().await.unwrap_err();
    assert_eq!(err, "TCL refused the sign-in: password error");
    assert!(!err.contains("nope") && !err.contains(&md5_hex("nope")));
    assert_eq!(cloud.paths(), ["/account/login"]);
}

#[tokio::test]
async fn a_region_that_is_not_a_region_is_refused() {
    let cloud = Cloud::start().await;
    *cloud.s.region.lock().unwrap() = Some("evil.example.com/x".into());
    let err = cloud.client(PASSWORD).devices().await.unwrap_err();
    assert_eq!(err, "TCL's cloud answered an unexpected region");
    assert!(cloud.count("/v3/auth/refresh_tokens") == 0 && cloud.count("/cognito") == 0);
}

#[tokio::test]
async fn a_thing_id_with_a_slash_is_refused() {
    let cloud = Cloud::start().await;
    let client = cloud.client(PASSWORD);
    for id in ["../x", "ac1/shadow", "", "ac 1"] {
        assert_eq!(client.reported(id).await.unwrap_err(), "Not a TCL unit id");
        assert_eq!(
            client
                .publish(id, props(json!({"powerSwitch": 0})))
                .await
                .unwrap_err(),
            "Not a TCL unit id"
        );
    }
    assert!(cloud.seen().is_empty());
}

#[tokio::test]
async fn a_unit_without_a_shadow_says_so() {
    let cloud = Cloud::start().await;
    assert_eq!(
        cloud.client(PASSWORD).reported("ac9").await.unwrap_err(),
        "TCL has no state for this unit yet"
    );
}
