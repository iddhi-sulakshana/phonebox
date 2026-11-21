//! `SpotifyControl.cs`: the Control Deck's Spotify surface. PKCE and refresh credentials stay on this computer
//! (`<config>.spotify.json`); there is no browser login during polling. Every read is triggered by a request, with a
//! 5 s cache. Connects and disconnects advance a generation: answers to requests sent with an older one are neither
//! published nor allowed to restore a rate limit; settings changes are caught by comparing the client id and manual
//! token a read started with.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::store::ConfigStore;
use crate::control_deck::models::DeckResult;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const API: &str = "https://api.spotify.com/v1/";
const SCOPES: &str = "user-read-playback-state user-read-currently-playing user-modify-playback-state user-library-modify";
const RATE_LIMITED: &str = "Spotify is rate limited; try again later";
const UNAVAILABLE: &str = "Spotify is unavailable";

/// One request to Spotify's fixed `https://` hosts.
#[derive(Debug, Clone)]
pub struct SpotifyRequest {
    pub method: &'static str,
    pub url: String,
    pub bearer: Option<String>,
    pub content_type: Option<&'static str>,
    pub body: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpotifyReply {
    pub status: u16,
    /// The `Retry-After` header as sent.
    pub retry_after: Option<String>,
    pub body: String,
}

/// Spotify's HTTP: real, or a fake in tests. An Err is a transport failure ("Spotify is unavailable").
pub type SpotifyHttp =
    Arc<dyn Fn(SpotifyRequest) -> BoxFuture<'static, Result<SpotifyReply, String>> + Send + Sync>;

/// The real client: redirects are not followed (the timeout is the control's).
pub fn spotify_http() -> SpotifyHttp {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("the Spotify client");
    Arc::new(move |req: SpotifyRequest| {
        let client = client.clone();
        Box::pin(async move {
            let method =
                reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| e.to_string())?;
            let mut builder = client.request(method, &req.url);
            if let Some(bearer) = &req.bearer {
                builder = builder.bearer_auth(bearer);
            }
            if let Some(content_type) = req.content_type {
                builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            if let Some(body) = req.body {
                builder = builder.body(body);
            }
            let res = builder
                .send()
                .await
                .map_err(|e| crate::net::error_text(&e))?;
            let status = res.status().as_u16();
            let retry_after = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = res.text().await.map_err(|e| crate::net::error_text(&e))?;
            Ok(SpotifyReply {
                status,
                retry_after,
                body,
            })
        })
    })
}

/// The saved login (PascalCase, `Expires` in Unix ms; 0 = no expiry known). Access and refresh both empty is the
/// disconnect marker.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Token {
    client_id: String,
    access: String,
    refresh: String,
    expires: i64,
}

/// One sign-in waiting for Spotify's redirect.
struct Pending {
    state: String,
    verifier: String,
    client_id: String,
    expires: i64,
}

/// A published snapshot with the settings and credential generation it was read with.
struct Entry {
    value: Value,
    auth: (String, String),
    generation: i64,
    read_at: i64,
}

/// `_token` and `_tokenIsManual`, behind the token gate.
#[derive(Default)]
struct Tokens {
    token: Option<Token>,
    /// The token came from the manual token in settings, not from the credential file.
    manual: bool,
}

/// Why a Spotify call failed: a message for the phone (C#'s IOException), or "Spotify is unavailable".
#[derive(Debug, Clone, PartialEq)]
enum Fail {
    Io(String),
    Unavailable,
}

impl Fail {
    fn text(&self) -> String {
        match self {
            Fail::Io(m) => m.clone(),
            Fail::Unavailable => UNAVAILABLE.into(),
        }
    }
}

pub struct SpotifyControl {
    cfg: Arc<ConfigStore>,
    port: u16,
    http: SpotifyHttp,
    timeout: Duration,
    /// `_tokenGate` with what it guards (not held during API calls).
    tokens: tokio::sync::Mutex<Tokens>,
    read_gate: tokio::sync::Mutex<()>,
    /// `_gate`: the pending sign-in; also held while the generation advances or a snapshot is published.
    gate: Mutex<Option<Pending>>,
    snapshot: Mutex<Option<Arc<Entry>>>,
    generation: AtomicI64,
    retry_at: AtomicI64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> i64 {
    crate::server::now_ms()
}

const CATEGORY: &str = "PhoneBox.Agent.SpotifyControl";

/// base64url without padding.
fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the system random source");
    bytes
}

fn escape(text: &str) -> String {
    crate::net::escape_data_string(text)
}

/// An I/O failure as text, without Rust's "(os error N)".
fn io_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// `HasPendingState` with `_gate` held: a pending, unexpired login whose state is equal in constant time.
fn pending_matches(pending: &Option<Pending>, state: Option<&str>) -> bool {
    let (Some(state), Some(pending)) = (state.filter(|s| !s.is_empty()), pending.as_ref()) else {
        return false;
    };
    pending.expires > now() && fixed_time_eq(state.as_bytes(), pending.state.as_bytes())
}

/// `CryptographicOperations.FixedTimeEquals`: false at once for different lengths, else every byte is compared.
fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What the phone gets for a track or an episode: its URI, name, artist or show, and the smallest cover at least
/// 64 px wide from Spotify's own image server. Nothing else from Spotify's answer is passed on.
pub fn track_of(item: Option<&Value>) -> Value {
    let Some(item) = item.filter(|i| i.is_object()) else {
        return Value::Null;
    };
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    let artist = match item.get("artists").and_then(Value::as_array) {
        Some(artists) => artists
            .iter()
            .map(|a| text(a.get("name")))
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        None => text(item.get("show").and_then(|s| s.get("name"))),
    };
    // album.images when present (even if it is not a list), else the item's own
    let images = item
        .get("album")
        .and_then(|a| a.get("images"))
        .filter(|v| !v.is_null())
        .or_else(|| item.get("images"))
        .and_then(Value::as_array);
    let cover = images
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let url = i.get("url").and_then(Value::as_str);
            let width = i
                .get("width")
                .and_then(Value::as_i64)
                .and_then(|w| i32::try_from(w).ok())
                .unwrap_or(0);
            cover_url(url).then(|| (url.unwrap_or(""), width))
        })
        .min_by_key(|(_, width)| if *width >= 64 { *width } else { i32::MAX })
        .map(|(url, _)| url.to_string());
    let field = |k: &str| item.get(k).cloned().unwrap_or(Value::Null);
    json!({ "id": field("uri"), "name": field("name"), "artist": artist, "image": cover })
}

/// Only covers on Spotify's image server reach the phone, which loads them itself.
pub fn cover_url(url: Option<&str>) -> bool {
    url.and_then(|u| u.strip_prefix("https://i.scdn.co/image/"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        && url.is_some_and(|u| u.len() <= 200)
}

/// `spotify:(track|episode|playlist|album):` and 22 letters or digits.
pub fn playback_uri(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("spotify:") else {
        return false;
    };
    ["track:", "episode:", "playlist:", "album:"]
        .iter()
        .any(|kind| {
            rest.strip_prefix(kind)
                .is_some_and(|id| id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        })
}

/// The token service's answer: `access_token` (required), `refresh_token` when a non-empty string (else the old
/// one), `expires_in` clamped to 30..=86400 s (3600 when not an int).
fn token_of(text: &str, client: &str, old_refresh: &str, now: i64) -> Result<Token, Fail> {
    let root: Value = serde_json::from_str(text).map_err(|_| Fail::Unavailable)?;
    let access = root
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|a| !a.trim().is_empty())
        .ok_or_else(|| Fail::Io("Spotify returned no access token".into()))?;
    let refresh = root
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .unwrap_or(old_refresh);
    let seconds = root
        .get("expires_in")
        .and_then(Value::as_i64)
        .and_then(|s| i32::try_from(s).ok())
        .map_or(3600, |s| i64::from(s.clamp(30, 86400)));
    Ok(Token {
        client_id: client.to_string(),
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: now + seconds * 1000,
    })
}

/// A 429's wait: `Retry-After` in whole seconds clamped to 1..=300 s; 30 s when absent or not delta-seconds.
fn retry_after_ms(header: Option<&str>) -> i64 {
    let seconds = header
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<i32>().ok());
    seconds
        .map_or(30_000, |s| i64::from(s) * 1000)
        .clamp(1_000, 300_000)
}

impl SpotifyControl {
    pub fn new(cfg: Arc<ConfigStore>, port: u16, http: SpotifyHttp) -> SpotifyControl {
        SpotifyControl {
            cfg,
            port,
            http,
            timeout: Duration::from_secs(8),
            tokens: tokio::sync::Mutex::new(Tokens::default()),
            read_gate: tokio::sync::Mutex::new(()),
            gate: Mutex::new(None),
            snapshot: Mutex::new(None),
            generation: AtomicI64::new(0),
            retry_at: AtomicI64::new(0),
        }
    }

    /// The deadline for one HTTP exchange (C#'s `HttpClient.Timeout`, 8 s).
    pub fn with_timeout(mut self, timeout: Duration) -> SpotifyControl {
        self.timeout = timeout;
        self
    }

    pub fn redirect_uri(&self) -> String {
        format!(
            "http://127.0.0.1:{}/api/controls/spotify/callback",
            self.port
        )
    }

    fn credential_path(&self) -> PathBuf {
        let mut path = self.cfg.path().as_os_str().to_owned();
        path.push(".spotify.json");
        PathBuf::from(path)
    }

    /// The sign-in link; a new pending login replaces any earlier one. Err: no usable client id.
    pub fn begin(&self) -> Result<String, String> {
        let client = self
            .cfg
            .get()
            .controls
            .spotify_client_id
            .clone()
            .unwrap_or_default();
        if client.trim().is_empty() || client.encode_utf16().count() > 128 {
            return Err("Save a Spotify Client ID in desktop settings first".into());
        }
        let pending = Pending {
            state: base64url(&random::<32>()),
            verifier: base64url(&random::<64>()),
            client_id: client.clone(),
            expires: now() + 600_000,
        };
        let challenge = {
            use sha2::Digest;
            base64url(&sha2::Sha256::digest(pending.verifier.as_bytes()))
        };
        let state = pending.state.clone();
        *lock(&self.gate) = Some(pending);
        let redirect = self.redirect_uri();
        let query = [
            ("client_id", client.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("state", state.as_str()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
            ("scope", SCOPES),
        ];
        let query: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", escape(k), escape(v)))
            .collect();
        Ok(format!(
            "https://accounts.spotify.com/authorize?{}",
            query.join("&")
        ))
    }

    /// A pending, unexpired login with this state (compared in constant time).
    pub fn has_pending_state(&self, state: Option<&str>) -> bool {
        pending_matches(&lock(&self.gate), state)
    }

    /// Spotify's redirect: exchanges the code once. Ok(false) when the state or code does not fit; Err when the
    /// exchange failed.
    pub async fn complete(&self, state: &str, code: &str) -> Result<bool, String> {
        let pending = {
            let mut gate = lock(&self.gate);
            let code_len = code.encode_utf16().count();
            if !pending_matches(&gate, Some(state)) || code_len == 0 || code_len > 4096 {
                return Ok(false); // the pending login stays
            }
            gate.take()
        };
        let Some(pending) = pending else {
            return Ok(false);
        };
        if self.cfg.get().controls.spotify_client_id.as_deref() != Some(pending.client_id.as_str())
        {
            return Ok(false);
        }
        let mut tokens = self.tokens.lock().await;
        let redirect = self.redirect_uri();
        let token = self
            .token_request(
                &[
                    ("client_id", pending.client_id.as_str()),
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", redirect.as_str()),
                    ("code_verifier", pending.verifier.as_str()),
                ],
                &pending.client_id,
                "",
            )
            .await
            .map_err(|e| e.text())?;
        self.save(&mut tokens, token).map_err(|e| e.text())?;
        self.new_generation();
        Ok(true)
    }

    /// The account changed: drop the snapshot and the rate limit that belong to the previous one.
    fn new_generation(&self) {
        let _gate = lock(&self.gate);
        self.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.snapshot) = None;
        self.retry_at.store(0, Ordering::SeqCst);
    }

    /// One exchange with the deadline; an Err is "Spotify is unavailable".
    async fn send(&self, req: SpotifyRequest) -> Result<SpotifyReply, Fail> {
        match tokio::time::timeout(self.timeout, (self.http)(req)).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(e)) => {
                crate::log::debug(CATEGORY, &format!("spotify: {e}"));
                Err(Fail::Unavailable)
            }
            Err(_) => Err(Fail::Unavailable),
        }
    }

    async fn token_request(
        &self,
        fields: &[(&str, &str)],
        client: &str,
        old_refresh: &str,
    ) -> Result<Token, Fail> {
        // FormUrlEncodedContent: EscapeDataString with spaces as '+'
        let body: Vec<String> = fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    escape(k).replace("%20", "+"),
                    escape(v).replace("%20", "+")
                )
            })
            .collect();
        let reply = self
            .send(SpotifyRequest {
                method: "POST",
                url: TOKEN_URL.into(),
                bearer: None,
                content_type: Some("application/x-www-form-urlencoded"),
                body: Some(body.join("&")),
            })
            .await?;
        if !(200..300).contains(&reply.status) {
            return Err(Fail::Io(
                "Spotify authorization failed; reconnect from desktop settings".into(),
            ));
        }
        token_of(&reply.body, client, old_refresh, now())
    }

    /// Writes the credential file atomically (0600 on macOS/Linux) and keeps the token in memory.
    fn save(&self, tokens: &mut Tokens, token: Token) -> Result<(), Fail> {
        let path = self.credential_path();
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(format!(".{}.tmp", crate::control_deck::leases::nonce()));
        let temporary = PathBuf::from(temporary);
        let written = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = crate::app_paths::create_private(&temporary)?;
            std::io::Write::write_all(
                &mut file,
                serde_json::to_string(&token).unwrap_or_default().as_bytes(),
            )?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, &path)
        })();
        let _ = std::fs::remove_file(&temporary);
        written.map_err(|e| Fail::Io(io_text(&e)))?;
        tokens.token = Some(token);
        tokens.manual = false;
        Ok(())
    }

    /// The access token to use: the saved login (refreshed when it has less than 60 s left, or when `force`), else
    /// the manual token from settings. None: not connected (the disconnect marker also turns the manual token off).
    async fn access(&self, force: bool) -> Result<Option<String>, Fail> {
        let mut t = self.tokens.lock().await;
        let cfg = self.cfg.get();
        let client = cfg.controls.spotify_client_id.clone().unwrap_or_default();
        let manual = cfg.controls.spotify_token.clone().unwrap_or_default();
        if t.token.is_none() {
            match std::fs::read_to_string(self.credential_path()) {
                Ok(text) => {
                    if let Ok(Some(saved)) = serde_json::from_str::<Option<Token>>(&text) {
                        t.token = Some(saved);
                        t.manual = false;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Fail::Io(io_text(&e))),
            }
        }
        if t.token.as_ref().is_some_and(|tok| tok.client_id != client) {
            *t = Tokens::default(); // memory only: the file stays
        }
        // a token from the manual setting follows it; a saved disconnect is not a manual token
        if t.manual && t.token.as_ref().is_some_and(|tok| tok.access != manual) {
            *t = Tokens::default();
        }
        if t.token.is_none() && !manual.is_empty() {
            t.token = Some(Token {
                client_id: client.clone(),
                access: manual.clone(),
                refresh: cfg
                    .controls
                    .spotify_refresh_token
                    .clone()
                    .unwrap_or_default(),
                expires: 0,
            });
            t.manual = true;
        }
        let Some(token) = t.token.clone() else {
            return Ok(None);
        };
        if token.access.is_empty() && token.refresh.is_empty() {
            return Ok(None);
        }
        if !force && (token.expires == 0 || token.expires - now() > 60_000) {
            return Ok(Some(token.access));
        }
        if token.refresh.is_empty() || client.is_empty() {
            return Ok(None);
        }
        let refreshed = self
            .token_request(
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", token.refresh.as_str()),
                    ("client_id", client.as_str()),
                ],
                &client,
                &token.refresh,
            )
            .await?;
        let access = refreshed.access.clone();
        self.save(&mut t, refreshed)?;
        Ok(Some(access))
    }

    /// One Web API call: a 401 is retried once with a refreshed token; a 429 holds every call for `Retry-After`.
    async fn request(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, Fail> {
        if self.retry_at.load(Ordering::SeqCst) > now() {
            return Err(Fail::Io(RATE_LIMITED.into()));
        }
        for attempt in 0..2 {
            let access = self
                .access(attempt > 0)
                .await?
                .ok_or_else(|| Fail::Io("Connect Spotify in desktop settings".into()))?;
            let generation = self.generation.load(Ordering::SeqCst);
            let reply = self
                .send(SpotifyRequest {
                    method,
                    url: format!("{API}{path}"),
                    bearer: Some(access),
                    content_type: body.as_ref().map(|_| "application/json; charset=utf-8"),
                    body: body.as_ref().map(Value::to_string),
                })
                .await?;
            if reply.status == 401 && attempt == 0 {
                continue;
            }
            if reply.status == 429 {
                // a limit on the previous account must not block the one connected since this request was sent
                let until = now() + retry_after_ms(reply.retry_after.as_deref());
                let _gate = lock(&self.gate);
                if self.generation.load(Ordering::SeqCst) == generation {
                    self.retry_at.store(until, Ordering::SeqCst);
                }
                return Err(Fail::Io(RATE_LIMITED.into()));
            }
            if !(200..300).contains(&reply.status) {
                return Err(Fail::Io(format!(
                    "Spotify returned HTTP {}; check account permissions and the active player",
                    reply.status
                )));
            }
            if reply.status == 204 || reply.body.trim().is_empty() {
                return Ok(None);
            }
            return serde_json::from_str(&reply.body)
                .map(Some)
                .map_err(|_| Fail::Unavailable);
        }
        Err(Fail::Io("Reconnect Spotify in desktop settings".into()))
    }

    fn auth(&self) -> (String, String) {
        let cfg = self.cfg.get();
        (
            cfg.controls.spotify_client_id.clone().unwrap_or_default(),
            cfg.controls.spotify_token.clone().unwrap_or_default(),
        )
    }

    /// The published snapshot if it is fresh and was read with these settings and the current account.
    fn cached(&self, auth: &(String, String)) -> Option<Value> {
        // one reference for the check and the value, so a concurrent publish cannot pair two accounts
        let entry = lock(&self.snapshot).clone()?;
        (entry.auth == *auth
            && entry.generation == self.generation.load(Ordering::SeqCst)
            && now() - entry.read_at < 5000)
            .then(|| entry.value.clone())
    }

    /// What is playing and the first 30 queued items (episodes too: without additional_types an episode that is
    /// playing comes back empty).
    async fn read_state(&self) -> Result<Value, Fail> {
        let playing = self
            .request(
                "GET",
                "me/player/currently-playing?additional_types=track,episode",
                None,
            )
            .await?;
        let queue = self
            .request(
                "GET",
                "me/player/queue?additional_types=track,episode",
                None,
            )
            .await?;
        let queue: Vec<Value> = queue
            .as_ref()
            .and_then(|q| q.get("queue"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(30)
            .map(|item| track_of(Some(item)))
            .collect();
        let field = |k: &str| playing.as_ref().and_then(|p| p.get(k));
        Ok(json!({
            "connected": true,
            "current": track_of(field("item")),
            "playing": field("is_playing").cloned().unwrap_or(Value::Null),
            "queue": queue,
        }))
    }

    /// Forgets the pending login and saves the disconnect marker (which also turns off the manual token fallback).
    pub async fn disconnect(&self) -> Result<(), String> {
        let mut tokens = self.tokens.lock().await;
        *lock(&self.gate) = None;
        let client = self
            .cfg
            .get()
            .controls
            .spotify_client_id
            .clone()
            .unwrap_or_default();
        self.save(
            &mut tokens,
            Token {
                client_id: client,
                ..Token::default()
            },
        )
        .map_err(|e| e.text())?;
        self.new_generation();
        Ok(())
    }

    /// `GET /api/controls/spotify/state`: what is playing and the first 30 queued items, or the error.
    pub async fn snapshot(&self) -> Value {
        if let Some(fast) = self.cached(&self.auth()) {
            return fast;
        }
        let _one_read = self.read_gate.lock().await;
        // both requests must describe one account: a connect, disconnect or settings change while they run discards
        // the result; read once more, then report the change instead of mixing the two
        for attempt in 0.. {
            let auth = self.auth();
            if let Some(cached) = self.cached(&auth) {
                return cached;
            }
            let generation = self.generation.load(Ordering::SeqCst);
            let value = self
                .read_state()
                .await
                .unwrap_or_else(|e| json!({ "connected": false, "error": e.text() }));
            let unchanged = self.auth() == auth;
            {
                let _gate = lock(&self.gate);
                if unchanged && self.generation.load(Ordering::SeqCst) == generation {
                    *lock(&self.snapshot) = Some(Arc::new(Entry {
                        value: value.clone(),
                        auth,
                        generation,
                        read_at: now(),
                    }));
                    return value;
                }
            }
            if attempt > 0 {
                break;
            }
        }
        json!({ "connected": false, "error": "The Spotify account changed; try again" })
    }

    /// `favorite`, `play-queued` (only items the snapshot listed) and `playlist` (a configured deck action).
    pub async fn act(&self, action: &str, id: Option<&str>) -> DeckResult {
        match self.act_once(action, id).await {
            Ok(None) => {
                *lock(&self.snapshot) = None;
                DeckResult::ok()
            }
            Ok(Some(refused)) => DeckResult::fail(refused),
            Err(e) => DeckResult::fail(e.text()),
        }
    }

    /// Ok(Some(text)) when the control is refused before reaching Spotify.
    async fn act_once(&self, action: &str, id: Option<&str>) -> Result<Option<&'static str>, Fail> {
        match action {
            "favorite" => {
                let state = self.snapshot().await;
                let Some(uri) = state["current"]["id"].as_str().filter(|u| playback_uri(u)) else {
                    return Ok(Some("No Spotify item is playing"));
                };
                self.request("PUT", &format!("me/library?uris={}", escape(uri)), None)
                    .await?;
            }
            "play-queued" => {
                let state = self.snapshot().await;
                let listed = |id: &str| {
                    state["queue"]
                        .as_array()
                        .is_some_and(|q| q.iter().any(|t| t["id"].as_str() == Some(id)))
                };
                let Some(id) = id.filter(|id| playback_uri(id) && listed(id)) else {
                    return Ok(Some("That queue item is no longer available"));
                };
                let path = format!("me/player/play{}", self.device_query());
                self.request("PUT", &path, Some(json!({ "uris": [id] })))
                    .await?;
            }
            "playlist" => {
                const UNSET: &str = "Configure a Spotify playlist URI on the computer";
                let cfg = self.cfg.get();
                let Some(configured) = cfg
                    .controls
                    .actions
                    .iter()
                    .find(|a| Some(a.id.as_str()) == id && a.kind == "playlist")
                else {
                    return Ok(Some(UNSET));
                };
                if !configured.enabled {
                    return Ok(Some("Control is disabled"));
                }
                let target = configured.target.as_str();
                if !playback_uri(target) {
                    return Ok(Some(UNSET));
                }
                let body = if target.starts_with("spotify:track:")
                    || target.starts_with("spotify:episode:")
                {
                    json!({ "uris": [target] })
                } else {
                    json!({ "context_uri": target })
                };
                let path = format!("me/player/play{}", self.device_query());
                self.request("PUT", &path, Some(body)).await?;
            }
            _ => return Ok(Some("Unknown Spotify control")),
        }
        Ok(None)
    }

    fn device_query(&self) -> String {
        match self.cfg.get().controls.spotify_device.as_deref() {
            Some(id) if !id.is_empty() => format!("?device_id={}", escape(id)),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
#[path = "spotify_control_tests.rs"]
mod tests;
