//! Port of `ToolProcess.cs`: bounded runs of optional command-line tools (gh, docker, codexbar, agy, shells). Both
//! output streams are drained, the working folder is neutral, and a timeout or a cancellation ends the whole process
//! tree and reaps it before the run returns.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A tool and its arguments.
#[derive(Debug, Clone, Default)]
pub struct ToolCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl ToolCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        ToolCommand {
            program: program.into(),
            ..Default::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// What a finished run gives: the exit code and both streams as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// `"<file name> timed out"` (C#'s `TimeoutException`).
    Timeout(String),
    /// The caller cancelled (C#'s `OperationCanceledException`).
    Cancelled,
    /// The program could not be started.
    Start(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Timeout(m) | RunError::Start(m) => f.write_str(m),
            RunError::Cancelled => f.write_str("The operation was canceled."),
        }
    }
}

/// The first `tool` (`tool.exe`, then `tool`, on Windows) in PATH, then in the Homebrew and system folders (not on
/// Windows). A file of that name counts; whether it is executable is the run's problem.
pub fn find(tool: &str) -> Option<PathBuf> {
    find_in(
        tool,
        &std::env::var("PATH").unwrap_or_default(),
        cfg!(windows),
    )
}

pub fn find_in(tool: &str, path_var: &str, windows: bool) -> Option<PathBuf> {
    let names: Vec<String> = if windows {
        vec![format!("{tool}.exe"), tool.to_string()]
    } else {
        vec![tool.to_string()]
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
    let mut dirs: Vec<String> = path_var
        .split(separator)
        .filter(|d| !d.is_empty())
        .map(|d| d.trim_matches(|c| c == '"' || c == ' ').to_string())
        .collect();
    if !windows {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(String::from));
    }
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| Path::new(d).join(n)))
        .find(|p| p.is_file())
}

/// Runs `cmd` until it exits, `timeout` passes or `cancelled` resolves (checked first: an already-cancelled run never
/// starts). On a timeout or a cancellation the whole process tree is killed and reaped.
pub async fn run(
    cmd: &ToolCommand,
    timeout: Duration,
    cancelled: impl Future<Output = ()>,
) -> Result<Output, RunError> {
    use futures_util::FutureExt;
    let mut cancelled = std::pin::pin!(cancelled);
    if cancelled.as_mut().now_or_never().is_some() {
        return Err(RunError::Cancelled);
    }
    let name = file_name(&cmd.program);
    let mut command = tokio::process::Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .current_dir(crate::app_paths::neutral_cwd())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0); // the tree is the group: one kill ends the children too
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = command
        .spawn()
        .map_err(|e| RunError::Start(format!("Could not start {name}: {e}")))?;
    let pid = child.id();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    enum Outcome {
        Done(std::io::Result<std::process::ExitStatus>, Vec<u8>, Vec<u8>),
        TimedOut,
        Cancelled,
    }
    let outcome = {
        let work = async {
            let (status, out, err) = tokio::join!(child.wait(), drain(stdout), drain(stderr));
            Outcome::Done(status, out, err)
        };
        tokio::select! {
            done = work => done,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            _ = cancelled.as_mut() => Outcome::Cancelled,
        }
    };
    match outcome {
        Outcome::Done(status, out, err) => Ok(Output {
            code: status.ok().and_then(|s| s.code()).unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }),
        stopped => {
            kill_tree(pid);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            Err(match stopped {
                Outcome::Cancelled => RunError::Cancelled,
                _ => RunError::Timeout(format!("{name} timed out")),
            })
        }
    }
}

/// Ends a process and everything it started (C#'s `Kill(entireProcessTree: true)`).
pub fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    unsafe {
        // the child leads its own process group (`process_group(0)`)
        libc::killpg(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let Ok(mut sys) = sysinfo::System::new() else {
            return;
        };
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let table: Vec<(u32, Option<u32>, u64)> = sys
            .processes()
            .iter()
            .map(|(id, p)| {
                (
                    id.as_u32(),
                    p.parent().map(|pp| pp.as_u32()),
                    p.start_time(),
                )
            })
            .collect();
        for p in tree_order(pid, &table) {
            if let Some(process) = sys.process(sysinfo::Pid::from_u32(p)) {
                process.kill();
            }
        }
    }
}

/// The processes under `root` in `table` (pid, parent pid, start time in seconds), the deepest first, so nobody is
/// left to start new children. As .NET's `Kill(entireProcessTree)`, a process older than its recorded parent is not
/// its child: Windows keeps a dead parent's pid, and that pid may since belong to someone else.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn tree_order(root: u32, table: &[(u32, Option<u32>, u64)]) -> Vec<u32> {
    use std::collections::HashMap;
    let started: HashMap<u32, u64> = table.iter().map(|&(id, _, at)| (id, at)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(id, parent, at) in table {
        if let Some(parent) = parent
            && started
                .get(&parent)
                .is_some_and(|&parent_at| parent_at <= at)
        {
            children.entry(parent).or_default().push(id);
        }
    }
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        order.push(p);
        stack.extend(children.get(&p).into_iter().flatten().copied());
    }
    order.reverse();
    order
}

/// [`run`] from a blocking caller: on a thread of its own, so the wait never sits inside an async task, on the
/// caller's runtime or, from a plain thread, a small one of its own. None when the tool could not run or timed out.
pub fn run_blocking(cmd: &ToolCommand, timeout: Duration) -> Option<Output> {
    let current = tokio::runtime::Handle::try_current().ok();
    std::thread::scope(|s| {
        s.spawn(|| {
            let run = run(cmd, timeout, std::future::pending());
            match current {
                Some(runtime) => runtime.block_on(run).ok(),
                None => tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?
                    .block_on(run)
                    .ok(),
            }
        })
        .join()
        .ok()
        .flatten()
    })
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes).await;
    }
    bytes
}

fn file_name(program: &Path) -> String {
    program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The shell the C# tests use: `/bin/sh -c` on Unix, Windows PowerShell elsewhere.
    pub(crate) fn shell(windows: &str, unix: &str) -> ToolCommand {
        if cfg!(windows) {
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            ToolCommand::new(
                PathBuf::from(system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            )
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                windows,
            ])
        } else {
            ToolCommand::new("/bin/sh").args(["-c", unix])
        }
    }

    #[test]
    fn a_tree_skips_processes_older_than_their_recorded_parent() {
        // 40 is an app whose launcher (pid 7) exited before 7 was reused by the tool started at 100
        let table = [
            (7, Some(1), 100),
            (8, Some(7), 101),
            (9, Some(8), 101),
            (40, Some(7), 50),
            (41, Some(40), 60),
        ];
        let order = tree_order(7, &table);
        assert_eq!(order, vec![9, 8, 7]);
    }

    #[test]
    fn a_tree_keeps_children_started_in_the_same_second() {
        let table = [(7, Some(1), 100), (8, Some(7), 100)];
        assert_eq!(tree_order(7, &table), vec![8, 7]);
    }

    #[test]
    fn a_blocking_run_works_from_a_thread_without_a_runtime() {
        let cmd = shell("Write-Output hi", "echo hi");
        let out = std::thread::spawn(move || run_blocking(&cmd, Duration::from_secs(10)))
            .join()
            .unwrap();
        assert_eq!(out.map(|o| o.stdout.trim().to_string()), Some("hi".into()));
    }

    pub(crate) fn alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // a zombie that is not reaped yet still answers; give the reaper a moment
            for _ in 0..40 {
                if unsafe { libc::kill(pid as i32, 0) } != 0 {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            true
        }
        #[cfg(windows)]
        {
            let Ok(mut sys) = sysinfo::System::new() else {
                return false;
            };
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            sys.process(sysinfo::Pid::from_u32(pid)).is_some()
        }
    }

    pub(crate) async fn read_pid(marker: &Path) -> u32 {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(marker)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the child never wrote its pid");
    }

    #[tokio::test]
    async fn drains_both_streams_and_reports_failure() {
        let result = run(
            &shell(
                "[Console]::Out.Write('hello'); [Console]::Error.Write('failed'); exit 7",
                "printf hello; printf failed >&2; exit 7",
            ),
            Duration::from_secs(10),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.code, 7);
        assert_eq!(result.stdout, "hello");
        assert_eq!(result.stderr, "failed");
    }

    #[tokio::test]
    async fn timeout_terminates_the_child_and_cancellation_remains_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let m = marker.display().to_string();
        let result = run(
            &shell(
                &format!("[IO.File]::WriteAllText('{m}', [string]$PID); Start-Sleep -Seconds 60"),
                &format!("echo $$ > '{m}'; exec sleep 60"),
            ),
            Duration::from_secs(3),
            std::future::pending(),
        )
        .await;
        let name = if cfg!(windows) {
            "powershell.exe"
        } else {
            "sh"
        };
        assert_eq!(result, Err(RunError::Timeout(format!("{name} timed out"))));
        let pid = read_pid(&marker).await;
        assert!(!alive(pid), "the child outlived its timeout");

        let result = run(
            &shell("Start-Sleep 60", "sleep 60"),
            Duration::from_secs(10),
            std::future::ready(()),
        )
        .await;
        assert_eq!(result, Err(RunError::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_grandchild_is_killed_with_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let m = marker.display().to_string();
        let result = run(
            &shell("", &format!("sleep 30 & echo $! > '{m}'; sleep 30")),
            Duration::from_secs(1),
            std::future::pending(),
        )
        .await;
        assert!(matches!(result, Err(RunError::Timeout(_))), "{result:?}");
        assert!(
            !alive(read_pid(&marker).await),
            "the background child outlived the run"
        );
    }

    #[tokio::test]
    async fn cancelling_a_run_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let m = marker.display().to_string();
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let cmd = shell(
            &format!("[IO.File]::WriteAllText('{m}', [string]$PID); Start-Sleep -Seconds 60"),
            &format!("echo $$ > '{m}'; exec sleep 60"),
        );
        let task = tokio::spawn(async move {
            run(&cmd, Duration::from_secs(30), async {
                let _ = cancelled.await;
            })
            .await
        });
        let pid = read_pid(&marker).await;
        cancel.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, Err(RunError::Cancelled));
        assert!(!alive(pid));
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_a_start_error() {
        let result = run(
            &ToolCommand::new("/nonexistent/phonebox-tool"),
            Duration::from_secs(5),
            std::future::pending(),
        )
        .await;
        assert!(matches!(result, Err(RunError::Start(_))), "{result:?}");
    }

    #[test]
    fn find_looks_in_path_then_the_homebrew_folders() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::create_dir(a.path().join("tool")).unwrap(); // a folder of that name is no tool
        std::fs::write(b.path().join("tool"), "").unwrap();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let path = format!(
            " \"{}\" {sep}{sep}{}",
            a.path().display(),
            b.path().display()
        );
        assert_eq!(find_in("tool", &path, false), Some(b.path().join("tool")));
        assert_eq!(find_in("phonebox-no-such-tool", &path, false), None);
        // Windows: tool.exe before tool
        std::fs::write(b.path().join("tool.exe"), "").unwrap();
        assert_eq!(
            find_in("tool", &path, true),
            Some(b.path().join("tool.exe"))
        );
        // the Homebrew and system folders come after PATH, outside Windows
        // (env: /usr/bin on Linux and macOS alike; macOS keeps sh in /bin)
        #[cfg(unix)]
        assert!(
            find_in("env", "", false)
                .is_some_and(|p| p.starts_with("/usr") || p.starts_with("/opt"))
        );
        assert_eq!(find_in("env", "", true), None);
    }

    #[test]
    fn the_error_texts_match_the_csharp_ones() {
        assert_eq!(
            RunError::Cancelled.to_string(),
            "The operation was canceled."
        );
        assert_eq!(file_name(Path::new("/opt/homebrew/bin/gh")), "gh");
    }
}
