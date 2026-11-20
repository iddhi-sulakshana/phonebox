//! Port of `AppRestart` (`UpdateService.cs`): restarting the agent (Wi-Fi turned on, an update installed).

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Exit code that asks launchd / systemd to start the agent again (KeepAlive SuccessfulExit=false, Restart=on-failure).
pub const SERVICE_RESTART_CODE: u8 = 75;

pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// True when launchd (label net.oxog.phonebox.agent) or systemd started the agent: a copy it starts itself would not
/// survive there, so it exits with [`SERVICE_RESTART_CODE`] instead.
pub fn service_managed(
    env: impl Fn(&str) -> Option<String>,
    pid: u32,
    parent_name: impl Fn() -> Option<String>,
) -> bool {
    env("XPC_SERVICE_NAME").as_deref() == Some("net.oxog.phonebox.agent")
        || (env("INVOCATION_ID").is_some_and(|v| !v.is_empty())
            && match env("SYSTEMD_EXEC_PID").filter(|v| !v.is_empty()) {
                Some(exec) => exec == pid.to_string(),
                None => parent_name().as_deref() == Some("systemd"),
            })
}

/// Linux: the parent process's name from /proc; None elsewhere or when it cannot be read.
pub fn parent_name() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // /proc/self/stat: "pid (comm) state ppid ..."; comm may hold spaces and parentheses, so read after the last ')'
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let ppid = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .to_string();
    Some(
        std::fs::read_to_string(format!("/proc/{ppid}/comm"))
            .ok()?
            .trim()
            .to_string(),
    )
}

/// The arguments of the new instance: the same ones, and `--restart` once (it waits for this one to exit).
pub fn restart_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| *a != "--restart")
        .cloned()
        .chain(["--restart".to_string()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_preserves_the_config_path_and_port_as_separate_arguments() {
        // the C# test's second half (a framework-dependent agent under `dotnet`) has no Rust counterpart
        let args: Vec<String> = [
            "--config",
            "C:/test folder/config.json",
            "--port",
            "18765",
            "--restart",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(restart_args(&args), args);
        assert_eq!(
            restart_args(&["--port".into(), "18765".into()]),
            ["--port", "18765", "--restart"]
        );
    }

    /// `AppRestartTests.Managed`: pid 4242, the given environment and parent process name.
    fn managed(vars: &[(&str, &str)], parent: Option<&str>) -> bool {
        let env = |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };
        service_managed(env, 4242, || parent.map(String::from))
    }

    #[test]
    fn the_setup_login_item_and_systemd_are_service_managers() {
        assert!(managed(
            &[("XPC_SERVICE_NAME", "net.oxog.phonebox.agent")],
            None
        ));
        assert!(managed(
            &[
                ("INVOCATION_ID", "5f0c1c3a9e8b4f0d"),
                ("SYSTEMD_EXEC_PID", "4242")
            ],
            None
        ));
        assert!(managed(
            &[("INVOCATION_ID", "5f0c1c3a9e8b4f0d")],
            Some("systemd")
        )); // systemd before 248
    }

    #[test]
    fn a_terminal_or_the_tray_is_not() {
        // macOS sets XPC_SERVICE_NAME for apps and Terminal sessions too ("0" or the app's own id)
        assert!(!managed(&[("XPC_SERVICE_NAME", "0")], None));
        assert!(!managed(
            &[("XPC_SERVICE_NAME", "application.com.apple.Terminal.1234")],
            None
        ));
        assert!(!managed(&[], None));
        assert!(!managed(&[("INVOCATION_ID", "")], None));
    }

    #[test]
    fn a_process_that_only_inherited_the_unit_is_not() {
        // started by hand from a shell inside a systemd unit: same INVOCATION_ID, but systemd started the shell
        assert!(!managed(
            &[
                ("INVOCATION_ID", "5f0c1c3a9e8b4f0d"),
                ("SYSTEMD_EXEC_PID", "1000")
            ],
            None
        ));
        assert!(!managed(
            &[("INVOCATION_ID", "5f0c1c3a9e8b4f0d")],
            Some("bash")
        ));
    }

    #[test]
    fn the_restart_code_is_a_failure() {
        assert_ne!(SERVICE_RESTART_CODE, 0);
    }
}
