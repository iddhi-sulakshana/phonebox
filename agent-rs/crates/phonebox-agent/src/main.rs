//! Entry point of `PhoneBoxAgent`; all behavior lives in the `phonebox` library crate. The order follows the C#
//! agent's `Program.cs`.

// Windows: no console window (C#'s WinExe); the agent lives in the notification area. Tests keep the console.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use phonebox::agent_endpoint::AgentEndpoint;
use phonebox::config::store::ConfigStore;
use phonebox::server::AppState;
use phonebox::{app_paths, app_restart, log, push_store::PushStore, single_instance};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // read now: an update renames this program to `.old` before the restart, and on Linux the running file's own path
    // follows the rename
    let exe = std::env::current_exe().ok();
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
        phonebox::tray::open_url(&endpoint.settings_url());
        return ExitCode::SUCCESS;
    };

    // an install updated from a framework-dependent C# release still carries its files; nothing loads them now
    if let Some(exe) = &exe
        && phonebox::update::migration::running_installed(exe)
    {
        phonebox::update::migration::remove_framework_dependent_leftovers(
            &app_paths::install_dir(),
            true,
        );
    }

    let (ensure_default, custom) = config_choice(&args);
    if ensure_default {
        let _ = app_paths::ensure_config();
    }
    // absolute from here on: files kept next to the config need a folder
    let config_path = custom.unwrap_or_else(app_paths::config_path);
    let config_path = std::path::absolute(&config_path).unwrap_or(config_path);
    app_paths::restrict_config(&config_path);
    let cfg = Arc::new(ConfigStore::new(config_path.clone()));
    // keep the Wi-Fi key stable across restarts; a settings file that cannot be written ends the agent, as in C#
    if let Err(e) = cfg.persist_generated() {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
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
    // the updater comes with the state, as the deck does
    let update = Arc::new(phonebox::update::service::UpdateSlot::default());
    let sources = phonebox::state::StateSources::phase1()
        .with_pc(Box::new(system.clone()))
        .with_ping(Box::new(ping.clone()))
        .with_update(Box::new(update.clone()))
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
    update.set(state.updates.clone());
    let controls = state.controls.clone();
    let stop = state.stop.clone();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut tray = None;
    let result = runtime.block_on(serve(state, config_path, &mut tray));
    // the tray went first: its thread removes the icon once the stop fires (also when the server failed); an icon left
    // behind stays in the notification area until the mouse passes over it
    stop.stop();
    if let Some(tray) = tray {
        tray.wait(Duration::from_secs(1));
    }
    // blocking work still running (a probe, a download) gets 5 s, then the agent goes on without it
    runtime.shutdown_timeout(Duration::from_secs(5));
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
        if let Some(exe) = &exe {
            let (program, folder) = app_restart::restart_target(exe);
            let _ = std::process::Command::new(program)
                .args(app_restart::restart_args(&args))
                .current_dir(folder)
                .spawn();
        }
    }
    ExitCode::SUCCESS
}

/// Whether to write the default settings file, and the `--config` file: C# skips the default whenever the flag is there,
/// even last with no file after it (then the default path is used without the default content).
fn config_choice(args: &[String]) -> (bool, Option<std::path::PathBuf>) {
    let custom = args
        .iter()
        .skip_while(|a| *a != "--config")
        .nth(1)
        .map(std::path::PathBuf::from);
    (!args.iter().any(|a| a == "--config"), custom)
}

async fn serve(
    state: AppState,
    config_path: std::path::PathBuf,
    tray: &mut Option<phonebox::tray::Tray>,
) -> std::io::Result<()> {
    let port = state.endpoint.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .inspect_err(|e| log_bind_failure(&format!("http://127.0.0.1:{port}"), e))?;
    // Wi-Fi / Tailscale: HTTPS on every IPv4 address with the pinned certificate; a port that cannot be bound ends the
    // agent, as in C#
    #[cfg(feature = "tls")]
    let wifi = match &state.wifi.cert {
        Some(cert) => {
            let config =
                phonebox::server::tls::server_config(cert).map_err(std::io::Error::other)?;
            let wifi_port = state.wifi.port;
            let tcp = tokio::net::TcpListener::bind(("0.0.0.0", wifi_port))
                .await
                .inspect_err(|e| log_bind_failure(&format!("https://0.0.0.0:{wifi_port}"), e))?;
            Some((tcp, config))
        }
        None => None,
    };
    #[cfg(not(feature = "tls"))]
    if state.wifi.cert.is_some() {
        log::warn("PhoneBoxAgent", "this build has no TLS: Wi-Fi stays off");
    }
    state.adb.start(); // the USB tunnel to the phone
    state.system.start(state.stop.clone(), state.jobs.clone());
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
    state.updates.start(); // after 20 s, then every minute: a check every 6 h (15 min after a failure); auto apply
    // every second: the phones' screens follow the Mac's lock while display.followComputerLock is on
    phonebox::session_lock_monitor::start(state.cfg.clone(), state.adb.clone(), state.stop.clone());
    // last, as C# registers it: Windows' notification-area icon; elsewhere a log line names the settings page
    *tray = Some(phonebox::tray::start(
        state.endpoint,
        state.adb.clone(),
        state.stop.clone(),
    ));
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

/// The `crit` line C#'s Kestrel writes before the agent ends on a port it cannot bind; without it a second agent (a C#
/// one keeps another lock) would end without a word in agent.log.
fn log_bind_failure(address: &str, e: &std::io::Error) {
    let reason = if e.kind() == std::io::ErrorKind::AddrInUse {
        "address already in use".to_string()
    } else {
        e.to_string()
    };
    log::log(
        phonebox::file_logger::Level::Critical,
        "PhoneBoxAgent",
        &format!("Unable to start the server. | Failed to bind to address {address}: {reason}."),
    );
}

/// Ctrl+C, the SIGTERM launchd / systemd send to stop the agent, or SIGQUIT (.NET's console lifetime stops on all
/// three).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut quit = signal(SignalKind::quit()).expect("SIGQUIT handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = quit.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    // survey b§3.2 item 14: C# skips the default settings file whenever the flag is there (`args.Contains`)
    #[test]
    fn a_trailing_config_flag_creates_no_config() {
        assert_eq!(
            config_choice(&args(&["--port", "9000", "--config"])),
            (false, None)
        );
        assert_eq!(
            config_choice(&args(&["--config", "x.json"])),
            (false, Some(std::path::PathBuf::from("x.json")))
        );
        assert_eq!(config_choice(&args(&["--restart"])), (true, None));
    }

    #[tokio::test]
    async fn a_busy_local_port_is_written_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("agent.log");
        log::init(&log_path).unwrap();
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let config_path = dir.path().join("config.json");
        let cfg = Arc::new(ConfigStore::new(config_path.clone()));
        let push = PushStore::new(&dir.path().join("data.json"));
        let state = AppState::new(
            cfg,
            AgentEndpoint { port },
            push,
            Arc::new(phonebox::state::StateSources::phase1()),
        );
        let mut tray = None;
        let error = serve(state, config_path, &mut tray).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(tray.is_none(), "nothing starts on a port that is taken");
        let text = std::fs::read_to_string(&log_path).unwrap();
        let line = format!(
            " crit PhoneBoxAgent: Unable to start the server. | Failed to bind to address http://127.0.0.1:{port}: \
             address already in use.\n"
        );
        assert!(text.contains(&line), "{text}");
    }
}
