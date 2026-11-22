//! Port of `Unix/Sh.cs`: small helpers for running command-line tools on Linux and macOS.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Places searched after PATH.
const USUAL_DIRS: [&str; 7] = [
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/usr/sbin",
    "/sbin",
    "/snap/bin",
];

/// Full path of a tool in PATH (or a few usual places), or None. A hit is remembered while the file exists.
pub fn which(tool: &str) -> Option<PathBuf> {
    static FOUND: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let mut found = FOUND
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(Some(hit)) = found.get(tool)
        && hit.is_file()
    {
        return Some(hit.clone());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let hit = path
        .split(':')
        .filter(|d| !d.is_empty())
        .chain(USUAL_DIRS)
        .map(|d| Path::new(d).join(tool))
        .find(|p| p.is_file());
    found.insert(tool.to_string(), hit.clone());
    hit
}

/// Runs a tool and returns its standard output, or None if it is missing, fails or takes too long (2 s).
pub fn run(tool: &str, args: &[&str]) -> Option<String> {
    run_with_timeout(2000, tool, args)
}

/// `ToolProcess.RunAsync` as `Sh` uses it: the process and its output pipes share one deadline. When the deadline
/// passes, a process that is still running is killed with its process group; a child that outlived it and still
/// holds the output open is left alone, and the answer is None.
pub fn run_with_timeout(timeout_ms: u64, tool: &str, args: &[&str]) -> Option<String> {
    run_inner(timeout_ms, tool, args, true)
}

/// [`run_with_timeout`] for tools whose exit code says nothing useful (lsof exits with 1 when nothing matches): the
/// output counts whatever the exit code.
pub fn run_any_exit(timeout_ms: u64, tool: &str, args: &[&str]) -> Option<String> {
    run_inner(timeout_ms, tool, args, false)
}

fn run_inner(timeout_ms: u64, tool: &str, args: &[&str], need_success: bool) -> Option<String> {
    let path = if tool.contains('/') {
        PathBuf::from(tool)
    } else {
        which(tool)?
    };
    let mut cmd = Command::new(path);
    cmd.args(args)
        .current_dir(crate::app_paths::neutral_cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (status, output) = wait_for(&mut child, deadline)?;
    if need_success && !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output).into_owned())
}

/// The tool's exit status and standard output (standard error is drained), read with poll(2) in this thread: it
/// sleeps until output or the end of the output arrives, or the deadline, never on a timer (C#'s WaitForExitAsync).
/// None when the deadline passes: a tool still running ends with its process group; one that exited while a child of
/// its own still holds the output open is left alone.
#[cfg(unix)]
fn wait_for(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<(std::process::ExitStatus, Vec<u8>)> {
    use std::os::fd::AsRawFd;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let mut stdout = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while out.is_some() || err.is_some() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                kill_tree(child);
            }
            return None;
        }
        let mut fds: Vec<libc::pollfd> = [
            out.as_ref().map(|p| p.as_raw_fd()),
            err.as_ref().map(|p| p.as_raw_fd()),
        ]
        .into_iter()
        .flatten()
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
        // rounded up, so that a wait shorter than a millisecond does not spin
        let ms = i32::try_from(left.as_millis() + 1).unwrap_or(i32::MAX);
        // SAFETY: fds holds fds.len() initialised pollfd entries for pipes this function owns and keeps open.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            kill_tree(child);
            return None;
        }
        let mut ready = fds.iter().map(|fd| fd.revents != 0);
        if out.is_some() && ready.next() == Some(true) {
            match out.as_mut().map(|p| p.read(&mut buf)) {
                Some(Ok(n)) if n > 0 => stdout.extend_from_slice(&buf[..n]),
                Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => out = None,
            }
        }
        if err.is_some() && ready.next() == Some(true) {
            match err.as_mut().map(|p| p.read(&mut buf)) {
                Some(Ok(n)) if n > 0 => {}
                Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => err = None,
            }
        }
    }
    // the output ended, so the tool has exited (or is exiting); one that closed its output and runs on gets the rest
    // of the deadline
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some((status, stdout)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                kill_tree(child);
                return None;
            }
        }
    }
}

/// [`wait_for`] where poll(2) is missing: a thread per output pipe, the exit looked at every 10 ms (nothing on Windows
/// runs these tools).
#[cfg(not(unix))]
fn wait_for(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<(std::process::ExitStatus, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let pipes: [(bool, Option<Box<dyn Read + Send>>); 2] = [
        (
            true,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ];
    for (is_out, pipe) in pipes {
        let Some(mut pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send((is_out, bytes));
        });
    }
    drop(tx);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                kill_tree(child);
                return None;
            }
        }
    };
    let mut output = None;
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, bytes)) => output = Some(bytes),
            Ok((false, _)) => {}
            Err(_) => return None, // a child still holds the output open: the deadline wins
        }
    }
    output.map(|bytes| (status, bytes))
}

/// `Process.Kill(entireProcessTree: true)`: the process group on Unix (the tool runs in its own group); these macOS
/// and Linux tools never run on Windows, where only the process would end.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill(2) with a negative pid signals that process group; the group was created for this child.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The effective user id of the agent (to tell the user's own processes from others'); 0 on Windows.
pub fn uid() -> u32 {
    #[cfg(unix)]
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    return unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    0
}

// the Linux and macOS helper: nothing on Windows runs it, so neither do its tests
#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    fn wait_for_file(path: &Path, limit: Duration) {
        let end = Instant::now() + limit;
        while !path.exists() {
            assert!(Instant::now() < end, "{} did not appear", path.display());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// A shell whose background child keeps the output pipe open until `<path>.release` appears (at most 12 s), then
    /// writes `<path>.done`; the shell itself exits at once.
    fn holding_process(path: &Path) -> (String, Vec<String>) {
        let p = path.to_string_lossy().into_owned();
        if cfg!(windows) {
            let escaped = p.replace('\'', "''");
            let script = format!(
                "$p = '{escaped}'; [IO.File]::WriteAllText($p + '.ready', ''); \
                 $timer = [Diagnostics.Stopwatch]::StartNew(); \
                 while (![IO.File]::Exists($p + '.release') -and $timer.Elapsed.TotalSeconds -lt 12) {{ Start-Sleep -Milliseconds 25 }}; \
                 [IO.File]::WriteAllText($p + '.done', '')"
            );
            let encoded = utf16_base64(&script);
            let parent = format!(
                "$info = New-Object Diagnostics.ProcessStartInfo; \
                 $info.FileName = $env:ComSpec; $info.UseShellExecute = $false; \
                 $info.Arguments = '/d /c start /b powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}'; \
                 $cmd = [Diagnostics.Process]::Start($info); $cmd.WaitForExit(); exit $cmd.ExitCode"
            );
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:/Windows".into());
            let powershell = format!("{system}/System32/WindowsPowerShell/v1.0/powershell.exe")
                .replace('\\', "/");
            return (
                powershell,
                [
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                ]
                .iter()
                .map(|s| s.to_string())
                .chain([utf16_base64(&parent)])
                .collect(),
            );
        }
        let unix = r#"(
    : > "$1.ready"
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
        [ -f "$1.release" ] && break
        sleep 1
    done
    : > "$1.done"
) &"#;
        (
            "/bin/sh".into(),
            vec!["-c".into(), unix.into(), "phonebox-test".into(), p],
        )
    }

    /// PowerShell's -EncodedCommand: base64 of the UTF-16LE text.
    fn utf16_base64(text: &str) -> String {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    ABC[(n >> (18 - 6 * i) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
    }

    #[test]
    fn output_deadline_applies_after_the_direct_process_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phonebox-sh");
        let with = |suffix: &str| PathBuf::from(format!("{}{suffix}", path.display()));
        let (ready, release, done) = (with(".ready"), with(".release"), with(".done"));
        let (tool, args) = holding_process(&path);
        let run = std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run_with_timeout(2000, &tool, &args)
        });
        wait_for_file(&ready, Duration::from_secs(6));
        let end = Instant::now() + Duration::from_secs(4);
        while !run.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(25));
        }
        let finished = run.is_finished();
        let done_early = done.exists();
        std::fs::write(&release, "").unwrap();
        wait_for_file(&done, Duration::from_secs(16));
        assert!(
            finished,
            "the deadline did not end the run while a child held the output open"
        );
        assert_eq!(run.join().unwrap(), None);
        assert!(!done_early, "the run waited for the child");
    }

    /// This thread's voluntary context switches (Linux): one per wake from a wait.
    #[cfg(target_os = "linux")]
    fn my_wakes() -> u64 {
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let n = status
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .unwrap();
        n.trim().parse().unwrap()
    }

    // While a tool runs, the caller sleeps until the tool's output or exit arrives (C#'s WaitForExitAsync); it used to
    // look every 10 ms, 100 wakes a second per tool, which tripled the idle agent's wake-ups on the MacBook (the media
    // and audio readers run nowplaying-cli and osascript every few seconds).
    #[cfg(target_os = "linux")]
    #[test]
    fn waiting_for_a_tool_does_not_poll() {
        let before = my_wakes();
        assert_eq!(
            run_with_timeout(5000, "sleep", &["0.5"]),
            Some(String::new())
        );
        let woke = my_wakes() - before;
        assert!(
            woke <= 10,
            "the caller woke {woke} times while the tool ran for 0.5 s"
        );
    }

    #[test]
    fn a_tool_that_finishes_returns_its_output_and_a_failing_one_none() {
        assert_eq!(run("sh", &["-c", "printf hi"]).as_deref(), Some("hi"));
        assert_eq!(run("sh", &["-c", "printf hi; exit 3"]), None);
        assert!(which("sh").is_some());
        assert_eq!(run("phonebox-no-such-tool", &[]), None);
    }
}
