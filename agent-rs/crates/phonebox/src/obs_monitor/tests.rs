//! `ObsTests` (a fake obs-websocket 5 server with a password) and `ObsPermissionTests` (a connected session set up by
//! hand, as C#'s `ObsPermissionFixture` did by reflection).

use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

const PASSWORD: &str = "supersecret";
const SALT: &str = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=";
const CHALLENGE: &str = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";

#[test]
fn the_password_answer_follows_obs_websocket_5() {
    assert_eq!(
        auth_answer(PASSWORD, SALT, CHALLENGE),
        "sQBlPUYd9mki/3XVFBp4Pt08FCMWdMVIqnFWdEitUME="
    );
}

/// The fake OBS of `ObsTests`.
#[derive(Default)]
struct Obs {
    requests: Mutex<Vec<String>>,
    recording: AtomicBool,
    scene: Mutex<String>,
    hold_inputs: AtomicBool,
    inputs_entered: Notify,
    inputs_ready: Notify,
    sessions: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Obs {
    fn requests(&self) -> Vec<String> {
        lock(&self.requests).clone()
    }

    /// Drops every live connection (the socket goes away mid-session).
    fn kill_sessions(&self) {
        for s in lock(&self.sessions).drain(..) {
            s.abort();
        }
    }

    async fn start() -> (Arc<Obs>, String) {
        let obs = Arc::new(Obs {
            scene: Mutex::new("Desk".into()),
            ..Default::default()
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let me = obs.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let me2 = me.clone();
                let task = tokio::spawn(async move { me2.session(tcp).await });
                lock(&me.sessions).push(task);
            }
        });
        (obs, url)
    }

    async fn session(&self, tcp: tokio::net::TcpStream) {
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, echo_protocol).await else {
            return;
        };
        let send = |v: Value| Message::text(v.to_string());
        let hello = json!({"op": 0, "d": {"obsWebSocketVersion": "5.5.0", "rpcVersion": 1,
            "authentication": {"challenge": CHALLENGE, "salt": SALT}}});
        if ws.send(send(hello)).await.is_err() {
            return;
        }
        let receive = |m: Option<Result<Message, _>>| match m {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(t.as_str()).ok(),
            _ => None,
        };
        let identify = receive(ws.next().await);
        let answer = identify
            .as_ref()
            .and_then(|i| i["d"]["authentication"].as_str());
        if answer != Some(auth_answer(PASSWORD, SALT, CHALLENGE).as_str()) {
            let _ = ws.close(None).await;
            return;
        }
        let _ = ws
            .send(send(json!({"op": 2, "d": {"negotiatedRpcVersion": 1}})))
            .await;
        while let Some(m) = receive(ws.next().await) {
            let d = &m["d"];
            let kind = d["requestType"].as_str().unwrap_or("").to_string();
            if kind == "GetInputList" {
                self.inputs_entered.notify_one();
                if self.hold_inputs.load(Ordering::SeqCst) {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(15), self.inputs_ready.notified())
                            .await;
                }
            }
            let scene = d["requestData"]["sceneName"]
                .as_str()
                .map(|s| format!(":{s}"))
                .unwrap_or_default();
            lock(&self.requests).push(format!("{kind}{scene}"));
            let recording = self.recording.load(Ordering::SeqCst);
            let data = match kind.as_str() {
                "GetStreamStatus" => json!({"outputActive": false, "outputDuration": 0}),
                "GetRecordStatus" => json!({"outputActive": recording, "outputPaused": false,
                    "outputDuration": if recording { 61_000 } else { 0 }}),
                "GetSceneList" => json!({"currentProgramSceneName": lock(&self.scene).clone(),
                    "scenes": [{"sceneName": "BRB"}, {"sceneName": "Desk"}, {"sceneName": "Camera"}]}),
                "GetSpecialInputs" => json!({"mic1": "Mic/Aux"}),
                "GetInputMute" => json!({"inputMuted": true}),
                "GetInputList" => json!({"inputs": [{"inputName": "Mic/Aux"}]}),
                "GetInputVolume" => json!({"inputVolumeMul": 0.65}),
                "GetSceneItemList" => {
                    json!({"sceneItems": [{"sceneItemId": 7, "sourceName": "Webcam", "sceneItemEnabled": true}]})
                }
                "GetSceneTransitionList" => json!({"transitions": [{"transitionName": "Fade"}]}),
                "GetCurrentSceneTransition" => {
                    json!({"transitionName": "Fade", "transitionDuration": 300})
                }
                "GetReplayBufferStatus" => json!({"outputActive": true}),
                "GetVirtualCamStatus" => json!({"outputActive": false}),
                _ => json!({}),
            };
            if kind == "ToggleRecord" {
                self.recording.store(!recording, Ordering::SeqCst);
            }
            if kind == "SetCurrentProgramScene" {
                *lock(&self.scene) = d["requestData"]["sceneName"].as_str().unwrap_or("").into();
            }
            let reply = json!({"op": 7, "d": {"requestType": kind, "requestId": d["requestId"],
                "requestStatus": {"result": true, "code": 100}, "responseData": data}});
            if ws.send(send(reply)).await.is_err() {
                return;
            }
        }
    }
}

/// The server's half of the subprotocol handshake: OBS answers with `obswebsocket.json`.
#[allow(clippy::result_large_err)] // the callback type tungstenite asks for
fn echo_protocol(
    _req: &Request,
    mut res: Response,
) -> Result<Response, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse> {
    res.headers_mut().insert(
        "sec-websocket-protocol",
        "obswebsocket.json".parse().unwrap(),
    );
    Ok(res)
}

fn monitor(dir: &tempfile::TempDir, url: &str, password: &str, control: bool) -> Arc<ObsMonitor> {
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        json!({
            "obs": {"enabled": true, "url": url, "password": password},
            "pages": [{"id": "p", "title": "P", "widgets": [{"id": "o", "type": "obs", "x": 0, "y": 0, "w": 4, "h": 3,
                "options": {"control": control}}]}],
        })
        .to_string(),
    )
    .unwrap();
    Arc::new(ObsMonitor::new(Arc::new(ConfigStore::new(path))))
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(condition());
}

async fn until_deck(obs: &Arc<ObsMonitor>) -> Value {
    for _ in 0..30 {
        let state = obs.deck_snapshot().await;
        if state["connected"] == true && state["inputs"].is_array() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("OBS discovery did not complete");
}

#[tokio::test]
async fn connects_reads_state_and_runs_buttons() {
    let (fake, url) = Obs::start().await;
    let d = tempfile::tempdir().unwrap();
    let obs = monitor(&d, &url, PASSWORD, true);
    let stop = Stop::default();
    obs.start(stop.clone());
    until(|| obs.snapshot().connected).await;
    let s = obs.snapshot();
    assert_eq!(s.scene.as_deref(), Some("Desk"));
    assert_eq!(s.scenes, ["Camera", "Desk", "BRB"]); // OBS lists scenes bottom-up
    assert_eq!(s.mic_muted, Some(true));
    assert_eq!(s.mic_name.as_deref(), Some("Mic/Aux"));
    assert!(!obs.live());

    assert_eq!(obs.act("record", None).await, None);
    assert_eq!(obs.act("scene", Some("Camera")).await, None);
    assert!(obs.act("scene", Some("No such scene")).await.is_some());
    until(|| obs.snapshot().recording && obs.snapshot().scene.as_deref() == Some("Camera")).await;
    assert!(obs.live());
    assert!(
        fake.requests()
            .contains(&"SetCurrentProgramScene:Camera".to_string())
    );
    stop.stop();
}

#[tokio::test]
async fn buttons_need_the_widgets_permission() {
    let d = tempfile::tempdir().unwrap();
    let obs = monitor(&d, "ws://127.0.0.1:9", PASSWORD, false);
    assert!(obs.act("record", None).await.is_some());
    assert!(!obs.deck_act("record", None, None, None).await.ok);
}

#[tokio::test]
async fn deck_reads_capabilities_and_sends_only_validated_resources() {
    let (fake, url) = Obs::start().await;
    let d = tempfile::tempdir().unwrap();
    let obs = monitor(&d, &url, PASSWORD, true);
    let stop = Stop::default();
    obs.start(stop.clone());
    until(|| obs.snapshot().connected).await;
    let state = until_deck(&obs).await;
    assert_eq!(state["inputs"][0]["volume"].as_f64(), Some(65.0));
    assert_eq!(state["replayActive"], true);
    assert!(
        !obs.deck_act("volume", Some("unknown"), Some(50), None)
            .await
            .ok
    );
    assert!(
        !obs.deck_act("volume", Some("Mic/Aux"), Some(101), None)
            .await
            .ok
    );
    assert!(
        !obs.deck_act("source", Some("99"), None, Some(false))
            .await
            .ok
    );
    assert!(
        !obs.deck_act("transition", Some("unknown"), None, None)
            .await
            .ok
    );
    assert!(
        obs.deck_act("volume", Some("Mic/Aux"), Some(45), None)
            .await
            .ok
    );
    assert!(
        obs.deck_act("source", Some("7"), None, Some(false))
            .await
            .ok
    );
    assert!(
        obs.deck_act("transition", Some("Fade"), None, None)
            .await
            .ok
    );
    assert!(obs.deck_act("save-replay", None, None, None).await.ok);
    assert!(obs.deck_act("camera", None, None, None).await.ok);
    let requests = fake.requests();
    assert_eq!(
        requests.iter().filter(|r| *r == "SetInputVolume").count(),
        1
    );
    assert!(requests.contains(&"SetSceneItemEnabled:Desk".to_string()));
    assert!(requests.contains(&"SaveReplayBuffer".to_string()));
    assert!(requests.contains(&"ToggleVirtualCam".to_string()));
    let keys: Vec<&str> = state
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "connected",
            "scene",
            "scenes",
            "inputs",
            "sources",
            "streaming",
            "recording",
            "recordPaused",
            "streamMs",
            "recordMs",
            "transitions",
            "transition",
            "duration",
            "replayAvailable",
            "replayActive",
            "cameraAvailable",
            "cameraActive"
        ]
    );
    stop.stop();
}

#[tokio::test]
async fn slow_obs_discovery_returns_loading_then_the_measured_controls() {
    let (fake, url) = Obs::start().await;
    fake.hold_inputs.store(true, Ordering::SeqCst);
    let d = tempfile::tempdir().unwrap();
    let obs = monitor(&d, &url, PASSWORD, true);
    let stop = Stop::default();
    obs.start(stop.clone());
    until(|| obs.snapshot().connected).await;
    let loading = obs.deck_snapshot().await;
    assert_eq!(loading["loading"], true);
    assert_eq!(loading["connected"], false);
    assert_eq!(
        loading["error"],
        "Discovering OBS controls; waiting for OBS"
    );
    fake.hold_inputs.store(false, Ordering::SeqCst);
    fake.inputs_ready.notify_one();
    let state = until_deck(&obs).await;
    assert_eq!(state["inputs"][0]["id"], "Mic/Aux");
    assert_eq!(state["inputs"][0]["volume"].as_f64(), Some(65.0));
    stop.stop();
}

#[tokio::test]
async fn test_reports_the_wrong_password() {
    let (_fake, url) = Obs::start().await;
    let ok = test(&url, PASSWORD).await;
    assert_eq!(ok, "Connected (obs-websocket 5.5.0)");
    assert_eq!(test(&url, "wrong").await, "Wrong password");
    assert_eq!(test(&url, "").await, "OBS asks for a password");
    assert_eq!(
        test("ws://127.0.0.1:9", PASSWORD).await,
        "No answer: is OBS running with its WebSocket server on?"
    );
}

#[tokio::test]
async fn disconnecting_mid_discovery_never_poisons_cached_deck_snapshot() {
    let (fake, url) = Obs::start().await;
    fake.hold_inputs.store(true, Ordering::SeqCst);
    let d = tempfile::tempdir().unwrap();
    let obs = monitor(&d, &url, PASSWORD, true);
    let stop = Stop::default();
    obs.start(stop.clone());
    until(|| obs.snapshot().connected).await;
    let discovery = tokio::spawn({
        let obs = obs.clone();
        async move { obs.deck_snapshot().await }
    });
    tokio::time::timeout(Duration::from_secs(10), fake.inputs_entered.notified())
        .await
        .unwrap();
    let mut refresh = None;
    for _ in 0..40 {
        refresh = obs.discovery();
        if refresh.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut refresh = refresh.expect("the shared discovery");
    fake.kill_sessions();
    fake.hold_inputs.store(false, Ordering::SeqCst);
    fake.inputs_ready.notify_one();
    let snap = tokio::time::timeout(Duration::from_secs(10), discovery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snap["connected"], false);
    // the caller may have got "loading" first: the shared discovery itself must end disconnected, caching nothing
    let discovered =
        tokio::time::timeout(Duration::from_secs(15), refresh.wait_for(|d| d.is_some()))
            .await
            .unwrap()
            .unwrap()
            .clone()
            .unwrap();
    assert_eq!(discovered["connected"], false);
    assert!(!obs.deck_cached());
    stop.stop();
}

// ---------------------------------------------------------------- ObsPermissionTests

/// A connected session to a peer that answers every request with `{}` (C#'s `ObsPermissionFixture`).
struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    obs: Arc<ObsMonitor>,
    mutations: Arc<AtomicUsize>,
    silent: Arc<AtomicBool>,
    hold_inputs: Arc<AtomicBool>,
    inputs_entered: Arc<Notify>,
    release: Arc<Notify>,
    _peer: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let store = Arc::new(ConfigStore::new(path));
        let obs = Arc::new(ObsMonitor::new(store.clone()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (mutations, silent, hold_inputs) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (inputs_entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let peer = tokio::spawn({
            let (mutations, silent, hold, entered, release) = (
                mutations.clone(),
                silent.clone(),
                hold_inputs.clone(),
                inputs_entered.clone(),
                release.clone(),
            );
            async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let m: Value = serde_json::from_str(t.as_str()).unwrap();
                    let d = &m["d"];
                    let kind = d["requestType"].as_str().unwrap_or("");
                    if kind == "ToggleRecord" {
                        mutations.fetch_add(1, Ordering::SeqCst);
                    }
                    if silent.load(Ordering::SeqCst) {
                        continue;
                    }
                    if kind == "GetInputList" {
                        entered.notify_one();
                        if hold.load(Ordering::SeqCst) {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(10), release.notified())
                                    .await;
                        }
                    }
                    let reply = json!({"op": 7, "d": {"requestId": d["requestId"], "requestStatus": {"result": true},
                        "responseData": {}}});
                    if ws.send(Message::text(reply.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        });
        let tcp = tokio::net::TcpStream::connect(url.trim_start_matches("ws://"))
            .await
            .unwrap();
        let io: Box<dyn Io> = Box::new(tcp);
        let (socket, _) = tokio_tungstenite::client_async(url.as_str(), io)
            .await
            .unwrap();
        obs.install_session(socket);
        Fixture {
            _dir: dir,
            store,
            obs,
            mutations,
            silent,
            hold_inputs,
            inputs_entered,
            release,
            _peer: peer,
        }
    }

    fn configure(&self, page_enabled: bool, control: bool, stack: bool, obs_enabled: bool) {
        let mut widget = json!({"type": "obs", "options": {"control": control}});
        if stack {
            widget = json!({"type": "stack", "options": {"widgets": [widget]}});
        }
        self.store
            .save(
                &json!({"obs": {"enabled": obs_enabled}, "pages": [
                    {"id": "home", "enabled": true, "widgets": [{"type": "clock"}]},
                    {"id": "obs", "enabled": page_enabled, "widgets": [widget]},
                ]})
                .to_string(),
                None,
            )
            .unwrap();
    }

    fn mutations(&self) -> usize {
        self.mutations.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn hidden_pages_cannot_grant_obs_control() {
    for stack in [false, true] {
        let f = Fixture::new().await;
        f.configure(false, true, stack, false);
        assert!(
            f.obs
                .act("record", None)
                .await
                .unwrap()
                .contains("turned off")
        );
        assert!(!f.obs.deck_act("record", None, None, None).await.ok);
        assert_eq!(f.mutations(), 0, "stack {stack}");
    }
}

#[tokio::test]
async fn visible_widget_permission_still_controls_both_obs_surfaces() {
    let f = Fixture::new().await;
    f.configure(true, false, false, false);
    assert!(f.obs.act("record", None).await.is_some());
    assert!(!f.obs.deck_act("record", None, None, None).await.ok);
    assert_eq!(f.mutations(), 0);
    // obs.enabled stays false: the buttons need only a connected session
    f.configure(true, true, false, false);
    assert_eq!(f.obs.act("record", None).await, None);
    assert!(f.obs.deck_act("record", None, None, None).await.ok);
    assert_eq!(f.mutations(), 2);
    f.configure(false, true, false, false);
    assert!(f.obs.act("record", None).await.is_some());
    assert!(!f.obs.deck_act("record", None, None, None).await.ok);
    assert_eq!(f.mutations(), 2);
}

#[tokio::test]
async fn hiding_the_widget_during_discovery_stops_the_deck_action() {
    let f = Fixture::new().await;
    f.configure(true, true, false, true);
    f.hold_inputs.store(true, Ordering::SeqCst);
    let act = tokio::spawn({
        let obs = f.obs.clone();
        async move { obs.deck_act("record", None, None, None).await }
    });
    tokio::time::timeout(Duration::from_secs(5), f.inputs_entered.notified())
        .await
        .unwrap();
    f.configure(false, true, false, true);
    f.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(10), act)
        .await
        .unwrap()
        .unwrap();
    assert!(!result.ok);
    assert_eq!(f.mutations(), 0);
}

#[tokio::test]
async fn turning_obs_off_stops_serving_the_cached_deck() {
    let f = Fixture::new().await;
    f.configure(true, true, false, true);
    let deck = tokio::time::timeout(Duration::from_secs(10), f.obs.deck_snapshot())
        .await
        .unwrap();
    assert_eq!(deck["connected"], true);
    f.configure(true, true, false, false);
    let deck = tokio::time::timeout(Duration::from_secs(10), f.obs.deck_snapshot())
        .await
        .unwrap();
    assert_eq!(deck["connected"], false);
}

#[tokio::test]
async fn a_request_dropped_by_disconnect_cleanup_still_ends_at_its_deadline() {
    let f = Fixture::new().await;
    f.configure(true, true, false, false);
    f.silent.store(true, Ordering::SeqCst);
    let act = tokio::spawn({
        let obs = f.obs.clone();
        async move { obs.act("record", None).await }
    });
    for _ in 0..100 {
        if f.mutations() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(f.mutations(), 1);
    // what the connection's cleanup does to a request it did not see while failing the others
    f.obs.forget_pending();
    let result = tokio::time::timeout(Duration::from_secs(15), act)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.as_deref(), Some("OBS did not answer"));
}
