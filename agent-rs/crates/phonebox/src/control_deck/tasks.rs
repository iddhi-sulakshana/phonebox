//! `ControlDeck.Tasks.cs`: runs the programs of the deck's task and server actions, and tracks only the processes it
//! started. A task may run 30 minutes, a server a day; the last 64 Ki characters of their output are kept for the phone.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::store::ConfigStore;
use crate::config::{DeckAction, Shortcut};
use crate::control_deck::models::DeckResult;

const CATEGORY: &str = "PhoneBox.Agent.ControlDeck";
/// The output the phone can read: the last this many characters (UTF-16 units).
const OUTPUT_KEEP: usize = 65_536;
const TASK_LIMIT: Duration = Duration::from_secs(30 * 60);
const SERVER_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One start of an action's program.
struct Job {
    stop: AtomicBool,
    state: Mutex<(String, String)>,
    done: Mutex<bool>,
    finished: Condvar,
}

impl Job {
    fn new() -> Job {
        Job {
            stop: AtomicBool::new(false),
            state: Mutex::new(("Starting".into(), String::new())),
            done: Mutex::new(false),
            finished: Condvar::new(),
        }
    }

    fn is_done(&self) -> bool {
        *lock(&self.done)
    }

    fn state(&self) -> String {
        lock(&self.state).0.clone()
    }

    fn set_state(&self, state: &str) {
        lock(&self.state).0 = state.to_string();
    }

    fn set_output(&self, output: String) {
        lock(&self.state).1 = output;
    }

    fn finish(&self) {
        *lock(&self.done) = true;
        self.finished.notify_all();
    }

    /// Waits until the work ended, at most until `until`; true when it ended.
    fn wait_until(&self, until: Instant) -> bool {
        let mut done = lock(&self.done);
        while !*done {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            done = self
                .finished
                .wait_timeout(done, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

pub struct ControlTasks {
    cfg: Arc<ConfigStore>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

impl ControlTasks {
    pub fn new(cfg: Arc<ConfigStore>) -> ControlTasks {
        ControlTasks {
            cfg,
            jobs: Default::default(),
        }
    }

    /// "Not started", "Starting", "Running", "Completed", "Failed (n)", "Stopped", "Timed out" or "Failed to start".
    pub fn state(&self, id: &str) -> String {
        lock(&self.jobs)
            .get(id)
            .map_or_else(|| "Not started".into(), |job| job.state())
    }

    /// The last output of an action's program, or "No task output".
    pub fn output(&self, id: &str) -> String {
        lock(&self.jobs)
            .get(id)
            .map_or_else(|| "No task output".into(), |job| lock(&job.state).1.clone())
    }

    /// Starts an action's program (or, with `stop`, stops it); a start that failed is reported as failed.
    pub fn start(&self, action: &DeckAction, stop: bool) -> DeckResult {
        let mut jobs = lock(&self.jobs);
        let running = jobs.get(&action.id).filter(|job| !job.is_done()).cloned();
        if stop {
            return match running {
                Some(job) => {
                    job.stop.store(true, Ordering::SeqCst);
                    DeckResult::ok()
                }
                None => DeckResult::fail("This task is not running"),
            };
        }
        if running.is_some() {
            return DeckResult::fail("This task is already running");
        }
        let cfg = self.cfg.get();
        let Some(shortcut) = cfg
            .shortcuts
            .iter()
            .find(|s| s.id == action.target)
            .filter(|s| !s.cmd.starts_with("builtin:"))
        else {
            return DeckResult::fail("Choose an executable shortcut for this task");
        };
        let job = Arc::new(Job::new());
        jobs.insert(action.id.clone(), job.clone());
        let limit = if action.kind == "server" {
            SERVER_LIMIT
        } else {
            TASK_LIMIT
        };
        // the program is started here, so a start that failed is known before the phone is answered
        run(job.clone(), shortcut, limit);
        if job.state() == "Failed to start" {
            DeckResult::fail("The configured task could not be started")
        } else {
            DeckResult::ok()
        }
    }

    /// Stops an action's program if it runs, without failing when it does not.
    pub fn stop_if_running(&self, id: &str) {
        if let Some(job) = lock(&self.jobs).get(id).filter(|job| !job.is_done()) {
            job.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Stops every program (the agent is stopping), and waits at most `limit` for them to end.
    pub fn stop_all(&self, limit: Duration) {
        let jobs: Vec<Arc<Job>> = lock(&self.jobs).values().cloned().collect();
        for job in &jobs {
            job.stop.store(true, Ordering::SeqCst);
        }
        let until = Instant::now() + limit;
        for job in jobs {
            job.wait_until(until);
        }
    }
}

/// The output both pipes share: the last [`OUTPUT_KEEP`] characters.
#[derive(Default)]
struct Output {
    text: String,
    /// Set once the run stopped reading: a daemon child that kept a pipe adds nothing more.
    closed: bool,
}

/// Appends decoded text and drops the oldest characters beyond the limit.
fn append(output: &Mutex<Output>, job: &Job, text: &str) {
    let mut out = lock(output);
    if out.closed {
        return;
    }
    out.text.push_str(text);
    let units = crate::dotnet_text::utf16_len(&out.text);
    if units > OUTPUT_KEEP {
        let mut excess = units - OUTPUT_KEEP;
        let mut cut = 0;
        for (i, c) in out.text.char_indices() {
            if excess == 0 {
                cut = i;
                break;
            }
            excess = excess.saturating_sub(c.len_utf16());
            cut = i + c.len_utf8();
        }
        out.text.drain(..cut);
    }
    job.set_output(out.text.clone());
}

/// Reads a pipe to its end as UTF-8 (a character split between two reads is kept for the next).
fn read_pipe(
    mut pipe: impl Read,
    output: Arc<Mutex<Output>>,
    job: Arc<Job>,
    done: std::sync::mpsc::Sender<()>,
) {
    let mut buf = [0u8; 2048];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&buf[..n]);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    pending.clear();
                    break;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    text.push_str(std::str::from_utf8(&pending[..good]).unwrap_or(""));
                    match e.error_len() {
                        Some(bad) => {
                            text.push('\u{FFFD}');
                            pending.drain(..good + bad);
                        }
                        None => {
                            pending.drain(..good);
                            break;
                        }
                    }
                }
            }
        }
        if !text.is_empty() {
            append(&output, &job, &text);
        }
    }
    if !pending.is_empty() {
        append(&output, &job, &String::from_utf8_lossy(&pending));
    }
    let _ = done.send(());
}

/// Waits for both readers, at most `limit`.
fn wait_readers(done: &std::sync::mpsc::Receiver<()>, limit: Duration) {
    let until = Instant::now() + limit;
    for _ in 0..2 {
        let left = until.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// `Run`: starts the shortcut's program (its own process group, no shell, no window), then follows it on a thread of
/// its own until it ends, is stopped or runs out of time.
fn run(job: Arc<Job>, shortcut: &Shortcut, limit: Duration) {
    let args = shortcut.args.as_deref().unwrap_or("");
    let mut command = std::process::Command::new(&shortcut.cmd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if !args.is_empty() {
            command.raw_arg(args); // the command line as written, as .NET passes it
        }
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.args(crate::shortcuts::split_arguments(args));
        command.process_group(0); // the task and what it starts are one group: a stop ends them all, nothing else
    }
    let cwd = shortcut
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(crate::app_paths::neutral_cwd);
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            crate::log::debug(
                CATEGORY,
                &format!("Configured Control Deck task failed: {e}"),
            );
            job.set_state("Failed to start");
            job.finish();
            return;
        }
    };
    let pid = child.id();
    let output: Arc<Mutex<Output>> = Default::default();
    let (done_tx, done) = std::sync::mpsc::channel();
    for pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ] {
        let (output, job, done_tx) = (output.clone(), job.clone(), done_tx.clone());
        std::thread::spawn(move || match pipe {
            Some(pipe) => read_pipe(pipe, output, job, done_tx),
            None => {
                let _ = done_tx.send(());
            }
        });
    }
    job.set_state("Running");
    std::thread::spawn(move || {
        let deadline = Instant::now() + limit;
        let state = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // a daemon child can inherit a pipe after its parent exits: do not wait for that pipe for long
                    wait_readers(&done, Duration::from_secs(1));
                    let code = exit_code(status);
                    break if code == 0 {
                        "Completed".to_string()
                    } else {
                        format!("Failed ({code})")
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    crate::log::debug(
                        CATEGORY,
                        &format!("Configured Control Deck task failed: {e}"),
                    );
                    break "Failed to start".to_string();
                }
            }
            if job.stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
                crate::tool_process::kill_tree(Some(pid));
                let _ = child.kill();
                let until = Instant::now() + Duration::from_secs(5);
                while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(25));
                }
                lock(&output).closed = true;
                wait_readers(&done, Duration::from_secs(1));
                break if job.stop.load(Ordering::SeqCst) {
                    "Stopped".to_string()
                } else {
                    "Timed out".to_string()
                };
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let text = {
            let mut out = lock(&output);
            out.closed = true;
            out.text.clone()
        };
        {
            let mut st = lock(&job.state);
            st.0 = state;
            st.1 = text;
        }
        job.finish();
    });
}

/// The exit code as .NET reports it (128 + the signal for a process a signal ended).
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir, shortcuts: serde_json::Value) -> Arc<ConfigStore> {
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({ "shortcuts": shortcuts }).to_string(),
        )
        .unwrap();
        Arc::new(ConfigStore::new(path))
    }

    fn action(id: &str, target: &str, kind: &str) -> DeckAction {
        DeckAction {
            id: id.into(),
            target: target.into(),
            kind: kind.into(),
            ..DeckAction::default()
        }
    }

    /// The shell the C# tests use: `/bin/sh` here, `cmd.exe` on Windows.
    fn shell() -> String {
        if cfg!(windows) {
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(r"{system}\System32\cmd.exe")
        } else {
            "/bin/sh".into()
        }
    }

    fn wait_while_running(tasks: &ControlTasks, id: &str) {
        for _ in 0..100 {
            if !matches!(tasks.state(id).as_str(), "Running" | "Starting") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn tasks_report_output_and_reject_unknown_or_builtin_commands() {
        let dir = tempfile::tempdir().unwrap();
        let args = if cfg!(windows) {
            "/c echo owned-task-proof"
        } else {
            "-c \"printf owned-task-proof\""
        };
        let cwd = dir.path().to_string_lossy().into_owned();
        let tasks = ControlTasks::new(store(
            &dir,
            serde_json::json!([
                { "id": "proof", "cmd": shell(), "args": args, "cwd": cwd },
                { "id": "keys", "cmd": "builtin:keys", "args": "Ctrl+A", "cwd": cwd },
            ]),
        ));
        assert!(
            !tasks
                .start(&action("bad", "not-configured", "shortcut"), false)
                .ok
        );
        assert!(
            !tasks
                .start(&action("builtin", "keys", "shortcut"), false)
                .ok
        );
        assert!(tasks.start(&action("run", "proof", "shortcut"), false).ok);
        wait_while_running(&tasks, "run");
        assert_eq!(tasks.state("run"), "Completed");
        tasks.stop_all(Duration::from_secs(8));
        assert!(tasks.output("run").contains("owned-task-proof"));
        assert!(!tasks.start(&action("run", "proof", "shortcut"), true).ok);
        assert_eq!(tasks.state("never"), "Not started");
        assert_eq!(tasks.output("never"), "No task output");
    }

    #[test]
    fn stopping_an_owned_server_does_not_stop_an_unrelated_process() {
        let dir = tempfile::tempdir().unwrap();
        let args = if cfg!(windows) {
            "/c ping -n 60 127.0.0.1 > nul"
        } else {
            "-c \"sleep 60\""
        };
        let mut unrelated = std::process::Command::new(shell());
        if cfg!(windows) {
            unrelated.args(["/c", "ping", "-n", "60", "127.0.0.1"]);
        } else {
            unrelated.args(["-c", "sleep 60"]);
        }
        let mut unrelated = unrelated
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let tasks = ControlTasks::new(store(
            &dir,
            serde_json::json!([{ "id": "server", "cmd": shell(), "args": args, "cwd": cwd }]),
        ));
        let server = action("owned", "server", "server");
        assert!(tasks.start(&server, false).ok);
        assert!(!tasks.start(&server, false).ok);
        assert!(tasks.start(&server, true).ok);
        tasks.stop_all(Duration::from_secs(8));
        assert_eq!(tasks.state("owned"), "Stopped");
        assert!(unrelated.try_wait().unwrap().is_none());
        let _ = unrelated.kill();
        let _ = unrelated.wait();
    }

    /// `DeckAuditTests.ATaskThatCannotStartIsNotReportedAsStarted`.
    #[test]
    fn a_task_that_cannot_start_is_not_reported_as_started() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-build-tool.exe");
        let cwd = dir.path().to_string_lossy().into_owned();
        let tasks = ControlTasks::new(store(
            &dir,
            serde_json::json!([{ "id": "build", "cmd": missing.to_string_lossy(), "args": "", "cwd": cwd }]),
        ));
        let result = tasks.start(&action("run", "build", "task"), false);
        assert_eq!(tasks.state("run"), "Failed to start");
        assert!(!result.ok);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_task_reports_its_exit_code_and_keeps_only_the_end_of_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        // a start, 70 000 characters, an end, then exit 3
        let tasks = ControlTasks::new(store(
            &dir,
            serde_json::json!([{ "id": "noisy", "cmd": "/bin/sh", "cwd": cwd,
                "args": "-c \"printf start; head -c 70000 /dev/zero | tr '\\0' x; printf end; exit 3\"" }]),
        ));
        assert!(tasks.start(&action("run", "noisy", "task"), false).ok);
        wait_while_running(&tasks, "run");
        assert_eq!(tasks.state("run"), "Failed (3)");
        let output = tasks.output("run");
        assert_eq!(output.encode_utf16().count(), OUTPUT_KEEP);
        assert!(output.ends_with("xend") && !output.contains("start"));
    }
}
