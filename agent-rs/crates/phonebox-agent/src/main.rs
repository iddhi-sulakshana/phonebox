//! Entry point of `PhoneBoxAgent`; all behavior lives in the `phonebox` library crate. The order follows the C#
//! agent's `Program.cs`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use phonebox::agent_endpoint::AgentEndpoint;
use phonebox::config::store::ConfigStore;
use phonebox::server::AppState;
use phonebox::{app_paths, app_restart, log, push_store::PushStore, single_instance};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = match AgentEndpoint::from_args(&args) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // install scripts ask where the agent belongs (it differs per OS), then copy it there
    if args.iter().any(|a| a == "--print-install-dir") {
        println!("{}", app_paths::install_dir().display());
        return ExitCode::SUCCESS;
    }
    // single instance: a second launch opens the running instance's settings page and exits; after a restart the
    // previous instance may still be shutting down, so wait for it briefly
    let wait = if args.iter().any(|a| a == "--restart") {
        Duration::from_secs(15)
    } else {
        Duration::ZERO
    };
    let Some(_instance) =
        single_instance::acquire(&app_paths::local_dir(), &endpoint.mutex_name(), wait)
    else {
        open_in_browser(&endpoint.settings_url());
        return ExitCode::SUCCESS;
    };

    let custom = args.iter().skip_while(|a| *a != "--config").nth(1).cloned();
    if custom.is_none() {
        let _ = app_paths::ensure_config();
    }
    // absolute from here on: files kept next to the config need a folder
    let config_path = custom
        .map(std::path::PathBuf::from)
        .unwrap_or_else(app_paths::config_path);
    let config_path = std::path::absolute(&config_path).unwrap_or(config_path);
    app_paths::restrict_config(&config_path);
    let cfg = Arc::new(ConfigStore::new(config_path.clone()));
    cfg.persist_generated(); // keep the Wi-Fi key stable across restarts
    let _ = log::init(&app_paths::log_path());
    // Wi-Fi / Tailscale: HTTPS with the pinned certificate kept next to the config (made on first use); only when Wi-Fi
    // is on with its own port at startup. A certificate that cannot be written ends the agent, as in C#.
    let config_dir = config_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(app_paths::config_dir);
    let wifi = match phonebox::lan::Wifi::start(&cfg.get().lan, endpoint.port, &config_dir) {
        Ok(wifi) => wifi,
        Err(e) => {
            let message = format!("the Wi-Fi certificate could not be written: {e}");
            log::warn("PhoneBoxAgent", &message);
            log::dispose();
            return ExitCode::FAILURE;
        }
    };

    let push = PushStore::new(&app_paths::config_dir().join("data.json"));
    // the monitors the state reads; serve starts them
    let system = phonebox::system_monitor::SystemMonitor::new(cfg.clone());
    let ping = phonebox::ping_monitor::PingMonitor::new(cfg.clone());
    let audio = phonebox::audio_control::AudioControl::new();
    let media = phonebox::media_monitor::MediaMonitor::new();
    // the state's media reads the control deck, which comes with the state
    let deck = Arc::new(phonebox::media_monitor::DeckSlot::default());
    // the "work" frame, the process list and the permission hook read the same session monitor
    let sessions =
        phonebox::session_monitor::SessionMonitor::new(Some(cfg.clone()), app_paths::home());
    let github = Arc::new(phonebox::github_monitor::GitHubMonitor::new());
    let obs = Arc::new(phonebox::obs_monitor::ObsMonitor::new(cfg.clone()));
    let privacy = phonebox::privacy_monitor::PrivacyMonitor::new();
    let rules = Arc::new(phonebox::rule_engine::RulesSlot::default());
    let sources = phonebox::state::StateSources::phase1()
        .with_pc(Box::new(system.clone()))
        .with_ping(Box::new(ping.clone()))
        .with_audio(Box::new(audio.clone()))
        .with_media(Box::new(phonebox::media_monitor::MediaState {
            media: media.clone(),
            deck: deck.clone(),
        }))
        .with_toggles(Box::new(phonebox::shortcuts::Toggles {
            cfg: cfg.clone(),
            push: push.clone(),
            audio: audio.clone(),
        }))
        .with_obs(Box::new(obs.clone()))
        .with_privacy(Box::new(phonebox::privacy_monitor::PrivacyState(
            privacy.clone(),
        )))
        .with_rules(
            Box::new(phonebox::rule_engine::ActiveRules(rules.clone())),
            Box::new(phonebox::rule_engine::RulesPaused(rules.clone())),
        )
        .with_work(Box::new(phonebox::session_monitor::WorkFeed {
            sessions: sessions.clone(),
            github: github.clone(),
        }));
    let mut state =
        AppState::new(cfg, endpoint, push.clone(), Arc::new(sources)).with_work(sessions, github);
    state.system = system;
    state.ping = ping;
    state.audio = audio;
    state.obs = obs;
    state.privacy = privacy;
    state.wifi = Arc::new(wifi);
    // started here, on its own thread: the server never stops it before the process ends
    media.start(state.stop.clone());
    state.media = media;
    // the rules and the charge guard use the monitors put in above; /api/state reads the same engine
    // the deck plays sounds and stations with the platform's players and reads the same volume and media
    let native = phonebox::control_deck::platform_native(state.audio.clone(), state.media.clone());
    let state = state.rewire().with_controls(native);
    rules.set(state.rules.clone());
    deck.set(state.controls.clone());
    let controls = state.controls.clone();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(serve(state, config_path));
    // push-to-talk holds end (microphones restored) and the deck's task and server programs end with the agent (at
    // most 8 s)
    controls.dispose();
    push.dispose();
    log::dispose();
    drop(_instance); // the next instance takes over the lock
    if let Err(e) = result {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    if app_restart::requested() {
        // launchd / systemd start a new instance themselves; otherwise hand over to a fresh copy of this program
        if app_restart::service_managed(
            |k| std::env::var(k).ok(),
            std::process::id(),
            app_restart::parent_name,
        ) {
            return ExitCode::from(app_restart::SERVICE_RESTART_CODE);
        }
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(&exe)
                .args(app_restart::restart_args(&args))
                .spawn();
        }
    }
    ExitCode::SUCCESS
}

async fn serve(state: AppState, config_path: std::path::PathBuf) -> std::io::Result<()> {
    let port = state.endpoint.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    // Wi-Fi / Tailscale: HTTPS on every IPv4 address with the pinned certificate; a port that cannot be bound ends the
    // agent, as in C#
    #[cfg(feature = "tls")]
    let wifi = match &state.wifi.cert {
        Some(cert) => {
            let config =
                phonebox::server::tls::server_config(cert).map_err(std::io::Error::other)?;
            let tcp = tokio::net::TcpListener::bind(("0.0.0.0", state.wifi.port)).await?;
            Some((tcp, config))
        }
        None => None,
    };
    #[cfg(not(feature = "tls"))]
    if state.wifi.cert.is_some() {
        log::warn("PhoneBoxAgent", "this build has no TLS: Wi-Fi stays off");
    }
    state.adb.start(); // the USB tunnel to the phone
    state.system.start(state.stop.clone());
    state.ping.start(state.stop.clone());
    state.procs.start(state.stop.clone());
    state.batteries.start(state.stop.clone());
    state.sessions.start(state.stop.clone()); // every 2 s
    state.github.start(state.stop.clone()); // after 3 s, then every 3 min
    state.quotas.start(state.stop.clone()); // usage every 30 s, the overview every 5 s
    state.docker.start(state.stop.clone()); // only while the phone asks
    state.obs.start(state.stop.clone()); // one connection while OBS is enabled, polled every 2 s
    state.discord.start(state.stop.clone()); // Discord's local RPC pipe while Discord is enabled, polled every 2 s
    state.feeds.start(state.stop.clone()); // every 5 s; each feed at most every 15 min, only while a Feeds widget shows
    state.calendar.start(state.stop.clone()); // at once, then every 10 min, on calendar changes and on /api/refresh
    state.cameras.start(state.stop.clone()); // every second: stops cameras off screen, restarts stalled ffmpegs
    state.privacy.start(state.stop.clone()); // every 2 s (macOS: unknown)
    state.rules.start(state.stop.clone()); // after 3 s, then every second
    state.charge.start(state.stop.clone()); // after 5 s, then every 30 s
    state.controls.start(state.stop.clone()); // every 250 ms: panic mutes, holds, stations, sleep timers, ducking
    // every second: the phones' screens follow the Mac's lock while display.followComputerLock is on
    phonebox::session_lock_monitor::start(state.cfg.clone(), state.adb.clone(), state.stop.clone());
    log::info(
        "PhoneBoxAgent",
        &format!(
            "PhoneBox agent {} ready on http://127.0.0.1:{port} (config: {})",
            app_paths::version(),
            config_path.display()
        ),
    );
    // Ctrl+C / SIGTERM stop the agent like a restart request does: live streams end, then the server drains
    let stop = state.stop.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        stop.stop();
    });
    let grace = Duration::from_secs(30);
    #[cfg(feature = "tls")]
    if let Some((tcp, config)) = wifi {
        log::info(
            "PhoneBoxAgent",
            &format!("Wi-Fi ready on https://0.0.0.0:{}", state.wifi.port),
        );
        let local = phonebox::server::serve(listener, state.clone(), grace);
        let wifi = phonebox::server::serve_wifi(tcp, config, state, grace);
        let (local, wifi) = tokio::join!(local, wifi);
        return local.and(wifi);
    }
    phonebox::server::serve(listener, state, grace).await
}

/// Ctrl+C, or the SIGTERM launchd / systemd send to stop the agent.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn open_in_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    let _ = result;
}
