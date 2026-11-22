//! `ObsMonitor.cs`: OBS Studio through obs-websocket 5 — the widget's state and buttons, and (in [`controls`]) the
//! Control Deck's OBS surface. One connection while `obs.enabled`; polled every 2 s; reconnects 5 s after it ends.

pub mod controls;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, ObsConfig};
use crate::server::Stop;

pub const DEFAULT_URL: &str = "ws://127.0.0.1:4455";
const NOT_RUNNING: &str = "OBS is not running or its WebSocket server is off";
const NOT_CONNECTED: &str = "OBS is not connected";
const NO_ANSWER: &str = "OBS did not answer";
const DISCONNECTED: &str = "OBS disconnected";
const CONTROL_OFF: &str = "controlling OBS is turned off in the widget's settings";
const CATEGORY: &str = "ObsMonitor";

/// Any stream a WebSocket runs over (TCP, TLS, a test pipe).
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}
pub type Socket = WebSocketStream<Box<dyn Io>>;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsSnapshot {
    pub connected: bool,
    pub error: Option<String>,
    pub streaming: bool,
    pub stream_ms: i64,
    pub recording: bool,
    pub record_paused: bool,
    pub record_ms: i64,
    pub scene: Option<String>,
    pub scenes: Vec<String>,
    pub mic_muted: Option<bool>,
    pub mic_name: Option<String>,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// One connection to OBS: messages go out through a writer task, the reader task answers the requests.
pub(crate) struct Session {
    pub(crate) id: u64,
    out: mpsc::UnboundedSender<Message>,
    open: Arc<AtomicBool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Session {
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct ObsMonitor {
    cfg: Arc<ConfigStore>,
    snap: Mutex<ObsSnapshot>,
    /// The live session (`_ws`); its lock is `_sessionGate`, never held across an await.
    ws: Mutex<Option<Arc<Session>>>,
    pending: Pending,
    sessions: AtomicU64,
    last_error: Mutex<Option<String>>,
    pub(crate) deck: controls::DeckState,
}

impl ObsMonitor {
    pub fn new(cfg: Arc<ConfigStore>) -> ObsMonitor {
        ObsMonitor {
            cfg,
            snap: Mutex::new(ObsSnapshot::default()),
            ws: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            sessions: AtomicU64::new(0),
            last_error: Mutex::new(None),
            deck: controls::DeckState::default(),
        }
    }

    pub fn snapshot(&self) -> ObsSnapshot {
        lock(&self.snap).clone()
    }

    /// Streaming, or recording and not paused (the rule engine's "OBS is live").
    pub fn live(&self) -> bool {
        let s = self.snapshot();
        s.connected && (s.streaming || (s.recording && !s.record_paused))
    }

    /// The connection loop, until `stop`.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            while !stop.is_stopped() {
                let obs = me.cfg.get().obs.clone();
                if !obs.enabled {
                    {
                        let mut s = lock(&me.snap);
                        s.connected = false;
                        s.error = None;
                    }
                    me.invalidate_deck();
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        _ = stop.stopped() => return,
                    }
                    continue;
                }
                if let Err(message) = me.run_connection(&obs, &stop).await
                    && !stop.is_stopped()
                {
                    {
                        let mut s = lock(&me.snap);
                        s.connected = false;
                        s.error = Some(message.clone());
                    }
                    let mut last = lock(&me.last_error);
                    if last.as_deref() != Some(message.as_str()) {
                        crate::log::info(CATEGORY, &format!("obs: {message}"));
                        *last = Some(message);
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = stop.stopped() => return,
                }
            }
        })
    }

    /// One connection: handshake, then a poll every 2 s while it is open and OBS stays enabled. Its cleanup retires
    /// the session, drops the deck and fails the requests still waiting.
    async fn run_connection(self: &Arc<Self>, obs: &ObsConfig, stop: &Stop) -> Result<(), String> {
        let url = if obs.url.trim().is_empty() {
            DEFAULT_URL
        } else {
            obs.url.as_str()
        };
        let mut socket = tokio::select! {
            connected = tokio::time::timeout(Duration::from_secs(5), connect(url)) => match connected {
                Ok(Ok(socket)) => socket,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("The operation was canceled.".into()),
            },
            _ = stop.stopped() => return Ok(()),
        };
        let hello = receive(&mut socket)
            .await?
            .ok_or("OBS closed the connection")?;
        let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
        if let Some(auth) = hello["d"].get("authentication").filter(|a| !a.is_null()) {
            let password = obs.password.as_deref().unwrap_or("");
            if password.is_empty() {
                return Err("OBS asks for a password (Settings › General › OBS)".into());
            }
            identify["authentication"] = json!(auth_answer(
                password,
                auth["salt"].as_str().unwrap_or(""),
                auth["challenge"].as_str().unwrap_or("")
            ));
        }
        socket
            .send(Message::text(json!({ "op": 1, "d": identify }).to_string()))
            .await
            .map_err(|_| NOT_RUNNING.to_string())?;
        match receive(&mut socket).await? {
            Some(m) if m["op"].as_i64() == Some(2) => {}
            _ => return Err("OBS rejected the password".into()),
        }
        crate::log::info(CATEGORY, "obs: connected");
        let session = self.attach(socket);
        *lock(&self.ws) = Some(session.clone());
        let result = self.poll(&session, stop).await;
        {
            let mut ws = lock(&self.ws);
            *ws = None;
            lock(&self.snap).connected = false; // the error is kept
            self.invalidate_deck();
        }
        session.close();
        for (_, waiting) in lock(&self.pending).drain() {
            let _ = waiting.send(Err(DISCONNECTED.into()));
        }
        result
    }

    async fn poll(&self, session: &Arc<Session>, stop: &Stop) -> Result<(), String> {
        let mut mic: Option<String> = None;
        while !stop.is_stopped() && session.is_open() && self.cfg.get().obs.enabled {
            let stream = self.request("GetStreamStatus", None).await?;
            let record = self.request("GetRecordStatus", None).await?;
            let scenes = self.request("GetSceneList", None).await?;
            if mic.is_none() {
                mic = self.request("GetSpecialInputs", None).await?["mic1"]
                    .as_str()
                    .map(String::from);
            }
            let muted = match &mic {
                Some(m) => self
                    .request("GetInputMute", Some(json!({ "inputName": m })))
                    .await?["inputMuted"]
                    .as_bool(),
                None => None,
            };
            // OBS lists scenes bottom-up
            let mut names: Vec<String> = scenes["scenes"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|s| s["sceneName"].as_str())
                        .filter(|n| !n.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            names.reverse();
            *lock(&self.snap) = ObsSnapshot {
                connected: true,
                error: None,
                streaming: stream["outputActive"].as_bool().unwrap_or(false),
                stream_ms: stream["outputDuration"].as_i64().unwrap_or(0),
                recording: record["outputActive"].as_bool().unwrap_or(false),
                record_paused: record["outputPaused"].as_bool().unwrap_or(false),
                record_ms: record["outputDuration"].as_i64().unwrap_or(0),
                scene: scenes["currentProgramSceneName"].as_str().map(String::from),
                scenes: names,
                mic_muted: muted,
                mic_name: mic.clone(),
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = stop.stopped() => {}
            }
        }
        Ok(())
    }

    /// The widget's buttons (`POST /api/obs/{action}`): None when done, else why not. Needs a connected session, not
    /// `obs.enabled`.
    pub async fn act(&self, action: &str, scene: Option<&str>) -> Option<String> {
        if !control_allowed(&self.cfg.get()) {
            return Some(CONTROL_OFF.into());
        }
        let snap = self.snapshot();
        let (kind, data) = match action {
            "record" => ("ToggleRecord", None),
            "pause" => ("ToggleRecordPause", None),
            "stream" => ("ToggleStream", None),
            "mic" if snap.mic_name.is_some() => (
                "ToggleInputMute",
                Some(json!({ "inputName": snap.mic_name })),
            ),
            "scene"
                if scene.is_some_and(|s| !s.is_empty() && snap.scenes.iter().any(|x| x == s)) =>
            {
                (
                    "SetCurrentProgramScene",
                    Some(json!({ "sceneName": scene })),
                )
            }
            _ => return Some("unknown action".into()),
        };
        crate::log::info(CATEGORY, &format!("obs: {action} {}", scene.unwrap_or("")));
        self.request(kind, data).await.err()
    }

    /// Sends one request on the current session; its answer, OBS's refusal, or "OBS did not answer" after 5 s.
    pub(crate) async fn request(&self, kind: &str, data: Option<Value>) -> Result<Value, String> {
        let session = self.current().ok_or(NOT_CONNECTED)?;
        let id = crate::control_deck::leases::nonce();
        let (answer, waiting) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), answer);
        struct Forget<'a>(&'a Pending, &'a str);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                lock(self.0).remove(self.1);
            }
        }
        let _forget = Forget(&self.pending, &id);
        let mut d = Map::new();
        d.insert("requestType".into(), json!(kind));
        d.insert("requestId".into(), json!(id));
        if let Some(data) = data {
            d.insert("requestData".into(), data);
        }
        // a request registered after the cleanup sweep must not hang
        if !self.is_current(&session) {
            return Err(NOT_CONNECTED.into());
        }
        if session
            .out
            .send(Message::text(json!({ "op": 6, "d": d }).to_string()))
            .is_err()
        {
            return Err(DISCONNECTED.into());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        match tokio::time::timeout_at(deadline, waiting).await {
            Ok(Ok(result)) => result,
            // dropped without an answer: it still ends at its deadline
            Ok(Err(_)) => {
                tokio::time::sleep_until(deadline).await;
                Err(NO_ANSWER.into())
            }
            Err(_) => Err(NO_ANSWER.into()),
        }
    }

    fn current(&self) -> Option<Arc<Session>> {
        lock(&self.ws).clone()
    }

    fn is_current(&self, session: &Arc<Session>) -> bool {
        lock(&self.ws)
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, session))
    }

    /// Starts the reader and writer of a connected (and identified) socket.
    pub(crate) fn attach(&self, socket: Socket) -> Arc<Session> {
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let open = Arc::new(AtomicBool::new(true));
        let writer = tokio::spawn({
            let open = open.clone();
            async move {
                while let Some(m) = outgoing.recv().await {
                    if sink.send(m).await.is_err() {
                        break;
                    }
                }
                open.store(false, Ordering::SeqCst);
            }
        });
        let reader = tokio::spawn({
            let (open, pending) = (open.clone(), self.pending.clone());
            async move {
                // ReadLoop: answers the requests; a close, an error or a message that is not JSON ends it
                while let Some(Ok(m)) = stream.next().await {
                    let text = match &m {
                        Message::Text(t) => t.as_str().to_string(),
                        Message::Binary(b) => String::from_utf8_lossy(b).into_owned(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let Ok(m) = serde_json::from_str::<Value>(&text) else {
                        break;
                    };
                    if m["op"].as_i64() != Some(7) {
                        continue;
                    }
                    let d = &m["d"];
                    let Some(waiting) = d["requestId"]
                        .as_str()
                        .and_then(|id| lock(&pending).remove(id))
                    else {
                        continue;
                    };
                    let status = &d["requestStatus"];
                    let result = if status["result"] == true {
                        Ok(d.get("responseData")
                            .filter(|r| !r.is_null())
                            .cloned()
                            .unwrap_or_else(|| json!({})))
                    } else {
                        Err(status["comment"]
                            .as_str()
                            .unwrap_or("OBS refused the request")
                            .to_string())
                    };
                    let _ = waiting.send(result);
                }
                open.store(false, Ordering::SeqCst);
            }
        });
        Arc::new(Session {
            id: self.sessions.fetch_add(1, Ordering::SeqCst) + 1,
            out,
            open,
            tasks: Mutex::new(vec![writer, reader]),
        })
    }

    /// Tests: a connected session as `ObsPermissionFixture` sets one up (no handshake, no polling).
    #[cfg(test)]
    pub(crate) fn install_session(&self, socket: Socket) -> Arc<Session> {
        let session = self.attach(socket);
        *lock(&self.ws) = Some(session.clone());
        *lock(&self.snap) = ObsSnapshot {
            connected: true,
            ..Default::default()
        };
        session
    }

    /// Tests: what the connection's cleanup does to the requests it fails (C# reflected on `_pending`).
    #[cfg(test)]
    pub(crate) fn forget_pending(&self) {
        lock(&self.pending).clear();
    }
}

/// `/api/state`'s `obs`: `{"streaming","recording","recordPaused"}` while connected, else null.
impl crate::state::StateSource for ObsMonitor {
    fn snapshot(&self) -> Value {
        let s = ObsMonitor::snapshot(self);
        if s.connected {
            json!({ "streaming": s.streaming, "recording": s.recording, "recordPaused": s.record_paused })
        } else {
            Value::Null
        }
    }
}

/// An `obs` or `obs-*` widget on an enabled page allows control.
fn control_allowed(cfg: &AgentConfig) -> bool {
    cfg.widgets(true).iter().any(|w| {
        (w.r#type == "obs" || w.r#type.starts_with("obs-")) && w.bool("control") == Some(true)
    })
}

/// obs-websocket 5's answer: `base64(sha256(base64(sha256(password + salt)) + challenge))`.
pub fn auth_answer(password: &str, salt: &str, challenge: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let b64 = base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    b64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

/// Opens the WebSocket with the `obswebsocket.json` subprotocol (TLS for `wss://` under the `tls` feature).
async fn connect(url: &str) -> Result<Socket, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    let secure = match parsed.scheme() {
        "ws" => false,
        "wss" => true,
        _ => return Err("Only Uris starting with 'ws://' or 'wss://' are supported.".into()),
    };
    let host = parsed
        .host_str()
        .ok_or("The URI has no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if secure { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|_| NOT_RUNNING.to_string())?;
    let _ = tcp.set_nodelay(true);
    let io: Box<dyn Io> = if secure {
        tls(tcp, &host).await?
    } else {
        Box::new(tcp)
    };
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        "obswebsocket.json".parse().expect("a header value"),
    );
    let (socket, _) = tokio_tungstenite::client_async(request, io)
        .await
        .map_err(|_| NOT_RUNNING.to_string())?;
    Ok(socket)
}

#[cfg(feature = "tls")]
async fn tls(tcp: tokio::net::TcpStream, host: &str) -> Result<Box<dyn Io>, String> {
    let mut roots = crate::quota::connect::webpki_roots();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(crate::quota::connect::tls_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name =
        rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|_| NOT_RUNNING.to_string())?;
    Ok(Box::new(stream))
}

#[cfg(not(feature = "tls"))]
async fn tls(_tcp: tokio::net::TcpStream, _host: &str) -> Result<Box<dyn Io>, String> {
    Err("this build has no TLS: use ws://".into())
}

/// The next JSON message; None when OBS closed the connection.
async fn receive(socket: &mut Socket) -> Result<Option<Value>, String> {
    loop {
        let text = match socket.next().await {
            Some(Ok(Message::Text(t))) => t.as_str().to_string(),
            Some(Ok(Message::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(_)) => continue,
            Some(Err(_)) => return Err(NOT_RUNNING.into()),
        };
        return serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| e.to_string());
    }
}

/// Settings › General › OBS › Test: one connection with the given (maybe unsaved) address and password, 5 s in all.
pub async fn test(url: &str, password: &str) -> String {
    const NO_ANSWER_TEST: &str = "No answer: is OBS running with its WebSocket server on?";
    let url = if url.trim().is_empty() {
        DEFAULT_URL
    } else {
        url
    };
    let attempt = async {
        let mut socket = connect(url).await?;
        let hello = receive(&mut socket).await?.unwrap_or(Value::Null);
        let version = hello["d"]["obsWebSocketVersion"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
        if let Some(auth) = hello["d"].get("authentication").filter(|a| !a.is_null()) {
            if password.is_empty() {
                return Ok("OBS asks for a password".to_string());
            }
            identify["authentication"] = json!(auth_answer(
                password,
                auth["salt"].as_str().unwrap_or(""),
                auth["challenge"].as_str().unwrap_or("")
            ));
        }
        socket
            .send(Message::text(json!({ "op": 1, "d": identify }).to_string()))
            .await
            .map_err(|_| NOT_RUNNING.to_string())?;
        let answer = receive(&mut socket).await.unwrap_or(None);
        let _ = socket.close(None).await;
        Ok(match answer {
            Some(m) if m["op"].as_i64() == Some(2) => {
                format!("Connected (obs-websocket {version})")
            }
            _ => "Wrong password".to_string(),
        })
    };
    match tokio::time::timeout(Duration::from_secs(5), attempt).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) if e == NOT_RUNNING => NO_ANSWER_TEST.into(),
        Ok(Err(e)) => e,
        Err(_) => NO_ANSWER_TEST.into(),
    }
}

#[cfg(test)]
mod tests;
