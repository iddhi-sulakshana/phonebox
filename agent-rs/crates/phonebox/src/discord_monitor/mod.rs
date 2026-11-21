//! `DiscordMonitor.cs`: the Discord widget through the desktop app's local RPC — who is in the voice channel, mute and
//! deafen, joining channels, and notifications. Logs in once with the user's approval (OAuth through the app), keeps
//! the login next to the settings, and polls every 2 s while connected; reconnects with a 5 → 30 s back-off.

pub mod ipc;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, WriteHalf};
use tokio::sync::oneshot;

use crate::config::AgentConfig;
use crate::config::store::ConfigStore;
use crate::server::Stop;
use ipc::Io;

const CATEGORY: &str = "DiscordMonitor";
const SCOPES: [&str; 4] = [
    "rpc",
    "rpc.voice.read",
    "rpc.voice.write",
    "rpc.notifications.read",
];
const REDIRECT: &str = "http://127.0.0.1";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const IMAGE_CACHE_LIMIT: usize = 8 << 20;
const IMAGE_MAX: usize = 512 << 10;
const NOT_CONNECTED: &str = "Discord is not connected";
const NOT_RUNNING: &str = "Discord is not running";
const CONTROL_OFF: &str = "controlling Discord is turned off in the widget's settings";
const RETRY: &str = " PhoneBox will not ask again until you change the Discord settings (or turn Discord off and on).";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordMember {
    pub id: String,
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordChannel {
    pub id: String,
    pub name: String,
    pub guild: Option<String>,
    pub guild_id: Option<String>,
    pub guild_icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordNotification {
    pub ts: i64,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordSnapshot {
    pub connected: bool,
    pub error: Option<String>,
    pub user: Option<String>,
    pub muted: bool,
    pub deafened: bool,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub channel_kind: Option<String>,
    pub members: Vec<DiscordMember>,
    pub channels: Vec<DiscordChannel>,
    pub notifications: Vec<DiscordNotification>,
}

impl DiscordSnapshot {
    pub fn empty(error: Option<String>) -> DiscordSnapshot {
        DiscordSnapshot {
            connected: false,
            error,
            user: None,
            muted: false,
            deafened: false,
            channel_id: None,
            channel_name: None,
            channel_kind: None,
            members: Vec::new(),
            channels: Vec::new(),
            notifications: Vec::new(),
        }
    }
}

/// The saved login (`discord-token.json`, shared with the C# agent: PascalCase, `Expires` in Unix seconds).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Tokens {
    client_id: String,
    access: String,
    refresh: String,
    expires: i64,
}

/// An icon or avatar and its type.
type Image = (Vec<u8>, &'static str);

/// Why a request got no answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RpcError {
    /// The pipe failed (shown as "Discord is not running" by the loop).
    Io(String),
    Timeout,
    /// Discord answered with an ERROR event.
    Refused(String),
}

impl RpcError {
    fn text(&self) -> String {
        match self {
            RpcError::Io(e) => e.clone(),
            RpcError::Timeout => "Discord did not answer".into(),
            RpcError::Refused(e) => e.clone(),
        }
    }
}

/// How a connection ended.
enum Ended {
    /// The user (or Discord) refused the login: not asked again until the settings change.
    Refused(String),
    /// The app went away or never answered.
    Dropped,
    Failed(String),
}

impl From<RpcError> for Ended {
    fn from(e: RpcError) -> Ended {
        match e {
            RpcError::Io(_) | RpcError::Timeout => Ended::Dropped,
            RpcError::Refused(m) => Ended::Failed(m),
        }
    }
}

struct Shared {
    snap: DiscordSnapshot,
    /// Newest first, at most 20.
    notes: VecDeque<DiscordNotification>,
    /// The current connection's reader has ended: a poll still finishing must not bring "connected" back.
    closed: bool,
    /// Bumped by each button press that published mute/deafen.
    voice_rev: i32,
}

type Writer = Arc<tokio::sync::Mutex<WriteHalf<Box<dyn Io>>>>;
type Answer = Result<Option<Value>, RpcError>;

pub struct DiscordMonitor {
    cfg: Arc<ConfigStore>,
    shared: Mutex<Shared>,
    pending: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
    io: Mutex<Option<Writer>>,
    notes_wanted: AtomicBool,
    logged_in: AtomicBool,
    /// Guild id → its voice channels, in the order Discord listed the guilds (cleared on each login).
    guild_channels: Mutex<indexmap::IndexMap<String, Vec<DiscordChannel>>>,
    /// Guild id → icon hash, asked once per guild per process.
    guild_icons: Mutex<HashMap<String, Option<String>>>,
    images: Mutex<(HashMap<String, Image>, usize)>,
    /// One button press at a time, so two quick toggles each start from the state the previous one produced.
    act_gate: tokio::sync::Mutex<()>,
    images_http: reqwest::Client,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A widget of type `discord` on an enabled page shows notifications (on unless turned off).
pub fn notifications_wanted(cfg: &AgentConfig) -> bool {
    cfg.widgets(true)
        .iter()
        .any(|w| w.r#type == "discord" && w.bool("notifications") != Some(false))
}

/// A `discord` widget on an enabled page allows control.
pub fn control_allowed(cfg: &AgentConfig) -> bool {
    cfg.widgets(true)
        .iter()
        .any(|w| w.r#type == "discord" && w.bool("control") == Some(true))
}

/// The wait before reconnecting: 5 s, then doubling to 30 s.
pub fn backoff(failures: u32) -> Duration {
    let doubled = 5u64.saturating_mul(1u64 << failures.saturating_sub(1).min(16));
    Duration::from_secs(doubled.min(30))
}

impl DiscordMonitor {
    pub fn new(cfg: Arc<ConfigStore>) -> DiscordMonitor {
        DiscordMonitor {
            cfg,
            shared: Mutex::new(Shared {
                snap: DiscordSnapshot::empty(None),
                notes: VecDeque::new(),
                closed: false,
                voice_rev: 0,
            }),
            pending: Mutex::new(HashMap::new()),
            io: Mutex::new(None),
            notes_wanted: AtomicBool::new(false),
            logged_in: AtomicBool::new(false),
            guild_channels: Mutex::new(indexmap::IndexMap::new()),
            guild_icons: Mutex::new(HashMap::new()),
            images: Mutex::new((HashMap::new(), 0)),
            act_gate: tokio::sync::Mutex::new(()),
            images_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("the Discord image client"),
        }
    }

    /// The widget's state; notification texts only while notifications are wanted now (read from the settings).
    pub fn snapshot(&self) -> DiscordSnapshot {
        let notes_now = self.notes_now();
        let shared = lock(&self.shared);
        let mut s = shared.snap.clone();
        s.notifications = if notes_now {
            shared.notes.iter().cloned().collect()
        } else {
            Vec::new()
        };
        s
    }

    /// `NotesNow`: notifications were subscribed, Discord is on and a widget shows them.
    fn notes_now(&self) -> bool {
        let cfg = self.cfg.get();
        self.notes_wanted.load(Ordering::SeqCst)
            && cfg.discord.enabled
            && notifications_wanted(&cfg)
    }

    /// Tests: whether notifications were subscribed (`NotificationsOn`).
    #[cfg(test)]
    pub(crate) fn set_notifications_on(&self, on: bool) {
        self.notes_wanted.store(on, Ordering::SeqCst);
    }

    /// `discord-token.json` next to the settings; never the settings file itself.
    pub fn token_path(&self) -> PathBuf {
        let settings =
            std::path::absolute(self.cfg.path()).unwrap_or_else(|_| self.cfg.path().to_path_buf());
        let beside = settings.with_file_name("discord-token.json");
        if beside
            .to_string_lossy()
            .eq_ignore_ascii_case(&settings.to_string_lossy())
        {
            let mut own = settings.into_os_string();
            own.push(".discord-token.json");
            return PathBuf::from(own);
        }
        beside
    }

    pub(crate) fn voice_revision(&self) -> i32 {
        lock(&self.shared).voice_rev
    }

    /// Publishes a poll; a button press that finished since the poll read `rev` keeps its mute/deafen state.
    pub(crate) fn publish_poll(
        &self,
        rev: i32,
        muted: bool,
        deafened: bool,
        snapshot: DiscordSnapshot,
    ) {
        let mut shared = lock(&self.shared);
        if shared.closed {
            return; // the connection this poll came from has ended
        }
        let stale = rev != shared.voice_rev;
        let (muted, deafened) = if stale {
            (shared.snap.muted, shared.snap.deafened)
        } else {
            (muted, deafened)
        };
        shared.snap = DiscordSnapshot {
            muted,
            deafened,
            ..snapshot
        };
    }

    /// The voice settings Discord answered a press with.
    pub(crate) fn apply_voice(&self, vs: Option<&Value>) {
        let Some(vs) = vs else {
            return;
        };
        let mut shared = lock(&self.shared);
        shared.voice_rev += 1;
        if let Some(m) = vs.get("mute").and_then(Value::as_bool) {
            shared.snap.muted = m;
        }
        if let Some(d) = vs.get("deaf").and_then(Value::as_bool) {
            shared.snap.deafened = d;
        }
    }

    /// The widget's buttons: mute, deafen, leave, join (a voice channel from the list the agent sent). None when done.
    pub async fn act(&self, action: &str, channel: Option<&str>) -> Option<String> {
        if !control_allowed(&self.cfg.get()) {
            return Some(CONTROL_OFF.into());
        }
        let _one_at_a_time = self.act_gate.lock().await;
        if !control_allowed(&self.cfg.get()) {
            return Some(CONTROL_OFF.into());
        }
        let s = lock(&self.shared).snap.clone();
        if !s.connected {
            return Some(NOT_CONNECTED.into());
        }
        let (cmd, args, voice) = match action {
            "mute" => ("SET_VOICE_SETTINGS", json!({ "mute": !s.muted }), true),
            "deafen" => ("SET_VOICE_SETTINGS", json!({ "deaf": !s.deafened }), true),
            "leave" => ("SELECT_VOICE_CHANNEL", json!({ "channel_id": null }), false),
            "join" if channel.is_some_and(|c| s.channels.iter().any(|x| x.id == c)) => (
                "SELECT_VOICE_CHANNEL",
                json!({ "channel_id": channel, "force": true }),
                false,
            ),
            _ => return Some("unknown action".into()),
        };
        crate::log::info(
            CATEGORY,
            &format!("discord: {action} {}", channel.unwrap_or("")),
        );
        match self
            .request(cmd, Some(args), None, Duration::from_secs(5))
            .await
        {
            Ok(answer) => {
                if voice {
                    self.apply_voice(answer.as_ref());
                }
                None
            }
            Err(e) => Some(e.text()),
        }
    }

    /// One RPC command; one deadline covers waiting for the pipe, the write and the answer. Ok(None) when the
    /// connection closed before an answer.
    async fn request(
        &self,
        cmd: &str,
        args: Option<Value>,
        evt: Option<&str>,
        wait: Duration,
    ) -> Answer {
        let writer = lock(&self.io)
            .clone()
            .ok_or_else(|| RpcError::Refused(NOT_CONNECTED.into()))?;
        let id = crate::control_deck::leases::nonce();
        let (answer, waiting) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), answer);
        let mut message =
            json!({ "cmd": cmd, "nonce": id, "args": args.unwrap_or_else(|| json!({})) });
        if let Some(evt) = evt {
            message["evt"] = json!(evt);
        }
        let work = async {
            {
                let mut w = writer.lock().await;
                ipc::write_frame(&mut *w, 1, &message)
                    .await
                    .map_err(|e| RpcError::Io(e.to_string()))?;
            }
            waiting.await.unwrap_or(Ok(None))
        };
        let result = tokio::time::timeout(wait, work)
            .await
            .unwrap_or(Err(RpcError::Timeout));
        lock(&self.pending).remove(&id);
        result
    }

    /// Reads frames until the connection ends (or `stop`): answers PINGs, keeps notifications, answers requests.
    /// At the end every waiting request gets "no answer" at once.
    pub(crate) async fn read_loop(
        &self,
        mut reader: impl AsyncRead + Unpin,
        writer: Writer,
        stop: Stop,
    ) {
        lock(&self.shared).closed = false;
        tokio::select! {
            _ = self.read_frames(&mut reader, &writer) => {}
            _ = stop.stopped() => {}
        }
        self.notes_wanted.store(false, Ordering::SeqCst);
        let enabled = self.cfg.get().discord.enabled;
        {
            let mut shared = lock(&self.shared);
            shared.closed = true; // a poll still finishing for this connection must not bring "connected" back
            shared.notes.clear();
            if shared.snap.connected {
                shared.snap = DiscordSnapshot::empty(enabled.then(|| NOT_RUNNING.to_string()));
            }
        }
        // the connection ended: whatever still waits gets "no answer" now (a pending login prompt must not time out
        // later as a refusal)
        for (_, waiting) in lock(&self.pending).drain() {
            let _ = waiting.send(Ok(None));
        }
    }

    async fn read_frames(&self, reader: &mut (impl AsyncRead + Unpin), writer: &Writer) {
        loop {
            let Ok(Some((op, message))) = ipc::read_frame(reader).await else {
                return;
            };
            if op == 3 {
                // PING: the same payload back, through the same lock as every other write
                let payload = match message {
                    Some(Value::Object(o)) => Value::Object(o),
                    _ => json!({}),
                };
                let mut w = writer.lock().await;
                if ipc::write_frame(&mut *w, 4, &payload).await.is_err() {
                    return;
                }
                continue;
            }
            let Some(Value::Object(obj)) = message else {
                continue;
            };
            if op == 4 {
                continue;
            }
            let evt = obj.get("evt").and_then(Value::as_str);
            if evt == Some("NOTIFICATION_CREATE") {
                let Some(d) = obj
                    .get("data")
                    .and_then(Value::as_object)
                    .filter(|_| self.notes_now())
                else {
                    continue;
                };
                let text = |v: Option<&Value>| v.and_then(Value::as_str).map(String::from);
                let title = text(d.get("title")).unwrap_or_default();
                let body = text(d.get("body"))
                    .or_else(|| text(d.get("message").and_then(|m| m.get("content"))))
                    .unwrap_or_default();
                let mut shared = lock(&self.shared);
                shared.notes.push_front(DiscordNotification {
                    ts: crate::server::now_ms(),
                    title,
                    body,
                });
                shared.notes.truncate(20);
                continue;
            }
            let Some(waiting) = obj
                .get("nonce")
                .and_then(Value::as_str)
                .and_then(|id| lock(&self.pending).remove(id))
            else {
                continue;
            };
            let data = obj.get("data");
            let _ = waiting.send(if evt == Some("ERROR") {
                Err(RpcError::Refused(
                    data.and_then(|d| d.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Discord refused the request")
                        .to_string(),
                ))
            } else {
                Ok(data.filter(|d| !d.is_null()).cloned())
            });
        }
    }

    /// Tests: a request waiting for an answer (C# set `_pending` directly).
    #[cfg(test)]
    pub(crate) fn wait_for(&self, nonce: &str) -> oneshot::Receiver<Answer> {
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(nonce.to_string(), tx);
        rx
    }

    /// An icon or avatar the current snapshot reported.
    pub fn is_known_image(&self, kind: &str, id: &str, hash: &str) -> bool {
        let shared = lock(&self.shared);
        match kind {
            "guild" => shared.snap.channels.iter().any(|c| {
                c.guild_id.as_deref() == Some(id) && c.guild_icon.as_deref() == Some(hash)
            }),
            "user" => shared
                .snap
                .members
                .iter()
                .any(|m| m.id == id && m.avatar.as_deref() == Some(hash)),
            _ => false,
        }
    }

    /// `GET /api/discord/img`: only for icons and avatars the agent reported, from Discord's CDN (64 px, ≤ 512 KiB).
    pub async fn image(&self, kind: &str, id: &str, hash: &str) -> Option<(Vec<u8>, &'static str)> {
        if !self.cfg.get().discord.enabled {
            return None;
        }
        let folder = match kind {
            "guild" => "icons",
            "user" => "avatars",
            _ => return None,
        };
        let id_ok = !id.is_empty() && id.len() <= 24 && id.bytes().all(|b| b.is_ascii_digit());
        let hash_ok = !hash.is_empty()
            && hash.len() <= 64
            && hash.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !id_ok || !hash_ok || !self.is_known_image(kind, id, hash) {
            return None;
        }
        let key = format!("{kind}/{id}/{hash}");
        if let Some(cached) = lock(&self.images).0.get(&key) {
            return Some(cached.clone());
        }
        let url = format!("https://cdn.discordapp.com/{folder}/{id}/{hash}.png?size=64");
        let mut res = self.images_http.get(url).send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > IMAGE_MAX {
                return None;
            }
        }
        let mime = crate::photo_frame::mime_of(&bytes).filter(|_| !bytes.is_empty())?;
        let mut images = lock(&self.images);
        if let Some(raced) = images.0.get(&key) {
            return Some(raced.clone());
        }
        if images.1 + bytes.len() > IMAGE_CACHE_LIMIT {
            images.0.clear();
            images.1 = 0;
        }
        images.1 += bytes.len();
        images.0.insert(key, (bytes.clone(), mime));
        Some((bytes, mime))
    }

    /// The connection loop, until `stop`.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            let mut refused: Option<(Option<String>, Option<String>)> = None;
            let mut last_error: Option<String> = None;
            let pause = |d: Duration| {
                let stop = stop.clone();
                async move {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => false,
                        _ = stop.stopped() => true,
                    }
                }
            };
            while !stop.is_stopped() {
                let d = me.cfg.get().discord.clone();
                if !d.enabled {
                    lock(&me.shared).snap = DiscordSnapshot::empty(None);
                    refused = None;
                    failures = 0;
                    if pause(Duration::from_secs(2)).await {
                        return;
                    }
                    continue;
                }
                // after a refused login: no new prompt until the client id or secret changes
                if refused
                    .as_ref()
                    .is_some_and(|r| *r == (d.client_id.clone(), d.client_secret.clone()))
                {
                    if pause(Duration::from_secs(2)).await {
                        return;
                    }
                    continue;
                }
                me.logged_in.store(false, Ordering::SeqCst);
                match me.run_connection(&stop).await {
                    Ok(()) => {}
                    Err(Ended::Refused(message)) => {
                        refused = Some((d.client_id.clone(), d.client_secret.clone()));
                        crate::log::info(CATEGORY, &format!("discord: {message}"));
                        lock(&me.shared).snap = DiscordSnapshot::empty(Some(message));
                        continue;
                    }
                    Err(ended) if !stop.is_stopped() => {
                        let message = match ended {
                            Ended::Failed(m) => m,
                            _ => NOT_RUNNING.to_string(),
                        };
                        if last_error.as_deref() != Some(message.as_str()) {
                            crate::log::info(CATEGORY, &format!("discord: {message}"));
                            last_error = Some(message.clone());
                        }
                        lock(&me.shared).snap = DiscordSnapshot::empty(Some(message));
                    }
                    Err(_) => return,
                }
                failures = if me.logged_in.load(Ordering::SeqCst) {
                    0
                } else {
                    failures + 1
                };
                if !me.cfg.get().discord.enabled {
                    lock(&me.shared).snap = DiscordSnapshot::empty(None);
                    refused = None;
                    failures = 0;
                    continue;
                }
                if pause(backoff(failures)).await {
                    return;
                }
            }
        })
    }

    async fn run_connection(self: &Arc<Self>, stop: &Stop) -> Result<(), Ended> {
        let d = self.cfg.get().discord.clone();
        let (Some(client_id), Some(_)) = (d.client_id.clone(), d.client_secret.clone()) else {
            return Err(Ended::Failed(
                "Enter the application's client ID and secret (Settings › General › Discord)"
                    .into(),
            ));
        };
        let io = ipc::open().await.map_err(|_| Ended::Dropped)?;
        let (mut reader, writer) = split(io);
        {
            let mut w = writer.lock().await;
            ipc::write_frame(&mut *w, 0, &json!({ "v": 1, "client_id": client_id }))
                .await
                .map_err(|_| Ended::Dropped)?;
        }
        let ready = match ipc::read_frame(&mut reader).await {
            Ok(Some((_, Some(ready)))) => ready,
            Ok(_) => return Err(Ended::Dropped), // "Discord closed the connection"
            Err(e) => return Err(Ended::Failed(e)),
        };
        if ready.get("evt").and_then(Value::as_str) != Some("READY") {
            let message = ready["data"]["message"]
                .as_str()
                .unwrap_or("Discord refused the client ID");
            return Err(Ended::Failed(message.to_string()));
        }
        *lock(&self.io) = Some(writer.clone());
        let reading = Stop::default();
        let reader_task = tokio::spawn({
            let (me, writer, reading) = (self.clone(), writer.clone(), reading.clone());
            async move { me.read_loop(reader, writer, reading).await }
        });
        lock(&self.shared).snap = DiscordSnapshot::empty(Some(
            "Waiting for you to approve PhoneBox in Discord…".into(),
        ));
        let result = self.logged_in_session(&reader_task, stop).await;
        *lock(&self.io) = None;
        self.notes_wanted.store(false, Ordering::SeqCst);
        reading.stop();
        let _ = reader_task.await;
        let enabled = self.cfg.get().discord.enabled;
        {
            let mut shared = lock(&self.shared);
            shared.notes.clear();
            if shared.snap.connected {
                shared.snap = DiscordSnapshot::empty(enabled.then(|| NOT_RUNNING.to_string()));
            }
        }
        for (_, waiting) in lock(&self.pending).drain() {
            let _ = waiting.send(Ok(None));
        }
        result
    }

    /// Logs in, then polls every 2 s while the reader runs and Discord stays enabled.
    async fn logged_in_session(
        &self,
        reader: &tokio::task::JoinHandle<()>,
        stop: &Stop,
    ) -> Result<(), Ended> {
        let user = self.authenticate().await?;
        self.logged_in.store(true, Ordering::SeqCst);
        crate::log::info(CATEGORY, "discord: connected");
        lock(&self.guild_channels).clear();
        let mut next_channels = Instant::now();
        while !stop.is_stopped() && !reader.is_finished() && self.cfg.get().discord.enabled {
            let want = notifications_wanted(&self.cfg.get());
            if want != self.notes_wanted.load(Ordering::SeqCst) {
                let cmd = if want { "SUBSCRIBE" } else { "UNSUBSCRIBE" };
                self.request(
                    cmd,
                    Some(json!({})),
                    Some("NOTIFICATION_CREATE"),
                    Duration::from_secs(5),
                )
                .await?;
                if !want {
                    lock(&self.shared).notes.clear();
                }
                self.notes_wanted.store(want, Ordering::SeqCst);
            }
            if Instant::now() >= next_channels {
                let complete = self.refresh_voice_channels().await;
                next_channels =
                    Instant::now() + Duration::from_secs(if complete { 60 } else { 10 });
            }
            let channels: Vec<DiscordChannel> = lock(&self.guild_channels)
                .values()
                .flatten()
                .cloned()
                .collect();
            let rev = self.voice_revision();
            let vs = self
                .request("GET_VOICE_SETTINGS", None, None, Duration::from_secs(5))
                .await?;
            let sel = self
                .request(
                    "GET_SELECTED_VOICE_CHANNEL",
                    None,
                    None,
                    Duration::from_secs(5),
                )
                .await?;
            let members = sel
                .as_ref()
                .and_then(|s| s["voice_states"].as_array())
                .map(|states| states.iter().map(member).collect())
                .unwrap_or_default();
            let kind = sel.as_ref().map(|s| match s["type"].as_i64() {
                Some(1) => "call",
                Some(3) => "group call",
                Some(13) => "stage",
                _ => "voice",
            });
            let name = sel.as_ref().map(|s| {
                s["name"].as_str().map(String::from).unwrap_or_else(|| {
                    if kind == Some("call") {
                        "Direct call".into()
                    } else {
                        "Call".into()
                    }
                })
            });
            let flag = |k: &str| vs.as_ref().and_then(|v| v[k].as_bool()) == Some(true);
            self.publish_poll(
                rev,
                flag("mute"),
                flag("deaf"),
                DiscordSnapshot {
                    connected: true,
                    user: Some(user.clone()),
                    channel_id: sel
                        .as_ref()
                        .and_then(|s| s["id"].as_str().map(String::from)),
                    channel_name: name,
                    channel_kind: kind.map(String::from),
                    members,
                    channels,
                    ..DiscordSnapshot::empty(None)
                },
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = stop.stopped() => {}
            }
        }
        Ok(())
    }

    /// A saved login (refreshed when it expires within 5 min), else the user approves PhoneBox in Discord (2 min).
    async fn authenticate(&self) -> Result<String, Ended> {
        let d = self.cfg.get().discord.clone();
        let client_id = d.client_id.clone().unwrap_or_default();
        let mut tokens = self.load_tokens(&client_id);
        if let Some(t) = &tokens
            && t.expires - now_s() < 300
        {
            tokens = self
                .exchange(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", t.refresh.as_str()),
                ])
                .await
                .ok()
                .flatten();
        }
        if let Some(t) = &tokens {
            match self.auth(&t.access).await {
                Ok(user) => return Ok(user),
                Err(e) => crate::log::info(
                    CATEGORY,
                    &format!("discord: saved login no longer works ({})", e.text()),
                ),
            }
        }
        let scopes: Vec<&str> = SCOPES.to_vec();
        let code = match self
            .request(
                "AUTHORIZE",
                Some(json!({ "client_id": client_id, "scopes": scopes })),
                None,
                Duration::from_secs(120),
            )
            .await
        {
            Ok(answer) => answer.and_then(|a| a["code"].as_str().map(String::from)),
            Err(RpcError::Timeout) => {
                return Err(Ended::Refused(format!(
                    "PhoneBox was not approved in Discord in time.{RETRY}"
                )));
            }
            Err(RpcError::Io(_)) => return Err(Ended::Dropped),
            Err(RpcError::Refused(m)) => {
                return Err(Ended::Refused(format!(
                    "Discord login was refused ({m}).{RETRY}"
                )));
            }
        };
        // no code: the connection closed (a dropped connection, not a refusal)
        let code = code.ok_or(Ended::Dropped)?;
        let tokens = match self
            .exchange(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
            ])
            .await
        {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Err(Ended::Refused(format!(
                    "Discord refused the client secret or the redirect (add http://127.0.0.1 under OAuth2 › Redirects).{RETRY}"
                )));
            }
            Err(e) => {
                return Err(Ended::Refused(format!(
                    "PhoneBox could not finish the Discord login ({e}).{RETRY}"
                )));
            }
        };
        match self.auth(&tokens.access).await {
            Ok(user) => Ok(user),
            Err(RpcError::Io(_)) | Err(RpcError::Timeout) => Err(Ended::Dropped),
            Err(e) => Err(Ended::Refused(format!(
                "Discord refused the new login ({}).{RETRY}",
                e.text()
            ))),
        }
    }

    /// `AUTHENTICATE`: the user's display name.
    async fn auth(&self, access: &str) -> Result<String, RpcError> {
        let answer = self
            .request(
                "AUTHENTICATE",
                Some(json!({ "access_token": access })),
                None,
                Duration::from_secs(5),
            )
            .await?
            .ok_or(RpcError::Io("Discord closed the connection".into()))?;
        let user = &answer["user"];
        Ok(user["global_name"]
            .as_str()
            .or_else(|| user["username"].as_str())
            .unwrap_or("Discord")
            .to_string())
    }

    fn load_tokens(&self, client_id: &str) -> Option<Tokens> {
        let text = std::fs::read_to_string(self.token_path()).ok()?;
        serde_json::from_str::<Tokens>(&text)
            .ok()
            .filter(|t| t.client_id == client_id)
    }

    /// The OAuth token service; Ok(None) when it refused (400/401) or gave no token. Saves the login.
    async fn exchange(&self, grant: &[(&str, &str)]) -> Result<Option<Tokens>, String> {
        let d = self.cfg.get().discord.clone();
        let (client_id, secret) = (
            d.client_id.unwrap_or_default(),
            d.client_secret.unwrap_or_default(),
        );
        let mut form: Vec<(&str, &str)> = grant.to_vec();
        form.push(("client_id", &client_id));
        form.push(("client_secret", &secret));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        let res = http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| crate::net::error_text(&e))?;
        let status = res.status().as_u16();
        if status == 400 || status == 401 {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(format!("Discord's token service answered HTTP {status}"));
        }
        let body = res.bytes().await.map_err(|e| crate::net::error_text(&e))?;
        let body: Value = serde_json::from_slice(&body)
            .map_err(|_| "Discord sent something unexpected".to_string())?;
        let tokens = Tokens {
            client_id: client_id.clone(),
            access: body["access_token"].as_str().unwrap_or("").to_string(),
            refresh: body["refresh_token"].as_str().unwrap_or("").to_string(),
            expires: now_s() + body["expires_in"].as_i64().unwrap_or(3600),
        };
        if tokens.access.is_empty() {
            return Ok(None);
        }
        let path = self.token_path();
        let saved = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                crate::app_paths::write_all_text_private(
                    &path,
                    &serde_json::to_string(&tokens).unwrap_or_default(),
                )
            });
        if saved.is_err() {
            crate::log::debug(CATEGORY, "discord: could not save the login");
        }
        Ok(Some(tokens))
    }

    /// The voice channels of the first 60 servers; false when the list may be incomplete (old lists are kept).
    async fn refresh_voice_channels(&self) -> bool {
        let wait = Duration::from_secs(5);
        let guilds = match self.request("GET_GUILDS", None, None, wait).await {
            Ok(answer) => answer
                .and_then(|a| a["guilds"].as_array().cloned())
                .unwrap_or_default(),
            Err(e) => {
                crate::log::debug(
                    CATEGORY,
                    &format!("discord: listing servers failed: {}", e.text()),
                );
                return false;
            }
        };
        crate::log::debug(
            CATEGORY,
            &format!("discord: {} servers reported", guilds.len()),
        );
        let mut complete = !guilds.is_empty();
        for g in guilds.iter().take(60) {
            let Some(guild_id) = g["id"].as_str() else {
                continue;
            };
            let guild_name = g["name"].as_str().map(String::from);
            let known = lock(&self.guild_icons).get(guild_id).cloned();
            let icon = match known {
                Some(icon) => icon,
                None => match self
                    .request(
                        "GET_GUILD",
                        Some(json!({ "guild_id": guild_id })),
                        None,
                        wait,
                    )
                    .await
                {
                    Ok(answer) => {
                        let icon = answer.and_then(|a| a["icon_url"].as_str().and_then(icon_hash));
                        lock(&self.guild_icons).insert(guild_id.to_string(), icon.clone());
                        icon
                    }
                    Err(e) => {
                        complete = false;
                        crate::log::debug(
                            CATEGORY,
                            &format!("discord: channels of {guild_id} failed: {}", e.text()),
                        );
                        continue;
                    }
                },
            };
            match self
                .request(
                    "GET_CHANNELS",
                    Some(json!({ "guild_id": guild_id })),
                    None,
                    wait,
                )
                .await
            {
                Ok(answer) => {
                    let list = answer
                        .and_then(|a| a["channels"].as_array().cloned())
                        .unwrap_or_default()
                        .iter()
                        .filter(|c| matches!(c["type"].as_i64(), Some(2 | 13)))
                        .filter_map(|c| {
                            let id = c["id"].as_str()?;
                            Some(DiscordChannel {
                                id: id.to_string(),
                                name: c["name"].as_str().unwrap_or(id).to_string(),
                                guild: guild_name.clone(),
                                guild_id: Some(guild_id.to_string()),
                                guild_icon: icon.clone(),
                            })
                        })
                        .collect();
                    lock(&self.guild_channels).insert(guild_id.to_string(), list);
                }
                Err(e) => {
                    complete = false;
                    crate::log::debug(
                        CATEGORY,
                        &format!("discord: channels of {guild_id} failed: {}", e.text()),
                    );
                }
            }
        }
        complete
    }
}

/// One member of the voice channel.
fn member(state: &Value) -> DiscordMember {
    let user = &state["user"];
    let vs = &state["voice_state"];
    let flag = |k: &str| vs[k].as_bool() == Some(true);
    DiscordMember {
        id: user["id"].as_str().unwrap_or("").to_string(),
        name: state["nick"]
            .as_str()
            .or_else(|| user["global_name"].as_str())
            .or_else(|| user["username"].as_str())
            .unwrap_or("?")
            .to_string(),
        muted: flag("mute") || flag("self_mute"),
        deafened: flag("deaf") || flag("self_deaf"),
        avatar: user["avatar"].as_str().map(String::from),
    }
}

/// The icon hash in a guild's icon URL (`/icons/<id>/<hash>.`).
fn icon_hash(url: &str) -> Option<String> {
    let rest = &url[url.find("/icons/")? + "/icons/".len()..];
    let slash = rest.find('/')?;
    if slash == 0 || !rest[..slash].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let after = &rest[slash + 1..];
    let hash: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    ((1..=64).contains(&hash.len()) && after[hash.len()..].starts_with('.')).then_some(hash)
}

fn now_s() -> i64 {
    crate::server::now_ms() / 1000
}

/// Splits a stream into the reader's half and the shared writer (requests and PONGs take turns on it).
pub(crate) fn split(io: Box<dyn Io>) -> (tokio::io::ReadHalf<Box<dyn Io>>, Writer) {
    let (reader, writer) = tokio::io::split(io);
    (reader, Arc::new(tokio::sync::Mutex::new(writer)))
}

#[cfg(test)]
mod tests;
