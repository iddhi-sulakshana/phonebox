//! Entry point of `PhoneBoxAgent`; all behavior lives in the `phonebox` library crate. The order follows the C#
//! agent's `Program.cs`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use phonebox::agent_endpoint::AgentEndpoint;
use phonebox::config::store::ConfigStore;
use phonebox::server::{AppState, build_router};
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

    let push = PushStore::new(&app_paths::config_dir().join("data.json"));
    let state = AppState::new(
        cfg,
        endpoint,
        push.clone(),
        Arc::new(phonebox::state::StateSources::phase1()),
    );
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(serve(state, config_path));
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
    state.adb.start(); // the USB tunnel to the phone
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
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown_signal().await;
            stop.stop();
        }
    });
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async move { stop.stopped().await })
        .await
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
