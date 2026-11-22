use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use http_body_util::BodyExt;

use crate::server::guard::tests::{send, test_state};
use crate::server::{AppState, build_router};

const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

async fn body(res: axum::response::Response) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

/// Status, content type and body of a request to the local port.
async fn call(state: &AppState, method: &str, path: &str) -> (StatusCode, Option<String>, String) {
    let app = build_router(state.clone());
    let res = send(&app, method, path, &[LOCAL]).await;
    let status = res.status();
    let kind = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    (status, kind, body(res).await)
}

const JSON: Option<&str> = Some("application/json; charset=utf-8");

fn json() -> Option<String> {
    JSON.map(String::from)
}

#[tokio::test]
async fn work_quotas_and_approvals_answer_their_empty_shapes() {
    let (_d, s) = test_state();
    assert_eq!(
        call(&s, "GET", "/api/work").await,
        (
            StatusCode::OK,
            json(),
            r#"{"sessions":[],"github":null}"#.into()
        )
    );
    assert_eq!(
        call(&s, "GET", "/api/quotas").await,
        (
            StatusCode::OK,
            json(),
            r#"{"providers":[],"overview":null}"#.into()
        )
    );
    assert_eq!(
        call(&s, "GET", "/api/approvals").await,
        (StatusCode::OK, json(), r#"{"items":[]}"#.into())
    );
}

#[tokio::test]
async fn quotas_carry_the_teamclaude_view_only_when_there_is_one() {
    let (_d, s) = test_state();
    let (_, _, body) = call(&s, "GET", "/api/quotas").await;
    assert!(!body.contains("teamclaude"), "{body}");
    s.quotas.state.lock().unwrap().teamclaude = Some(crate::quota::teamclaude::TeamView {
        host: "tc.example.com".into(),
        ..crate::quota::teamclaude::TeamView::named()
    });
    let (status, _, body) = call(&s, "GET", "/api/quotas").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["teamclaude"]["name"], "TeamClaude");
    assert_eq!(v["teamclaude"]["host"], "tc.example.com");
    assert_eq!(v["providers"], serde_json::json!([]));
}

#[tokio::test]
async fn the_work_feed_lists_sessions_and_github_in_csharp_key_order() {
    let (_d, s) = test_state();
    s.sessions
        .set_sessions(vec![crate::session_monitor::AgentSession {
            pid: Some(1122),
            ..crate::session_monitor::AgentSession::new(
                "claude:sess-1",
                "claude",
                "my-app",
                "my-app",
                "waiting",
                Some("permission prompt"),
                0,
                0,
            )
        }]);
    let gh = Arc::new(crate::github_monitor::GitHubMonitor::with_gh(Arc::new(
        |args: Vec<String>| {
            Box::pin(async move {
                Ok(if args[1] == "graphql" {
                    r#"{"data":{"viewer":{"login":"me","pullRequests":{"nodes":[]}},"search":{"nodes":[
                  {"number":7,"title":"t","isDraft":false,"updatedAt":"2025-11-20T00:53:20Z","author":{"login":"x"},
                   "repository":{"nameWithOwner":"o/r"}}]}}}"#
                    .to_string()
                } else {
                    "[1,2,3]".to_string()
                })
            })
        },
    )));
    gh.refresh_once().await;
    let s = s.clone().with_work(s.sessions.clone(), gh.clone());
    let (status, _, text) = call(&s, "GET", "/api/work").await;
    assert_eq!(status, StatusCode::OK);
    let updated = gh.snapshot().unwrap().updated_at.unwrap();
    assert_eq!(
        text,
        format!(
            concat!(
                r#"{{"sessions":[{{"id":"claude:sess-1","tool":"claude","name":"my-app","project":"my-app","status":"waiting","#,
                r#""waitingFor":"permission prompt","since":0,"startedAt":0,"pid":1122,"model":null,"effort":null,"#,
                r#""totalTokens":null,"inputTokens":null,"outputTokens":null,"cachedInputTokens":null,"reasoningTokens":null,"#,
                r#""lastInputTokens":null,"contextWindow":null,"turnStartedAt":null,"outcome":null,"lastTool":null,"title":null}}],"#,
                r#""github":{{"user":"me","notifications":3,"reviewRequests":[{{"repo":"o/r","number":7,"title":"t","author":"x","#,
                r#""draft":false,"review":null,"ci":null,"updated":1763600000000}}],"myPrs":[],"updatedAt":{},"error":null}}}}"#
            ),
            updated
        )
    );
}

#[tokio::test]
async fn a_conversation_needs_an_id_and_the_setting() {
    let (_d, s) = test_state();
    assert_eq!(
        call(&s, "GET", "/api/work/conversation").await,
        (StatusCode::BAD_REQUEST, None, String::new())
    );
    assert_eq!(
        call(&s, "GET", "/api/work/conversation?id=x").await,
        (StatusCode::OK, json(), r#"{"enabled":false}"#.into())
    );
    s.cfg
        .save(r#"{"claude":{"shareConversation":true}}"#, None)
        .unwrap();
    assert_eq!(
        call(&s, "GET", "/api/work/conversation?id=").await,
        (
            StatusCode::OK,
            json(),
            r#"{"enabled":true,"messages":[],"steps":[]}"#.into()
        )
    );
}

/// A prompt the phone sees: the phone counts as connected, then `ask` waits in the background.
fn ask(s: &AppState, id: &str) -> tokio::task::JoinHandle<Option<String>> {
    s.phone
        .last_seen
        .store(crate::server::now_ms(), std::sync::atomic::Ordering::SeqCst);
    let approvals = s.approvals.clone();
    let approval = crate::approval_store::Approval {
        id: id.into(),
        source: "Claude Code".into(),
        session: "sess".into(),
        project: "my-app".into(),
        tool: "Run a command".into(),
        detail: "ls".into(),
        description: None,
        created_at: 1,
        expires_at: crate::server::now_ms() + 60_000,
        kind: "permission".into(),
    };
    tokio::spawn(async move { approvals.ask(approval, std::future::pending()).await })
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn approvals_are_listed_and_decided() {
    let (_d, s) = test_state();
    let asking = ask(&s, "a1b2c3d4e5f6");
    until("the prompt", || !s.approvals.snapshot().is_empty()).await;
    let (status, kind, text) = call(&s, "GET", "/api/approvals").await;
    assert_eq!((status, kind), (StatusCode::OK, json()));
    let expires = s.approvals.snapshot()[0].expires_at;
    assert_eq!(
        text,
        format!(
            r#"{{"items":[{{"id":"a1b2c3d4e5f6","source":"Claude Code","session":"sess","project":"my-app","tool":"Run a command","detail":"ls","description":null,"createdAt":1,"expiresAt":{expires},"kind":"permission"}}]}}"#
        )
    );
    assert_eq!(
        call(&s, "POST", "/api/approvals/a1b2c3d4e5f6").await,
        (StatusCode::BAD_REQUEST, None, String::new())
    );
    assert_eq!(
        call(&s, "POST", "/api/approvals/nope?decision=allow").await,
        (StatusCode::NOT_FOUND, None, String::new())
    );
    assert_eq!(
        call(&s, "POST", "/api/approvals/a1b2c3d4e5f6?decision=allow").await,
        (StatusCode::OK, None, String::new())
    );
    assert_eq!(asking.await.unwrap().as_deref(), Some("allow"));
}

async fn post_hook(state: &AppState, json_body: &str) -> (StatusCode, Option<String>, String) {
    use tower::ServiceExt;
    let app = build_router(state.clone());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(crate::claude_hook::ENDPOINT)
        .header("host", "127.0.0.1:8765")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json_body.to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let kind = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    (status, kind, body(res).await)
}

const PLAIN: Option<&str> = Some("text/plain; charset=utf-8");

#[tokio::test]
async fn the_permission_hook_answers_with_the_phone_decision() {
    let (_d, s) = test_state();
    let nothing = (StatusCode::OK, PLAIN.map(String::from), String::new());
    assert_eq!(post_hook(&s, "{ not json").await, nothing);
    assert_eq!(post_hook(&s, "[1]").await, nothing);
    s.cfg
        .save(r#"{"claude":{"phoneApprovals":false}}"#, None)
        .unwrap();
    assert_eq!(post_hook(&s, r#"{"tool_name":"Bash"}"#).await, nothing);

    s.cfg
        .save(
            r#"{"claude":{"phoneApprovals":true,"onlyWhenAway":false}}"#,
            None,
        )
        .unwrap();
    s.phone
        .last_seen
        .store(crate::server::now_ms(), std::sync::atomic::Ordering::SeqCst);
    let posting = {
        let s = s.clone();
        tokio::spawn(async move {
            post_hook(&s, "\u{feff}{\"session_id\":\"x\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls\"}}").await
        })
    };
    until("the prompt", || !s.approvals.snapshot().is_empty()).await;
    let card = s.approvals.snapshot().remove(0);
    assert_eq!(
        (card.tool.as_str(), card.detail.as_str()),
        ("Run a command", "ls")
    );
    assert!(s.approvals.decide(&card.id, "allow"));
    assert_eq!(
        posting.await.unwrap(),
        (
            StatusCode::OK,
            json(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#.into()
        )
    );
}

#[tokio::test]
async fn the_hook_install_routes_edit_the_settings_file() {
    let (dir, mut s) = test_state();
    let settings = dir.path().join(".claude").join("settings.json");
    s.claude_settings = settings.clone();
    let path = serde_json::to_string(&settings.display().to_string()).unwrap();
    assert_eq!(
        call(&s, "GET", "/api/claude/hook").await,
        (
            StatusCode::OK,
            json(),
            format!(r#"{{"installed":false,"path":{path}}}"#)
        )
    );
    assert_eq!(
        call(&s, "POST", "/api/claude/hook").await,
        (StatusCode::OK, None, String::new())
    );
    assert!(
        std::fs::read_to_string(&settings)
            .unwrap()
            .contains(":8765/api/hooks/claude/permission")
    );
    assert_eq!(
        call(&s, "GET", "/api/claude/hook").await,
        (
            StatusCode::OK,
            json(),
            format!(r#"{{"installed":true,"path":{path}}}"#)
        )
    );
    assert_eq!(
        call(&s, "DELETE", "/api/claude/hook").await,
        (StatusCode::OK, None, String::new())
    );
    assert!(!crate::claude_hook::installed(&settings));
    std::fs::write(&settings, "[1]").unwrap();
    assert_eq!(
        call(&s, "POST", "/api/claude/hook").await,
        (
            StatusCode::BAD_REQUEST,
            json(),
            r#"{"error":"settings.json is not a JSON object"}"#.into()
        )
    );
}

const PS: &str = r#"{"ID":"a1b2c3d4e5f6","Image":"redis:7","Labels":"com.docker.compose.project=webapp","Names":"cache-1","State":"running","Status":"Up 5 minutes"}"#;

fn fake_docker(s: &AppState) -> Arc<crate::docker_monitor::DockerMonitor> {
    Arc::new(crate::docker_monitor::DockerMonitor::new(s.cfg.clone()).with_cli("docker").with_runner(Arc::new(
        |_cli, args: Vec<String>| {
            Box::pin(async move {
                Ok(match args[0].as_str() {
                    "ps" => (0, PS.to_string(), String::new()),
                    "stats" => (0, r#"{"ID":"a1b2c3d4e5f6","CPUPerc":"2.50%","MemUsage":"24.5MiB / 7.77GiB"}"#.to_string(), String::new()),
                    _ => (0, String::new(), String::new()),
                })
            })
        },
    )))
}

#[tokio::test]
async fn docker_routes_show_the_snapshot_and_act() {
    let (_d, mut s) = test_state();
    s.docker = fake_docker(&s);
    let (status, kind, text) = call(&s, "GET", "/api/docker").await;
    assert_eq!((status, kind), (StatusCode::OK, json()));
    let updated = s.docker.snapshot().updated;
    assert_eq!(
        text,
        format!(
            r#"{{"snapshot":{{"available":true,"error":null,"containers":[{{"id":"a1b2c3d4e5f6","name":"cache-1","image":"redis:7","state":"running","status":"Up 5 minutes","health":null,"cpu":2.5,"mem":"24.5MiB","project":"webapp"}}],"updated":{updated}}},"control":false}}"#
        )
    );
    assert_eq!(
        call(&s, "POST", "/api/docker/a1b2c3d4e5f6/restart").await,
        (
            StatusCode::BAD_REQUEST,
            json(),
            r#"{"error":"starting and stopping containers is turned off in the widget's settings"}"#.into()
        )
    );
    s.cfg
        .save(
            r#"{"pages":[{"id":"p","title":"P","enabled":true,"widgets":[{"id":"d","type":"docker","x":0,"y":0,"w":6,"h":4,"options":{"control":true}}]}]}"#,
            None,
        )
        .unwrap();
    assert_eq!(
        call(&s, "POST", "/api/docker/a1b2c3d4e5f6/restart").await,
        (StatusCode::OK, json(), r#"{"ok":true}"#.into())
    );
}

#[tokio::test]
async fn command_widgets_answer_their_result_or_404() {
    let (_d, s) = test_state();
    assert_eq!(
        call(&s, "GET", "/api/widget/p/w").await,
        (StatusCode::NOT_FOUND, None, String::new())
    );
    s.cfg
        .save(
            r#"{"pages":[{"id":"p","title":"P","enabled":true,"widgets":[{"id":"w","type":"command","x":0,"y":0,"w":2,"h":2,"options":{"command":"","commandUnix":""}}]}]}"#,
            None,
        )
        .unwrap();
    assert_eq!(
        call(&s, "GET", "/api/widget/p/w?force=maybe").await,
        (StatusCode::BAD_REQUEST, None, String::new())
    );
    assert_eq!(
        call(&s, "GET", "/api/widget/p/w?force=TRUE").await,
        (
            StatusCode::OK,
            json(),
            r#"{"output":"(no command)","at":0,"ok":true,"running":false,"durationMs":0}"#.into()
        )
    );
}

#[tokio::test]
async fn wrong_methods_are_405() {
    let (_d, s) = test_state();
    for (method, path) in [
        ("PUT", "/api/work"),
        ("POST", "/api/quotas"),
        ("GET", "/api/hooks/claude/permission"),
        ("PATCH", "/api/claude/hook"),
        ("GET", "/api/approvals/x"),
        ("GET", "/api/docker/x/start"),
        ("POST", "/api/widget/p/w"),
    ] {
        assert_eq!(
            call(&s, method, path).await.0,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_hook_routes_are_not_served_over_wifi() {
    let (_d, s) = test_state();
    s.cfg
        .save(
            r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key"}}"#,
            None,
        )
        .unwrap();
    let app = crate::server::build_wifi_router(s);
    let key = ("authorization", "Bearer phone-key");
    for (method, path) in [
        ("POST", crate::claude_hook::ENDPOINT),
        ("GET", "/api/claude/hook"),
        ("POST", "/api/claude/hook"),
    ] {
        let res = send(&app, method, path, &[key]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    for path in ["/api/work", "/api/quotas", "/api/approvals"] {
        assert_eq!(
            send(&app, "GET", path, &[key]).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_request_whose_client_disconnects_ends_its_prompt() {
    use tokio::io::AsyncWriteExt;
    let (_d, s) = test_state();
    s.cfg
        .save(
            r#"{"claude":{"phoneApprovals":true,"onlyWhenAway":false}}"#,
            None,
        )
        .unwrap();
    s.phone
        .last_seen
        .store(crate::server::now_ms(), std::sync::atomic::Ordering::SeqCst);
    let seq = s.push.seq();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(crate::server::serve(
        listener,
        s.clone(),
        Duration::from_secs(1),
    ));

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let json = r#"{"session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
        crate::claude_hook::ENDPOINT,
        json.len()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    until("the prompt", || !s.approvals.snapshot().is_empty()).await;

    // the user answered in the terminal: curl goes away
    drop(client);
    until("the prompt to leave the phone", || {
        s.approvals.snapshot().is_empty()
    })
    .await;
    let kinds: Vec<String> = s.push.since(seq).into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"approval-done".to_string()), "{kinds:?}");
    s.stop.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

#[tokio::test]
async fn the_stream_sends_the_live_work_feed() {
    let (dir, base) = test_state();
    let sources = crate::state::StateSources::phase1().with_work(Box::new(
        crate::session_monitor::WorkFeed {
            sessions: base.sessions.clone(),
            github: base.github.clone(),
        },
    ));
    let s = AppState::new(
        base.cfg.clone(),
        base.endpoint,
        base.push.clone(),
        Arc::new(sources),
    )
    .with_work(base.sessions.clone(), base.github.clone());
    s.sessions
        .set_sessions(vec![crate::session_monitor::AgentSession::new(
            "codex:1", "codex", "repo", "repo", "busy", None, 5, 1,
        )]);
    let app = build_router(s);
    let mut stream = send(&app, "GET", "/api/stream", &[LOCAL]).await.into_body();
    let mut work = None;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(3), stream.frame()).await {
            Ok(Some(Ok(frame))) => {
                let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
                if text.starts_with("event: work\n") {
                    work = Some(text);
                    break;
                }
            }
            _ => break,
        }
    }
    let work = work.expect("a work frame");
    assert!(
        work.starts_with("event: work\ndata: {\"sessions\":[{\"id\":\"codex:1\""),
        "{work}"
    );
    drop(dir);
}

#[test]
fn the_process_list_sees_the_agent_sessions() {
    use crate::process_monitor::{AgentSessionRef, AgentSessions};
    let (_d, s) = test_state();
    s.sessions
        .set_sessions(vec![crate::session_monitor::AgentSession {
            pid: Some(42),
            ..crate::session_monitor::AgentSession::new(
                "claude:1", "claude", "my-app", "repo", "idle", None, 77, 1,
            )
        }]);
    assert_eq!(
        s.sessions.sessions(),
        [AgentSessionRef {
            pid: Some(42),
            title: "my-app".into(),
            updated: 77
        }]
    );
}
