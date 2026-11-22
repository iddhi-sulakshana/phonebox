//! Antigravity endpoint discovery, quota parsing and plan resolution (`AntigravityTests`).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use super::*;
use crate::json_text::parse;
use crate::quota::connect::{HttpFail, UsageReply, UsageRequest};

fn json(s: &str) -> Node {
    parse(s, false).unwrap()
}

fn output(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn run_with(f: impl Fn(&ToolCommand, Duration) -> Output + Send + Sync + 'static) -> Box<CliRun> {
    Box::new(move |cmd, timeout| {
        let out = f(&cmd, timeout);
        Box::pin(async move { Ok(out) })
    })
}

fn fake(
    f: impl Fn(&UsageRequest) -> Result<UsageReply, HttpFail> + Send + Sync + 'static,
) -> UsageHttp {
    Arc::new(move |req| {
        let result = f(&req);
        Box::pin(async move { result })
    })
}

fn reply(status: u16, body: &str) -> Result<UsageReply, HttpFail> {
    Ok(UsageReply {
        status,
        retry_after: None,
        body: body.into(),
    })
}

fn header<'a>(req: &'a UsageRequest, name: &str) -> Vec<&'a str> {
    req.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

fn authority(url: &str) -> String {
    let uri: http::Uri = url.parse().unwrap();
    format!(
        "{}://{}",
        uri.scheme_str().unwrap(),
        uri.authority().unwrap()
    )
}

fn method(url: &str) -> String {
    url.rsplit('/').next().unwrap().to_string()
}

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

#[tokio::test]
async fn cli_requires_a_successful_usage_command_envelope() {
    let run = run_with(|cmd, _| {
        if cmd.args[0] == "--version" {
            output(0, "1.5.3", "")
        } else {
            output(
                0,
                r#"{"groups":[{"buckets":[{"remaining_fraction":0.5}]}]}"#,
                "",
            )
        }
    });
    assert_eq!(
        read_antigravity_cli(Path::new("agy"), never(), &*run).await,
        Ok(None)
    );
}

#[tokio::test]
async fn unsupported_cli_versions_never_run_a_print_prompt() {
    for version in ["1.0.0", "unknown", "1.1.10", "v1.1"] {
        let calls = Arc::new(Mutex::new(0));
        let counted = calls.clone();
        let run = run_with(move |cmd, _| {
            *counted.lock().unwrap() += 1;
            assert_eq!(cmd.args, ["--version"]);
            output(0, version, "")
        });
        assert_eq!(
            read_antigravity_cli(Path::new("agy"), never(), &*run).await,
            Ok(None),
            "{version}"
        );
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}

#[tokio::test]
async fn supported_cli_reads_only_the_usage_command_with_bounded_execution() {
    let calls = Arc::new(Mutex::new(0));
    let counted = calls.clone();
    let run = run_with(move |cmd, timeout| {
        let mut n = counted.lock().unwrap();
        *n += 1;
        let env: HashMap<_, _> = cmd.env.iter().cloned().collect();
        assert_eq!(
            env.get("AGY_CLI_DISABLE_AUTO_UPDATE").map(String::as_str),
            Some("true")
        );
        assert_eq!(env.get("BROWSER").map(String::as_str), Some("false"));
        assert!((1..=20).contains(&timeout.as_secs()), "{timeout:?}");
        if *n == 1 {
            return output(0, "1.5.3\n", "");
        }
        assert_eq!(
            cmd.args,
            [
                "-p",
                "/usage",
                "--output-format",
                "json",
                "--print-timeout",
                "15s"
            ]
        );
        output(
            0,
            r#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"name":"Gemini Models",
            "buckets":[{"id":"gemini-weekly","window":"weekly","remaining_fraction":0.7354}]}]}}}"#,
            "",
        )
    });
    let provider = read_antigravity_cli(Path::new("agy"), never(), &*run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(provider.limits.len(), 1);
    assert_eq!(provider.limits[0].percent, 26.5);
    assert_eq!(provider.plan, "Antigravity");
    assert_eq!(provider.error, None);
}

#[test]
fn failed_or_unrelated_cli_commands_are_not_quota_data() {
    for (status, command) in [("ERROR", "usage"), ("SUCCESS", "other")] {
        let doc = json(&format!(
            r#"{{"status":"{status}","command":{{"name":"{command}","data":{{"groups":[{{"buckets":[{{"remaining_fraction":0.5}}]}}]}}}}}}"#
        ));
        assert!(
            parse_antigravity_limits(&doc).is_empty(),
            "{status} {command}"
        );
    }
}

#[tokio::test]
async fn failed_cli_exit_does_not_publish_even_an_apparently_valid_payload() {
    let run = run_with(|cmd, _| {
        if cmd.args[0] == "--version" {
            output(0, "1.5.3", "")
        } else {
            output(
                1,
                r#"{"groups":[{"buckets":[{"remaining_fraction":0.5}]}]}"#,
                "private auth error",
            )
        }
    });
    assert_eq!(
        read_antigravity_cli(Path::new("agy"), never(), &*run).await,
        Ok(None)
    );
}

#[test]
fn cli_usage_envelope_maps_measured_groups_without_exposing_conversation_metadata() {
    let doc = json(
        r#"{"status":"SUCCESS","conversation_id":"private-session","num_turns":0,"command":{"name":"usage","data":{"groups":[
        {"name":"Gemini Models","buckets":[{"id":"gemini-weekly","window":"weekly","remaining_fraction":0.7354,"reset_time":"2026-11-13T23:43:55Z"},
        {"id":"gemini-5h","window":"5h","remaining_fraction":1,"reset_time":"2026-11-10T04:17:04Z"}]},
        {"name":"Claude and GPT models","buckets":[{"id":"3p-weekly","window":"weekly","remaining_fraction":1},
        {"id":"3p-5h","window":"5h","remaining_fraction":1}]}]}}}"#,
    );
    let limits = parse_antigravity_limits(&doc);
    assert_eq!(limits.len(), 4);
    assert_eq!(
        (limits[0].label.as_str(), limits[0].percent),
        ("5-hour (Gemini)", 0.0)
    );
    assert_eq!(
        (limits[1].label.as_str(), limits[1].percent),
        ("Weekly (Gemini)", 26.5)
    );
    assert!(limits[1].resets_at.is_some());
    assert_eq!(limits[2].label, "5-hour (Claude/GPT)");
    assert_eq!(limits[3].label, "Weekly (Claude/GPT)");
    assert!(
        !serde_json::to_string(&limits)
            .unwrap()
            .contains("private-session")
    );
}

#[test]
fn discovery_includes_legacy_names_and_rejects_other_editors() {
    let commands: HashMap<&str, &str> = HashMap::from([
        (
            "language_server_windows_x64",
            "language_server_windows_x64 --app_data_dir=antigravity --extension_server_port=18765 --csrf_token=windows",
        ),
        (
            "language_server_linux_x64",
            "language_server_linux_x64 --app_data_dir=antigravity --extension_server_port=18766 --csrf_token=linux",
        ),
        (
            "language_server_linux_arm",
            "language_server_linux_arm --app_data_dir=antigravity --extension_server_port=18767 --csrf_token=arm",
        ),
        (
            "language_server",
            "language_server --app_data_dir=windsurf --port=18768 --csrf_token=other-editor",
        ),
        ("agy", "agy --hub --hub-port=18769 --csrf_token=hub"),
    ]);
    let endpoints = find_antigravity_endpoints(
        &|name| {
            commands
                .get(name)
                .map(|c| vec![(1000, c.to_string())])
                .unwrap_or_default()
        },
        &|_| panic!("No command line asked for a run-time port"),
    );
    let mut ports: Vec<u16> = endpoints.iter().map(|ep| ep.port).collect();
    ports.sort_unstable();
    assert_eq!(ports, [18765, 18766, 18767, 18769]);
    assert!(endpoints.iter().all(|ep| ep.scheme == "http"));
}

#[test]
fn https_server_port_is_read_with_the_servers_own_token() {
    for (cmd, port, token) in [
        (
            "language_server --csrf_token abc --https_server_port 0",
            0,
            "abc",
        ),
        (
            "language_server --csrf_token=abc --https_server_port=62173",
            62173,
            "abc",
        ),
        (
            "language_server --extension_server_port 1 --extension_server_csrf_token ext --csrf_token main --https_server_port 0",
            0,
            "main",
        ),
        (
            "language_server --extension_server_csrf_token ext --https_server_port 0",
            0,
            "ext",
        ),
    ] {
        assert_eq!(
            parse_https_server_from_command_line(cmd),
            Some((port, token.to_string())),
            "{cmd}"
        );
    }
}

#[test]
fn missing_or_invalid_https_server_flags_are_not_candidates() {
    for cmd in [
        "language_server --csrf_token abc",
        "language_server --https_server_port 0",
        "language_server --csrf_token abc --https_server_port -1",
        "language_server --csrf_token abc --https_server_port 65536",
        "language_server --csrf_token abc --https_server_port 0x10",
    ] {
        assert_eq!(parse_https_server_from_command_line(cmd), None, "{cmd}");
    }
}

fn v4(port: u16) -> Listener {
    Listener {
        address: IpAddr::from([127, 0, 0, 1]),
        port,
    }
}

fn v6(port: u16) -> Listener {
    Listener {
        address: "::1".parse().unwrap(),
        port,
    }
}

#[test]
fn run_time_https_ports_come_from_the_same_process_after_every_named_port() {
    // Issue #227: Antigravity 2.32 passes --https_server_port 0 and serves the RPC over HTTPS on a port it picks.
    let commands: HashMap<&str, Vec<(u32, &str)>> = HashMap::from([
        (
            "language_server",
            vec![
                (
                    41,
                    r"C:\Antigravity\language_server.exe --csrf_token=dyn --https_server_port 0 --extension_server_port 58324",
                ),
                (
                    42,
                    "/opt/antigravity/language_server --csrf_token=other --https_server_port 0",
                ),
            ],
        ),
        (
            "language_server_windows_x64",
            vec![(
                43,
                r"antigravity\language_server_windows_x64 --csrf_token=fixed --https_server_port=50001",
            )],
        ),
        (
            "agy",
            vec![(44, "agy --hub --hub-port=18769 --csrf_token=hub")],
        ),
    ]);
    let asked = Mutex::new(Vec::new());
    let endpoints = find_antigravity_endpoints(
        &|name| {
            commands
                .get(name)
                .map(|c| c.iter().map(|(p, s)| (*p, s.to_string())).collect())
                .unwrap_or_default()
        },
        &|pid| {
            asked.lock().unwrap().push(pid);
            match pid {
                41 => vec![v4(58324), v4(62173), v6(62174), v6(58324)],
                42 => vec![v4(50001), v4(7000)],
                _ => vec![v4(9999)],
            }
        },
    );
    assert_eq!(*asked.lock().unwrap(), [41, 42]);
    assert_eq!(
        endpoints,
        [
            AntigravityEndpoint::http(18769, "hub"),
            AntigravityEndpoint::http(58324, "dyn"), // the explicit HTTP extension port of process 41
            AntigravityEndpoint::new(50001, "fixed", "https", "127.0.0.1"), // a concrete HTTPS port
            AntigravityEndpoint::new(62173, "dyn", "https", "127.0.0.1"), // process 41's own listeners, minus the named port
            AntigravityEndpoint::new(62174, "dyn", "https", "[::1]"), // an IPv6-only listener is asked on ::1
            AntigravityEndpoint::new(58324, "dyn", "https", "[::1]"), // [::1]:58324 is not the named 127.0.0.1:58324
            AntigravityEndpoint::new(7000, "other", "https", "127.0.0.1"), // 50001 is already a named candidate
        ]
    );
}

#[test]
fn endpoint_text_never_contains_the_token() {
    let ep = AntigravityEndpoint::new(62173, "secret-csrf-token", "https", "127.0.0.1");
    assert_eq!(ep.to_string(), "https://127.0.0.1:62173");
    let v6 = AntigravityEndpoint {
        port: 62174,
        host: "[::1]".into(),
        ..ep.clone()
    };
    assert_eq!(v6.to_string(), "https://[::1]:62174");
    assert!(!format!("{ep} {ep:?}").contains("secret-csrf-token"));
}

#[test]
fn invalid_certificates_pass_only_for_loopback_https() {
    for (url, accepted) in [
        ("https://127.0.0.1:62173/x", true),
        ("https://[::1]:62173/x", true),
        ("https://localhost:62173/x", true),
        ("https://LOCALHOST:62173/x", true),
        ("https://127.0.0.2:62173/x", true), // all of 127/8 is loopback
        ("https://192.168.1.10:62173/x", false),
        ("https://example.com/x", false),
        ("https://localhost.example.com/x", false),
        ("https://127.0.0.1.nip.io/x", false),
        ("http://127.0.0.1:62173/x", false),
    ] {
        assert_eq!(
            accept_loopback_certificate(Some(url), true),
            accepted,
            "{url}"
        );
        assert!(accept_loopback_certificate(Some(url), false));
    }
    assert!(!accept_loopback_certificate(None, true));
}

#[tokio::test]
async fn https_candidates_use_the_loopback_tls_client_and_http_the_shared_one() {
    let shared = Arc::new(Mutex::new(Vec::new()));
    let tls = Arc::new(Mutex::new(Vec::new()));
    let (s, t) = (shared.clone(), tls.clone());
    let http = fake(move |req| {
        s.lock().unwrap().push(authority(&req.url));
        reply(404, "")
    });
    let https = fake(move |req| {
        t.lock().unwrap().push(authority(&req.url));
        assert_eq!(header(req, "X-Codeium-Csrf-Token"), ["dyn"]);
        reply(
            200,
            r#"{"groups":[{"buckets":[{"window":"5h","remaining":{"remainingFraction":0.5}}]}]}"#,
        )
    });
    let endpoints = [
        AntigravityEndpoint::http(58324, "dyn"),
        AntigravityEndpoint::new(62173, "dyn", "https", "127.0.0.1"),
    ];
    let provider = read_antigravity(&http, &endpoints, never(), Some(&https))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.limits.len(), 1);
    assert_eq!(provider.limits[0].percent, 50.0);
    assert_eq!(
        *shared.lock().unwrap(),
        ["http://127.0.0.1:58324", "http://127.0.0.1:58324"]
    );
    assert_eq!(
        *tls.lock().unwrap(),
        ["https://127.0.0.1:62173", "https://127.0.0.1:62173"]
    );
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn reads_quotas_from_a_self_signed_loopback_https_server() {
    use axum::extract::Path as UrlPath;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;

    let cert: crate::lan_cert::LanCert =
        crate::lan_cert::create("localhost", chrono::Utc::now()).into();
    let config = crate::server::tls::server_config(&cert).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let listener =
        crate::server::tls::TlsListener::new(tcp, config, crate::server::tls::HANDSHAKE_TIMEOUT)
            .unwrap();
    let app = axum::Router::new().route(
        "/exa.language_server_pb.LanguageServerService/{method}",
        axum::routing::post(|UrlPath(method): UrlPath<String>, headers: HeaderMap| async move {
            if headers.get("X-Codeium-Csrf-Token").and_then(|v| v.to_str().ok()) != Some("loopback-token") {
                return StatusCode::FORBIDDEN.into_response();
            }
            if method == "RetrieveUserQuotaSummary" {
                r#"{"groups":[{"buckets":[{"window":"5h","remaining":{"remainingFraction":0.25}}]}]}"#.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let endpoint = [AntigravityEndpoint::new(
        port,
        "loopback-token",
        "https",
        "127.0.0.1",
    )];

    // the certificate really is untrusted: a client with default validation gets nothing
    let strict = crate::quota::connect::usage_http();
    assert_eq!(
        read_antigravity(&strict, &endpoint, never(), None).await,
        Ok(None)
    );

    let http = fake(|_| panic!("HTTPS went to the shared client"));
    let https = loopback_tls_http();
    let provider = read_antigravity(&http, &endpoint, never(), Some(&https))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.limits.len(), 1);
    assert_eq!(provider.limits[0].percent, 75.0);
    server.abort();
}

#[cfg(feature = "tls")]
#[test]
fn loopback_tls_handler_checks_the_request_host() {
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    let cert: crate::lan_cert::LanCert =
        crate::lan_cert::create("localhost", chrono::Utc::now()).into();
    let der = CertificateDer::from(cert.der.clone());
    let verifier = loopback_verifier();
    let check = |host: &str| {
        verifier
            .verify_server_cert(
                &der,
                &[],
                &ServerName::try_from(host.to_string()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .is_ok()
    };
    assert!(check("127.0.0.1"));
    assert!(check("::1"));
    assert!(check("localhost"));
    assert!(!check("example.com"));
    assert!(!check("192.168.1.10"));
}

#[test]
fn malformed_shapes_do_not_throw_or_produce_measurements() {
    for doc in [
        "null",
        "[]",
        r#"{"response":null}"#,
        r#"{"groups":[null,2,{"buckets":[null,2,{}]}]}"#,
    ] {
        let doc = json(doc);
        assert!(parse_antigravity_limits(&doc).is_empty());
        assert!(parse_antigravity_legacy_limits(&doc).is_empty());
        assert_eq!(parse_antigravity_plan(&doc), None);
    }
}

#[test]
fn real_zero_and_full_measurements_are_retained() {
    for (remaining, used) in [("0", 100.0), ("1", 0.0), ("0.0", 100.0), ("1.0", 0.0)] {
        let doc = json(&format!(
            r#"{{"groups":[{{"buckets":[{{"remainingFraction":{remaining}}}]}}]}}"#
        ));
        let limits = parse_antigravity_limits(&doc);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].percent, used);
    }
}

#[tokio::test]
async fn read_retries_candidates_and_uses_legacy_measured_quotas() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let http = fake(move |req| {
        let uri: http::Uri = req.url.parse().unwrap();
        assert_eq!(uri.host(), Some("127.0.0.1"));
        assert_eq!(header(req, "Connect-Protocol-Version"), ["1"]);
        assert_eq!(header(req, "X-Codeium-Csrf-Token"), ["test-token"]);
        assert_eq!(req.body.as_deref(), Some("{}"));
        let method = method(&req.url);
        let port = uri.port_u16().unwrap();
        seen.lock().unwrap().push((port, method.clone()));
        if port == 18765 || method == "RetrieveUserQuotaSummary" {
            return reply(404, "");
        }
        reply(
            200,
            r#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Ultra"}},"cascadeModelConfigData":{
            "clientModelConfigs":[{"label":"Gemini","quotaInfo":{"remainingFraction":0.25,"resetTime":"2026-11-15T07:30:12Z"}},
            {"label":"Unknown","quotaInfo":{"resetTime":"2026-11-15T07:30:12Z"}}]}}}"#,
        )
    });
    let endpoints = [
        AntigravityEndpoint::http(18765, "test-token"),
        AntigravityEndpoint::http(18766, "test-token"),
    ];
    let provider = read_antigravity(&http, &endpoints, never(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.plan, "Ultra");
    assert_eq!(provider.limits.len(), 1);
    assert_eq!(provider.limits[0].percent, 75.0);
    assert_eq!(provider.limits[0].window_sec, None);
    assert!(provider.limits[0].resets_at.is_some());
    assert_eq!(requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn invalid_summary_still_uses_the_measured_legacy_response() {
    let http = fake(|req| {
        if method(&req.url) == "RetrieveUserQuotaSummary" {
            reply(200, "<not an RPC response>")
        } else {
            reply(
                200,
                r#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Ultra"}},
                "cascadeModelConfigData":{"clientModelConfigs":[{"label":"Gemini","quotaInfo":{"remainingFraction":0.25}}]}}}"#,
            )
        }
    });
    let provider = read_antigravity(
        &http,
        &[AntigravityEndpoint::http(18765, "test-token")],
        never(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(provider.plan, "Ultra");
    assert_eq!(provider.limits[0].percent, 75.0);
}

#[tokio::test]
async fn plan_failure_does_not_discard_measured_summary() {
    let http = fake(|req| {
        if method(&req.url) == "GetUserStatus" {
            reply(503, "")
        } else {
            reply(
                200,
                r#"{"groups":[{"buckets":[{"window":"5h","remaining":{"remainingFraction":0.25}}]}]}"#,
            )
        }
    });
    let provider = read_antigravity(
        &http,
        &[AntigravityEndpoint::http(18765, "test-token")],
        never(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(provider.plan, "Antigravity");
    assert_eq!(provider.limits.len(), 1);
    assert_eq!(provider.limits[0].percent, 75.0);
}

#[tokio::test]
async fn cancelled_read_stops_without_publishing_healthy_data() {
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel_tx = Mutex::new(Some(cancel_tx));
    let http: UsageHttp = Arc::new(move |_| {
        if let Some(tx) = cancel_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Box::pin(std::future::pending())
    });
    let cancel = async {
        let _ = cancel_rx.await;
    };
    let result = read_antigravity(
        &http,
        &[AntigravityEndpoint::http(18765, "test-token")],
        cancel,
        None,
    )
    .await;
    assert_eq!(result, Err(Cancelled));
}

#[tokio::test]
async fn missing_measurements_do_not_produce_a_healthy_provider() {
    let http = fake(|_| reply(200, r#"{"groups":[{"buckets":[{}]}]}"#));
    assert_eq!(
        read_antigravity(
            &http,
            &[AntigravityEndpoint::http(18765, "test-token")],
            never(),
            None
        )
        .await,
        Ok(None)
    );
}

#[test]
fn nested_remaining_fraction_is_measured_instead_of_reported_as_unused() {
    let doc = json(
        r#"{"response":{"groups":[{"buckets":[{"bucketId":"gemini-5h",
        "remaining":{"remainingFraction":0.25},"resetTime":"2026-11-15T07:30:12Z"}]}]}}"#,
    );
    let limits = parse_antigravity_limits(&doc);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].percent, 75.0);
    assert!(limits[0].resets_at.is_some());
}

#[test]
fn unknown_or_invalid_measurements_are_skipped() {
    for bucket in [
        "{}",
        r#"{"remainingFraction":null}"#,
        r#"{"remainingFraction":"unknown"}"#,
        r#"{"remainingFraction":-0.1}"#,
        r#"{"remainingFraction":1.1}"#,
    ] {
        let doc = json(&format!(r#"{{"groups":[{{"buckets":[{bucket}]}}]}}"#));
        assert!(parse_antigravity_limits(&doc).is_empty(), "{bucket}");
    }
}

#[test]
fn endpoint_uses_the_correct_port_and_matching_token() {
    for (cmd, port, token) in [
        (
            "language_server_windows_x64 --extension_server_port=18766 --csrf_token=legacy",
            18766,
            "legacy",
        ),
        (
            "language_server_linux_x64 --extension_server_port 18766 --csrf_token main --extension_server_csrf_token extension",
            18766,
            "extension",
        ),
        (
            "agy --port=18765 --hub-port=18766 --csrf_token main",
            18766,
            "main",
        ),
    ] {
        assert_eq!(
            parse_endpoint_from_command_line(cmd),
            Some((port, token.to_string())),
            "{cmd}"
        );
    }
}

#[test]
fn invalid_ports_are_not_endpoints() {
    for cmd in [
        "agy --hub-port=0 --csrf_token=x",
        "agy --hub-port=65536 --csrf_token=x",
        "agy --hub-port=123abc --csrf_token=x",
    ] {
        assert_eq!(parse_endpoint_from_command_line(cmd), None, "{cmd}");
    }
}

#[test]
fn parse_endpoint_from_command_line_extracts_port_and_token() {
    for (cmd, port, token) in [
        (
            "agy.exe --hub --hub-port=13897 --csrf_token=token-12345",
            13897,
            "token-12345",
        ),
        (
            "language_server --port 8080 --csrfToken abc-xyz",
            8080,
            "abc-xyz",
        ),
        (
            "Antigravity --hub_port=9090 --csrf_token \"my-quoted-token\"",
            9090,
            "my-quoted-token",
        ),
        (
            "agy --csrf_token 'single quoted' --hub-port 1",
            1,
            "single quoted",
        ),
        (
            "agy --hub-port=1 --csrf_token=first --csrf_token=last",
            1,
            "last",
        ),
    ] {
        assert_eq!(
            parse_endpoint_from_command_line(cmd),
            Some((port, token.to_string())),
            "{cmd}"
        );
    }
}

#[test]
fn parse_endpoint_from_command_line_invalid_returns_null() {
    for cmd in [
        "",
        "agy.exe --hub --hub-port=13897",
        "language_server --csrf_token=my-token",
        "notepad.exe somefile.txt",
        "agy --hub-port=1 --csrf_token --next",
        "agy --hub-port=1 --csrf_token=\"open",
        "agy --hub-port=1x2\"3 --csrf_token=x",
    ] {
        assert_eq!(parse_endpoint_from_command_line(cmd), None, "{cmd}");
    }
}

#[test]
fn parse_antigravity_limits_multi_group_orders_session_before_weekly() {
    let doc = json(
        r#"{"response":{"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.95,"resetTime":"2026-11-15T07:30:12Z"},
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.90,"resetTime":"2026-11-09T22:15:55Z"}]},
            {"displayName":"Claude and GPT models","buckets":[
                {"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.15,"resetTime":"2026-11-16T17:46:14Z"},
                {"bucketId":"3p-5h","window":"5h","remainingFraction":1.0,"resetTime":"2026-11-09T22:46:14Z"}]}]}}"#,
    );
    let limits = parse_antigravity_limits(&doc);
    let rows: Vec<_> = limits
        .iter()
        .map(|l| {
            (
                l.label.as_str(),
                l.percent,
                l.window_sec,
                l.severity.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("5-hour (Gemini)", 10.0, Some(18000), "normal"),
            ("Weekly (Gemini)", 5.0, Some(604800), "normal"),
            ("5-hour (Claude/GPT)", 0.0, Some(18000), "normal"),
            ("Weekly (Claude/GPT)", 85.0, Some(604800), "warning"),
        ]
    );
}

#[test]
fn parse_antigravity_limits_single_group_uses_simple_labels() {
    let doc = json(
        r#"{"response":{"groups":[{"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.02,"resetTime":"2026-11-09T22:15:55Z"},
            {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.80,"resetTime":"2026-11-15T07:30:12Z"}]}]}}"#,
    );
    let limits = parse_antigravity_limits(&doc);
    assert_eq!(limits.len(), 2);
    assert_eq!(
        (
            limits[0].label.as_str(),
            limits[0].percent,
            limits[0].severity.as_str()
        ),
        ("5-hour session", 98.0, "critical")
    );
    assert_eq!(
        (
            limits[1].label.as_str(),
            limits[1].percent,
            limits[1].severity.as_str()
        ),
        ("Weekly", 20.0, "normal")
    );
}

#[test]
fn parse_antigravity_limits_malformed_or_empty_returns_empty_list() {
    assert!(parse_antigravity_limits(&json("{}")).is_empty());
}

#[test]
fn parse_antigravity_plan_extracts_plan_name_falls_back_to_user_tier() {
    let with_plan = json(
        r#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Pro"}},"userTier":{"name":"Google AI Pro"}}}"#,
    );
    assert_eq!(parse_antigravity_plan(&with_plan).as_deref(), Some("Pro"));
    let tier_only = json(r#"{"userStatus":{"userTier":{"name":"Google AI Ultra"}}}"#);
    assert_eq!(
        parse_antigravity_plan(&tier_only).as_deref(),
        Some("Google AI Ultra")
    );
    let blank =
        json(r#"{"planStatus":{"planInfo":{"planName":"  "}},"userTier":{"name":"Root tier"}}"#);
    assert_eq!(parse_antigravity_plan(&blank).as_deref(), Some("Root tier"));
    assert_eq!(parse_antigravity_plan(&json("{}")), None);
}

#[test]
fn the_process_table_gives_command_lines_by_dotnet_process_name() {
    let me = std::process::id();
    let name = {
        let exe = std::env::current_exe().unwrap();
        crate::process_monitor::dotnet_name(me, &exe.file_name().unwrap().to_string_lossy())
    };
    let rows = read_commands(&name);
    let mine = rows
        .iter()
        .find(|(pid, _)| *pid == me)
        .expect("this test process");
    assert!(!mine.1.is_empty());
    assert!(read_commands("no-such-process-name-for-phonebox").is_empty());
}
