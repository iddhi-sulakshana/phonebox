//! The HTTP server (Kestrel in the C# agent): shared state, routes and the request pipeline of `Program.cs`.

pub mod guard;
pub mod query;
mod routes_charge;
mod routes_console;
mod routes_controls;
mod routes_desk;
mod routes_dev;
pub mod routes_info;
mod routes_integrations;
pub mod routes_pair;
mod routes_phones;
mod routes_push;
mod routes_rules;
pub mod routes_settings;
pub mod routes_state;
mod routes_system;
mod routes_widgets;
pub mod static_files;
#[cfg(test)]
mod tests_task10;
#[cfg(feature = "tls")]
pub mod tls;

use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

use crate::agent_endpoint::AgentEndpoint;
use crate::config::store::ConfigStore;
use crate::push_store::PushStore;
use crate::request_timings::RequestTimings;

/// The whole app: the `/agent/api` rewrite, then every route behind the timing and guard layers.
pub type App =
    tower::util::MapRequest<axum::Router, fn(axum::extract::Request) -> axum::extract::Request>;

pub fn build_router(state: AppState) -> App {
    let rewrite: fn(axum::extract::Request) -> axum::extract::Request = rewrite_agent_api;
    tower::util::MapRequest::new(routes(state), rewrite)
}

/// `/agent/api/...` becomes `/api/...` (`Program.cs`): the panel in the layout editor's live preview calls the API by
/// the phone's own path. It runs before routing, timing and the guard, so all of them see the new path (the guard
/// must: a cross-site navigation is allowed to pages, never to `/api`). Local port only: the Wi-Fi listener
/// does not apply it.
fn rewrite_agent_api(mut req: axum::extract::Request) -> axum::extract::Request {
    const PREFIX: &str = "/agent/api";
    let path = req.uri().path();
    if crate::browser_guard::starts_with_segments(path, PREFIX) {
        let rest = &path[PREFIX.len()..];
        let target = match req.uri().query() {
            Some(q) => format!("/api{rest}?{q}"),
            None => format!("/api{rest}"),
        };
        let mut parts = req.uri().clone().into_parts();
        if let Ok(pq) = target.parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    req
}

/// The Wi-Fi port's app: the same routes behind the Wi-Fi guard (the phone key, the webhook key or a pairing claim,
/// only on the allow-list), without the `/agent/api` rewrite.
pub fn build_wifi_router(state: AppState) -> axum::Router {
    guard::apply_wifi(all_routes(), state)
}

/// Which listener a request came in on, as a request extension the guards set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// 127.0.0.1: the phone's USB tunnel, the settings page, tools on this computer.
    Local,
    /// The Wi-Fi port (HTTPS).
    Wifi,
}

impl Via {
    /// The live stream's log label.
    pub fn label(self) -> &'static str {
        match self {
            Via::Local => "USB/local",
            Via::Wifi => "Wi-Fi",
        }
    }

    /// How the phone registry says the phone was seen.
    pub fn transport(self) -> &'static str {
        match self {
            Via::Local => "USB",
            Via::Wifi => "Wi-Fi",
        }
    }
}

fn routes(state: AppState) -> axum::Router {
    guard::apply_local(all_routes(), state)
}

fn all_routes() -> axum::Router<AppState> {
    axum::Router::new()
        .merge(static_files::routes())
        .merge(routes_settings::routes())
        .merge(routes_info::routes())
        .merge(routes_state::routes())
        .merge(routes_system::routes())
        .merge(routes_desk::routes())
        .merge(routes_pair::routes())
        .merge(routes_dev::routes())
        .merge(routes_push::routes())
        .merge(routes_integrations::routes())
        .merge(routes_widgets::routes())
        .merge(routes_phones::routes())
        .merge(routes_rules::routes())
        .merge(routes_charge::routes())
        .merge(routes_console::routes())
        .merge(routes_controls::routes())
        // Kestrel's default request body limit: a settings page with many pages still fits
        .layer(axum::extract::DefaultBodyLimit::max(30_000_000))
}

/// Serves until [`Stop::stop`]: the live streams end and the server drains its open connections for at most `grace`
/// (ASP.NET's shutdown timeout, 30 s by default), then lets go of the ones still open: a client that stopped reading
/// must not keep the agent from stopping or restarting.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let stop = state.stop.clone();
    let app = axum::ServiceExt::<axum::extract::Request>::into_make_service(build_router(state));
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let stop = stop.clone();
        async move { stop.stopped().await }
    });
    drain(server, &stop, grace).await
}

/// The Wi-Fi port (HTTPS with the pinned certificate), until [`Stop::stop`] as [`serve`].
#[cfg(feature = "tls")]
pub async fn serve_wifi(
    listener: tokio::net::TcpListener,
    config: Arc<rustls::ServerConfig>,
    state: AppState,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let listener = tls::TlsListener::new(listener, config, tls::HANDSHAKE_TIMEOUT)?;
    serve_wifi_on(listener, state, grace).await
}

#[cfg(feature = "tls")]
pub(crate) async fn serve_wifi_on(
    listener: tls::TlsListener,
    state: AppState,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let stop = state.stop.clone();
    let app = build_wifi_router(state).into_make_service();
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let stop = stop.clone();
        async move { stop.stopped().await }
    });
    drain(server, &stop, grace).await
}

/// Runs `server` (with its graceful shutdown on [`Stop`]) and gives up on the connections still open `grace` after
/// the stop.
async fn drain<F>(server: F, stop: &Stop, grace: std::time::Duration) -> std::io::Result<()>
where
    F: std::future::IntoFuture<Output = std::io::Result<()>>,
{
    tokio::select! {
        result = server.into_future() => result,
        _ = async { stop.stopped().await; tokio::time::sleep(grace).await } => {
            crate::log::warn("Program", "connections still open after the shutdown grace period were closed");
            Ok(())
        }
    }
}

/// Marks an answer that stands for an unhandled exception in the C# agent.
#[derive(Debug, Clone, Copy)]
pub struct Unhandled;

/// A C# unhandled exception's answer: 500 with no body and none of the middleware's headers (ASP.NET clears the
/// response before it writes the 500).
pub fn unhandled() -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    response.extensions_mut().insert(Unhandled);
    response
}

/// A JSON answer as ASP.NET writes one: `application/json; charset=utf-8`, compact, doubles in .NET's shortest form
/// (1.0 is written as 1).
pub fn json_response<T: serde::Serialize + ?Sized>(
    status: axum::http::StatusCode,
    value: &T,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let text = crate::config::write::to_dotnet_json_compact(value);
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ConfigStore>,
    pub endpoint: AgentEndpoint,
    pub push: Arc<PushStore>,
    pub timings: Arc<RequestTimings>,
    pub phone: Arc<PhoneSeenState>,
    pub adb: Arc<crate::adb::AdbReverse>,
    /// Microphone and camera use (the "On air" widget and rules); main shares it with the state sources.
    pub privacy: Arc<crate::privacy_monitor::PrivacyMonitor>,
    /// The programs of the deck's task and server actions (the deck's own).
    pub tasks: Arc<crate::control_deck::tasks::ControlTasks>,
    /// The radio-browser.info directory the settings page imports stations from.
    pub radio: Arc<crate::control_deck::radio_directory::RadioDirectory>,
    /// The Control Deck: the phone's controls page (audio, media, push to talk, profiles, power and platform actions).
    pub controls: Arc<crate::control_deck::core::ControlDeck>,
    /// The app console widget: sends a profile's keys to the app in front (Windows).
    pub console: Arc<crate::app_console::AppConsole>,
    /// Battery care: switches the phone's power supply by its battery level.
    pub charge: Arc<crate::charge_guard::ChargeGuard>,
    /// Automation rules; [`AppState::rewire`] builds it over the monitors in place.
    pub rules: Arc<crate::rule_engine::RuleEngine>,
    pub stop: Stop,
    /// The monitors behind `/api/state` and the live stream.
    pub sources: Arc<crate::state::StateSources>,
    /// The live stream's "state" frame, built once for all streams that wake on the same second.
    pub frames: Arc<crate::state_frame_cache::StateFrameCache>,
    /// The PC monitor (`pc` in the state); main starts it and puts the same one in `sources`.
    pub system: Arc<crate::system_monitor::SystemMonitor>,
    /// The ping monitor (`ping` in the state); main starts it and puts the same one in `sources`.
    pub ping: Arc<crate::ping_monitor::PingMonitor>,
    /// The process list (`/api/procs*`); main starts its sampling loop.
    pub procs: Arc<crate::process_monitor::ProcessMonitor>,
    /// Wireless device batteries (`/api/batteries`); main starts its loop.
    pub batteries: Arc<crate::battery_monitor::BatteryMonitor>,
    /// The output volume (`audio` in the state, `/api/volume`); main puts the same one in `sources`.
    pub audio: Arc<crate::audio_control::AudioControl>,
    /// Shortcut icons the user uploaded, in `icons/` next to the config file.
    pub icons: Arc<crate::shortcut_icon_uploads::ShortcutIconUploads>,
    /// Installed applications (the All apps widget, `/api/apps*`).
    pub apps: Arc<crate::app_catalog::AppCatalog>,
    /// The Dictation widget's typing (`/api/dictate`).
    pub dictation: Arc<crate::dictation::Dictation>,
    /// What is playing (`/api/media`, `/api/art`); main starts the platform's monitor and puts it here.
    pub media: Arc<dyn crate::media_monitor::SystemMedia>,
    /// The control deck's radio stations in front of the system media (plan 6; [`crate::media_monitor::NoDeck`] until
    /// then).
    pub deck: Arc<dyn crate::media_monitor::MediaDeck>,
    /// The Spectrum bars widget's band levels (`/api/spectrum`, the stream's "spectrum" events).
    pub spectrum: Arc<crate::spectrum_analyzer::SpectrumAnalyzer>,
    /// Wi-Fi as the agent started it: the startup port, the certificate when the Wi-Fi listener runs, the pairing
    /// codes (`/api/pair*`, `/api/lan`); main sets it up.
    pub wifi: Arc<crate::lan::Wifi>,
    /// The Claude Code, Codex and Cursor sessions (`/api/work`, the stream's "work" frame); main starts its loop.
    pub sessions: Arc<crate::session_monitor::SessionMonitor>,
    /// The session monitor's conversation cache (`/api/work/conversation`).
    pub transcripts: Arc<crate::claude_transcript::ClaudeTranscript>,
    /// Permission prompts waiting for the phone (`/api/approvals*`).
    pub approvals: Arc<crate::approval_store::ApprovalStore>,
    /// The Claude Code PermissionRequest hook (`/api/hooks/claude/permission`).
    pub hook: Arc<crate::claude_hook::ClaudeHook>,
    /// Claude Code's settings file, where `/api/claude/hook` adds and removes the hook.
    pub claude_settings: std::path::PathBuf,
    /// AI quotas (`/api/quotas`); main starts its loops.
    pub quotas: Arc<crate::quota::QuotaMonitor>,
    /// Pull requests and notifications through `gh` (`/api/work`); main starts its loop.
    pub github: Arc<crate::github_monitor::GitHubMonitor>,
    /// Docker / Podman containers (`/api/docker*`); main starts its loop.
    pub docker: Arc<crate::docker_monitor::DockerMonitor>,
    /// Command widgets' output (`/api/widget/...`).
    pub commands: Arc<crate::command_runner::CommandRunner>,
    /// WorkBuddy's workspace names (the permission card's project).
    pub work_names: Arc<crate::work_buddy::WorkBuddyNames>,
    /// Run after every successful settings save, before the page gets its answer.
    pub settings_hooks: Arc<crate::settings_hooks::SettingsHooks>,
    pub ha: Arc<crate::home_assistant::HomeAssistant>,
    pub obs: Arc<crate::obs_monitor::ObsMonitor>,
    pub discord: Arc<crate::discord_monitor::DiscordMonitor>,
    pub spotify: Arc<crate::spotify_control::SpotifyControl>,
    pub feeds: Arc<crate::feeds::FeedMonitor>,
    pub calendar: Arc<crate::calendar::CalendarMonitor>,
    pub photos: Arc<crate::photo_frame::PhotoFrame>,
    pub cameras: Arc<crate::camera_feeds::CameraFeeds>,
    /// `/api/proxy`'s client (a fake in tests).
    pub proxy: crate::widget_feed_proxy::ProxySend,
}

/// Serializes the writes that move the config version for shortcut icons (upload, removal) with the settings save.
/// Taken before the icon store's own lock, never inside it (its cleanup reads the settings), and never held across an
/// await.
pub static ICON_COMMIT: Mutex<()> = Mutex::new(());

impl AppState {
    pub fn new(
        cfg: Arc<ConfigStore>,
        endpoint: AgentEndpoint,
        push: Arc<PushStore>,
        sources: Arc<crate::state::StateSources>,
    ) -> AppState {
        let frames = {
            let (sources, cfg, push) = (sources.clone(), cfg.clone(), push.clone());
            let build = move || crate::state::build_state(&sources, &cfg, &push, None);
            Arc::new(crate::state_frame_cache::StateFrameCache::new(
                Box::new(build),
                100,
                None,
            ))
        };
        // not started here: main starts the loop (tests never run adb)
        let adb = crate::adb::AdbReverse::new(
            cfg.clone(),
            endpoint.port,
            Arc::new(crate::adb::ProcessRunner::default()),
        );
        let icons_dir = cfg
            .path()
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(crate::app_paths::config_dir)
            .join("icons");
        let wifi = Arc::new(crate::lan::Wifi::off(
            cfg.get().lan.port.clamp(1, 65535) as u16
        ));
        let home = crate::app_paths::home();
        let phone: Arc<PhoneSeenState> = Default::default();
        let sessions = crate::session_monitor::SessionMonitor::new(Some(cfg.clone()), home.clone());
        let approvals = crate::approval_store::ApprovalStore::new(push.clone(), phone.clone());
        let work_names = Arc::new(crate::work_buddy::WorkBuddyNames::new());
        let hook = claude_hook(&cfg, &approvals, &sessions, &work_names, &home);
        let photos = Arc::new(crate::photo_frame::PhotoFrame::new(
            cfg.clone(),
            &crate::app_paths::local_dir(),
        ));
        let settings_hooks: Arc<crate::settings_hooks::SettingsHooks> = Default::default();
        {
            // a cleared or changed photo folder takes its scaled copies and any scan in flight with it
            let photos = photos.clone();
            settings_hooks.add(Arc::new(move || photos.settings_saved()));
        }
        let cameras = Arc::new(crate::camera_feeds::CameraFeeds::new(
            cfg.clone(),
            crate::camera_feeds::real_launcher(),
        ));
        {
            // a camera whose address changed starts again, a removed one stops
            let cameras = cameras.clone();
            settings_hooks.add(Arc::new(move || cameras.settings_saved()));
        }
        let ha = Arc::new(crate::home_assistant::HomeAssistant::new(cfg.clone()));
        let audio = crate::audio_control::AudioControl::new();
        let controls = crate::control_deck::core::ControlDeck::new(
            cfg.clone(),
            Arc::new(crate::control_deck::core::NoNative),
            Default::default(),
        );
        let state = AppState {
            // replaced below, once the monitors it reads are in place
            rules: crate::rule_engine::RuleEngine::new(
                cfg.clone(),
                push.clone(),
                None,
                Arc::new(crate::rule_engine::NoInputs),
                crate::rule_engine::RuleClock::system(),
            ),
            privacy: crate::privacy_monitor::PrivacyMonitor::new(),
            photos,
            cameras,
            transcripts: sessions.transcripts(),
            procs: crate::process_monitor::ProcessMonitor::new(sessions.clone()),
            hook,
            claude_settings: crate::claude_hook::settings_path(&home),
            settings_hooks,
            console: Arc::new(crate::app_console::AppConsole::new(cfg.clone())),
            tasks: controls.tasks(),
            controls: controls.clone(),
            radio: Arc::new(crate::control_deck::radio_directory::RadioDirectory::new()),
            charge: Arc::new(crate::charge_guard::ChargeGuard::new(
                cfg.clone(),
                ha.clone(),
                push.clone(),
                Some(audio.clone()),
                Arc::new(crate::usb_doctor::last_report),
            )),
            ha,
            obs: Arc::new(crate::obs_monitor::ObsMonitor::new(cfg.clone())),
            discord: Arc::new(crate::discord_monitor::DiscordMonitor::new(cfg.clone())),
            feeds: Arc::new(crate::feeds::FeedMonitor::new(cfg.clone())),
            calendar: Arc::new(crate::calendar::CalendarMonitor::new(cfg.clone())),
            proxy: crate::widget_feed_proxy::real_send(),
            spotify: Arc::new(crate::spotify_control::SpotifyControl::new(
                cfg.clone(),
                endpoint.port,
                crate::spotify_control::spotify_http(),
            )),
            quotas: Arc::new(crate::quota::QuotaMonitor::new(
                Some(cfg.clone()),
                &crate::app_paths::local_dir(),
                &home,
            )),
            github: Arc::new(crate::github_monitor::GitHubMonitor::new()),
            docker: Arc::new(crate::docker_monitor::DockerMonitor::new(cfg.clone())),
            commands: Arc::new(crate::command_runner::CommandRunner::new(cfg.clone(), home)),
            sessions,
            approvals,
            work_names,
            phone,
            adb,
            wifi,
            apps: crate::app_catalog::AppCatalog::new(cfg.clone()),
            dictation: crate::dictation::Dictation::new(cfg.clone()),
            media: crate::media_monitor::MediaMonitor::new(),
            deck: controls.clone(),
            spectrum: crate::spectrum_analyzer::SpectrumAnalyzer::new(cfg.clone()),
            icons: Arc::new(crate::shortcut_icon_uploads::ShortcutIconUploads::new(
                icons_dir,
            )),
            system: crate::system_monitor::SystemMonitor::new(cfg.clone()),
            ping: crate::ping_monitor::PingMonitor::new(cfg.clone()),
            batteries: crate::battery_monitor::BatteryMonitor::new(),
            audio,
            cfg,
            endpoint,
            push,
            timings: Default::default(),
            stop: Stop::default(),
            sources,
            frames,
        };
        state.rewire()
    }
}

impl AppState {
    /// The rule engine and the charge guard over the monitors now in place (main calls it again after putting its own
    /// monitors in).
    pub fn rewire(mut self) -> AppState {
        self.charge = Arc::new(crate::charge_guard::ChargeGuard::new(
            self.cfg.clone(),
            self.ha.clone(),
            self.push.clone(),
            Some(self.audio.clone()),
            Arc::new(crate::usb_doctor::last_report),
        ));
        let inputs = Arc::new(crate::rule_engine::LiveInputs::of(&self));
        self.rules = crate::rule_engine::RuleEngine::new(
            self.cfg.clone(),
            self.push.clone(),
            Some(self.audio.clone()),
            inputs,
            crate::rule_engine::RuleClock::system(),
        );
        self
    }

    /// The Control Deck over the platform's native side, as main runs it: settings saves stop what was disabled
    /// (before the other hooks) and reconcile the stations (after them); `/api/media` and the Spotify routes take
    /// turns with it.
    pub fn with_controls(
        mut self,
        native: Arc<dyn crate::control_deck::models::DeckNative>,
    ) -> AppState {
        let controls = crate::control_deck::core::ControlDeck::new(
            self.cfg.clone(),
            native,
            crate::control_deck::core::DeckOptions {
                push: Some(self.push.clone()),
                master: Some(self.audio.clone()),
                spotify: Some(self.spotify.clone()),
                ..Default::default()
            },
        );
        {
            let deck = controls.clone();
            self.settings_hooks
                .add_first(Arc::new(move || deck.stop_disabled_controls()));
        }
        {
            let deck = controls.clone();
            self.settings_hooks
                .add(Arc::new(move || deck.reconcile_radios(true)));
        }
        self.tasks = controls.tasks();
        self.deck = controls.clone();
        self.controls = controls;
        self
    }

    /// The session and GitHub monitors main built first (the state sources' "work" frame reads them): the process list
    /// and the permission hook read the same session monitor.
    pub fn with_work(
        mut self,
        sessions: Arc<crate::session_monitor::SessionMonitor>,
        github: Arc<crate::github_monitor::GitHubMonitor>,
    ) -> AppState {
        let home = sessions.home().to_path_buf();
        self.procs = crate::process_monitor::ProcessMonitor::new(sessions.clone());
        self.hook = claude_hook(
            &self.cfg,
            &self.approvals,
            &sessions,
            &self.work_names,
            &home,
        );
        self.transcripts = sessions.transcripts();
        self.sessions = sessions;
        self.github = github;
        self
    }
}

fn claude_hook(
    cfg: &Arc<ConfigStore>,
    approvals: &Arc<crate::approval_store::ApprovalStore>,
    sessions: &Arc<crate::session_monitor::SessionMonitor>,
    names: &Arc<crate::work_buddy::WorkBuddyNames>,
    home: &std::path::Path,
) -> Arc<crate::claude_hook::ClaudeHook> {
    Arc::new(crate::claude_hook::ClaudeHook {
        cfg: cfg.clone(),
        approvals: approvals.clone(),
        sessions: sessions.clone(),
        names: names.clone(),
        home: home.to_path_buf(),
        idle: Arc::new(crate::idle::seconds),
    })
}

/// The agent is stopping (Ctrl+C, SIGTERM, or a restart from the settings page). Live streams end on it, so the
/// server's graceful shutdown can finish while a phone is connected.
#[derive(Clone)]
pub struct Stop(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Stop {
    fn default() -> Self {
        Stop(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl Stop {
    pub fn stop(&self) {
        self.0.send_replace(true);
    }
    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }
    /// Returns once [`Stop::stop`] was called (at once if it already was).
    pub async fn stopped(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What the request pipeline learns from the phone's headers.
#[derive(Default)]
pub struct PhoneSeenState {
    /// Unix ms of the phone's last `/api/state` or stream frame (`ApprovalStore.PhoneLastSeen`).
    pub last_seen: AtomicI64,
    /// The phone app's version (`UpdateService.AppVersion`), from X-PhoneBox-App.
    pub app_version: Mutex<Option<String>>,
}
