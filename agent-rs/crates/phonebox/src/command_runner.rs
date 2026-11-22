//! Port of `CommandRunner.cs`: runs the commands of "command" widgets. Commands live only in config.json on this PC;
//! the phone asks for a widget by page and widget id and gets the cached output. A command runs at most once per its
//! interval, and only while a phone is looking at it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, WidgetConfig};

const MAX_OUTPUT: usize = 4000;
const TIMEOUT: Duration = Duration::from_secs(20);
/// Kept of each stream; the rest is read and dropped.
const CAP: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    pub output: String,
    pub at: i64,
    pub ok: bool,
    pub running: bool,
    pub duration_ms: f64,
}

/// Starts a run in the background; tests replace it to hold a run between its request and its start.
pub type StartWorker = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

pub struct CommandRunner {
    cfg: Arc<ConfigStore>,
    home: PathBuf,
    results: Mutex<HashMap<String, CommandResult>>,
    running: Mutex<HashSet<String>>,
    start_worker: StartWorker,
}

impl CommandRunner {
    /// Commands run in `home` (the user's folder) when it exists.
    pub fn new(cfg: Arc<ConfigStore>, home: PathBuf) -> Self {
        CommandRunner {
            cfg,
            home,
            results: Mutex::new(HashMap::new()),
            running: Mutex::new(HashSet::new()),
            start_worker: Arc::new(|work| {
                tokio::spawn(work);
            }),
        }
    }

    pub fn with_start_worker(mut self, start_worker: StartWorker) -> Self {
        self.start_worker = start_worker;
        self
    }

    /// The cached result (possibly stale, or empty while the first run is in progress), or None when the page is
    /// hidden or has no such command widget. A stale (or forced) result starts a run.
    pub fn get(
        self: &Arc<Self>,
        page_id: &str,
        widget_id: &str,
        force: bool,
    ) -> Option<CommandResult> {
        let cfg = self.cfg.get();
        let widget = find_widget(&cfg, page_id, widget_id)?;
        let command = resolve_command(widget);
        let interval = widget.int("interval").unwrap_or(30).clamp(5, 86400);
        let prefix = format!("{page_id}/{widget_id}:");
        let key = key(page_id, widget_id, &command);
        // drop the results of commands this widget no longer has, against the settings as they are now: a request
        // that read an older command must not evict the result of the current one
        let live = self.cfg.get();
        let live_key = find_widget(&live, page_id, widget_id)
            .map(|w| self::key(page_id, widget_id, &resolve_command(w)));
        let cached = {
            let mut results = lock(&self.results);
            results.retain(|k, _| {
                !k.starts_with(&prefix) || *k == key || live_key.as_deref() == Some(k.as_str())
            });
            results.get(&key).cloned()
        };
        let now = crate::server::now_ms();
        let stale = cached
            .as_ref()
            .is_none_or(|c| now - c.at < 0 || now - c.at >= i64::from(interval) * 1000);
        if (stale || force) && !command.trim().is_empty() && lock(&self.running).insert(key.clone())
        {
            let me = self.clone();
            let (page, widget, cmd, k) = (
                page_id.to_string(),
                widget_id.to_string(),
                command.clone(),
                key.clone(),
            );
            (self.start_worker)(Box::pin(async move {
                let _finish = Finish(me.clone(), k.clone());
                // checked again when the worker starts (the page may have been hidden, or the command changed, while
                // this run waited), and once more for its result
                if !me.still_current(&page, &widget, &cmd) {
                    return;
                }
                let result = run_command(&cmd, &me.home).await;
                if me.still_current(&page, &widget, &cmd) {
                    lock(&me.results).insert(k, result);
                }
            }));
        }
        let running = lock(&self.running).contains(&key);
        Some(match cached {
            None => CommandResult {
                output: if command.trim().is_empty() {
                    "(no command)"
                } else {
                    ""
                }
                .into(),
                at: 0,
                ok: true,
                running,
                duration_ms: 0.0,
            },
            Some(cached) => CommandResult { running, ..cached },
        })
    }

    fn still_current(&self, page_id: &str, widget_id: &str, command: &str) -> bool {
        find_widget(&self.cfg.get(), page_id, widget_id)
            .is_some_and(|w| resolve_command(w) == command)
    }
}

/// Takes a run off the running list however it ends.
struct Finish(Arc<CommandRunner>, String);

impl Drop for Finish {
    fn drop(&mut self) {
        lock(&self.0.running).remove(&self.1);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A command widget at the top level of an enabled page (Smart Stack children never run commands).
fn find_widget<'a>(
    cfg: &'a AgentConfig,
    page_id: &str,
    widget_id: &str,
) -> Option<&'a WidgetConfig> {
    cfg.pages
        .iter()
        .find(|p| p.enabled && p.id == page_id)?
        .widgets
        .as_ref()?
        .iter()
        .find(|w| w.id == widget_id && w.r#type == "command")
}

/// `commandUnix` when it is set and this is not Windows, else `command`.
fn resolve_command(w: &WidgetConfig) -> String {
    if !cfg!(windows)
        && let Some(unix) = w.str("commandUnix")
        && !unix.trim().is_empty()
    {
        return unix.to_string();
    }
    w.str("command").unwrap_or("").to_string()
}

/// The cache key of a widget's command.
pub fn key(page_id: &str, widget_id: &str, command: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    command.hash(&mut hash);
    format!("{page_id}/{widget_id}:{:016x}", hash.finish())
}

/// Over `max` UTF-16 units: cut there (a pair that would be split is left out) and end with "…".
pub fn truncate_output(output: &str, max: usize) -> String {
    crate::dotnet_text::cut(output, max)
}

/// One run through the user's shell (a login shell on macOS and Linux), 20 s at most.
pub async fn run_command(command: &str, home: &Path) -> CommandResult {
    let clock = std::time::Instant::now();
    let result = |output: String, ok: bool| CommandResult {
        output,
        at: crate::server::now_ms(),
        ok,
        running: false,
        duration_ms: clock.elapsed().as_secs_f64() * 1000.0,
    };
    match run(command, home).await {
        Ok(Some((ok, stdout, stderr))) => {
            let mut output = stdout.trim_end().to_string();
            let err = stderr.trim();
            if output.is_empty() && !err.is_empty() {
                output = err.to_string();
            }
            result(truncate_output(&output, MAX_OUTPUT), ok)
        }
        Ok(None) => result(format!("timed out after {} s", TIMEOUT.as_secs()), false),
        Err(e) => {
            crate::log::warn(CATEGORY, &format!("command widget failed: {e}"));
            result(e, false)
        }
    }
}

const CATEGORY: &str = "PhoneBox.Agent.CommandRunner";

/// The run: (exit code 0, output, error output), or None when it timed out and its tree was killed.
async fn run(command: &str, home: &Path) -> Result<Option<(bool, String, String)>, String> {
    let (shell, args): (&str, Vec<String>) = if cfg!(windows) {
        let script = format!(
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $ProgressPreference='SilentlyContinue'; {command}"
        );
        let mut args: Vec<String> = [
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ]
        .map(String::from)
        .to_vec();
        args.push(script);
        ("powershell.exe", args)
    } else {
        // a login shell: the user's PATH (docker, brew…) is there
        let shell = if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        (shell, vec!["-lc".to_string(), command.to_string()])
    };
    let cwd = if home.is_dir() {
        home.to_path_buf()
    } else {
        crate::app_paths::neutral_cwd()
    };
    let mut cmd = tokio::process::Command::new(shell);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0); // the tree is the group: one kill ends the children too
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    let (out, err) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut pump_out = tokio::spawn(pump(child.stdout.take(), out.clone()));
    let mut pump_err = tokio::spawn(pump(child.stderr.take(), err.clone()));
    let status = match tokio::time::timeout(TIMEOUT, child.wait()).await {
        Ok(status) => status.map_err(|e| e.to_string())?,
        Err(_) => {
            crate::tool_process::kill_tree(pid);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            pump_out.abort();
            pump_err.abort();
            return Ok(None);
        }
    };
    // The shell is done. A process it left running in the background (start /b, cmd &) inherits the output pipe and
    // may keep it open for ever: wait a moment for the rest of the output, not for that process.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        let _ = (&mut pump_out).await;
        let _ = (&mut pump_err).await;
    })
    .await;
    pump_out.abort();
    pump_err.abort();
    Ok(Some((status.success(), text(&out), text(&err))))
}

/// Reads a stream to its end, keeping the first [`CAP`] bytes.
async fn pump(stream: Option<impl tokio::io::AsyncRead + Unpin>, buf: Arc<Mutex<Vec<u8>>>) {
    use tokio::io::AsyncReadExt;
    let Some(mut stream) = stream else { return };
    let mut chunk = [0u8; 8192];
    while let Ok(n) = stream.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut kept = lock(&buf);
        let room = CAP.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// The captured bytes as text; at the cap, a character cut off at the end is left out.
fn text(buf: &Mutex<Vec<u8>>) -> String {
    let bytes = lock(buf);
    let len = if bytes.len() == CAP {
        trim_incomplete_utf8_tail(&bytes)
    } else {
        bytes.len()
    };
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

fn trim_incomplete_utf8_tail(span: &[u8]) -> usize {
    let mut cont = 0;
    while cont < 3 && cont < span.len() && span[span.len() - 1 - cont] & 0xC0 == 0x80 {
        cont += 1;
    }
    if cont >= span.len() {
        return span.len();
    }
    let lead_index = span.len() - 1 - cont;
    let lead = span[lead_index];
    if lead < 0x80 {
        return span.len();
    }
    let needed = if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        1
    };
    if cont + 1 < needed {
        lead_index
    } else {
        span.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn ps(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    fn sh(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    async fn wait_for(what: &str, mut done: impl FnMut() -> bool, limit: Duration) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < limit, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The configured command only writes an owned marker; its atomic rename shows that it ran.
    struct Fixture {
        dir: tempfile::TempDir,
        store: Arc<ConfigStore>,
        runner: Arc<CommandRunner>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(ConfigStore::new(dir.path().join("config.json")));
            let runner = Arc::new(CommandRunner::new(store.clone(), dir.path().into()));
            Fixture { dir, store, runner }
        }

        fn marker(&self) -> PathBuf {
            self.dir.path().join("ran")
        }

        fn ran(&self) -> bool {
            self.marker().exists() || self.dir.path().join("ran.tmp").exists()
        }

        fn configure(&self, enabled: bool) {
            let (marker, tmp) = (
                self.marker().display().to_string(),
                self.dir.path().join("ran.tmp").display().to_string(),
            );
            let command = format!(
                "[IO.File]::WriteAllText({},[string]$PID); [IO.File]::Move({},{})",
                ps(&tmp),
                ps(&tmp),
                ps(&marker)
            );
            let command_unix = format!(
                "printf '%s' $$ > {}; mv {} {}",
                sh(&tmp),
                sh(&tmp),
                sh(&marker)
            );
            let json = serde_json::json!({"pages": [
                {"id": "home", "enabled": true, "widgets": [{"type": "clock"}]},
                {"id": "commands", "enabled": enabled, "widgets": [
                    {"id": "probe", "type": "command", "options": {"command": command, "commandUnix": command_unix}}]},
            ]});
            self.store.save(&json.to_string(), None).unwrap();
        }

        async fn try_execute(&self) -> bool {
            if self.runner.get("commands", "probe", false).is_none() {
                return false;
            }
            wait_for(
                "the marker",
                || self.marker().exists(),
                Duration::from_secs(10),
            )
            .await;
            let pid: i32 = std::fs::read_to_string(self.marker())
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let start = Instant::now();
            while crate::session_monitor::alive(pid) && start.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            true
        }
    }

    #[tokio::test]
    async fn a_hidden_page_cannot_run_its_command_widget() {
        let f = Fixture::new();
        f.configure(false);
        assert!(!f.try_execute().await);
    }

    #[tokio::test]
    async fn hiding_a_previously_visible_command_revokes_access() {
        let f = Fixture::new();
        f.configure(true);
        assert!(f.try_execute().await);
        f.configure(false);
        assert_eq!(f.runner.get("commands", "probe", false), None);
    }

    // A run queued while its page was visible must not start after the page was hidden.
    #[tokio::test]
    async fn a_run_queued_before_the_page_was_hidden_does_not_start() {
        let f = Fixture::new();
        let queued: Arc<Mutex<Option<BoxFuture<'static, ()>>>> = Arc::new(Mutex::new(None));
        let hold = queued.clone();
        let runner = Arc::new(
            CommandRunner::new(f.store.clone(), f.dir.path().into())
                .with_start_worker(Arc::new(move |work| *hold.lock().unwrap() = Some(work))),
        );
        f.configure(true);
        assert!(runner.get("commands", "probe", false).is_some());
        let work = queued.lock().unwrap().take().expect("a run was queued");

        f.configure(false);
        tokio::time::timeout(Duration::from_secs(30), work)
            .await
            .unwrap();
        assert!(!f.ran());
        assert!(runner.running.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retired_in_flight_command_does_not_publish_clock_rollback_refreshes_and_surrogates_stay_intact()
     {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        let started = dir.path().join("started");
        let release = dir.path().join("release");
        let make_json = |enabled: bool, cmd: &str, cmd_unix: &str| {
            serde_json::json!({"pages": [
                {"id": "home", "title": "Home", "enabled": true, "widgets": [{"id": "clk", "type": "clock"}]},
                {"id": "cmds", "title": "Cmds", "enabled": enabled, "widgets": [
                    {"id": "w1", "type": "command", "options": {"command": cmd, "commandUnix": cmd_unix, "interval": 60}}]},
            ]})
            .to_string()
        };
        let (s, r) = (started.display().to_string(), release.display().to_string());
        let win_cmd = format!(
            "[IO.File]::WriteAllText({},'1'); while (-not [IO.File]::Exists({})) {{ Start-Sleep -Milliseconds 10 }}; Write-Output 'retired-secret'",
            ps(&s),
            ps(&r)
        );
        let unix_cmd = format!(
            "printf '1' > {}; while [ ! -f {} ]; do sleep 0.01; done; printf 'retired-secret\\n'",
            sh(&s),
            sh(&r)
        );

        std::fs::write(&cfg_path, make_json(true, &win_cmd, &unix_cmd)).unwrap();
        let store = Arc::new(ConfigStore::new(cfg_path));
        let runner = Arc::new(CommandRunner::new(store.clone(), dir.path().into()));
        let _ = runner.get("cmds", "w1", false);
        wait_for(
            "the command to start",
            || started.exists(),
            Duration::from_secs(10),
        )
        .await;

        store
            .save(&make_json(false, &win_cmd, &unix_cmd), None)
            .unwrap();
        std::fs::write(&release, "go").unwrap();
        wait_for(
            "the worker",
            || runner.running.lock().unwrap().is_empty(),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            runner.results.lock().unwrap().is_empty(),
            "the retired command's output was discarded"
        );

        // a clock set back (`at` in the future) marks the cached result stale and starts a run
        store
            .save(
                &make_json(true, "Write-Output 'ok'", "printf 'ok\\n'"),
                None,
            )
            .unwrap();
        let resolved = if cfg!(windows) {
            "Write-Output 'ok'"
        } else {
            "printf 'ok\\n'"
        };
        let k = key("cmds", "w1", resolved);
        let future = crate::server::now_ms() + 3_600_000;
        runner.results.lock().unwrap().insert(
            k.clone(),
            CommandResult {
                output: "before-rollback".into(),
                at: future,
                ok: true,
                running: false,
                duration_ms: 1.0,
            },
        );
        let _ = runner.get("cmds", "w1", false).unwrap();
        wait_for(
            "the rerun",
            || {
                runner.running.lock().unwrap().is_empty()
                    && runner
                        .results
                        .lock()
                        .unwrap()
                        .get(&k)
                        .is_some_and(|r| r.output == "ok")
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(runner.results.lock().unwrap()[&k].ok);

        // a surrogate pair at the limit is never split into a lone high surrogate
        let truncated = truncate_output(&format!("{}🚀tail", "a".repeat(3999)), 4000);
        assert_eq!(crate::dotnet_text::utf16_len(&truncated), 4000);
        assert_eq!(truncated, format!("{}…", "a".repeat(3999)));
    }

    #[tokio::test]
    async fn a_background_process_left_by_the_command_does_not_block_the_widget() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("background.pid");
        let worker = dir.path().join("background.ps1");
        std::fs::write(
            &worker,
            format!(
                "[IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 30",
                pid_path.display().to_string().replace('\'', "''")
            ),
        )
        .unwrap();
        let command = format!(
            "cmd /c start /b powershell.exe -NoProfile -NonInteractive -File \"{}\"",
            worker.display()
        );
        let command_unix = format!(
            "/bin/sleep 30 & printf '%s' $! > {}; echo hi",
            sh(&pid_path.display().to_string())
        );
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({"pages": [{"id": "p", "title": "P", "widgets": [
                {"id": "bg", "type": "command", "x": 0, "y": 0, "w": 6, "h": 3,
                 "options": {"command": command, "commandUnix": command_unix, "interval": 60}}]}]})
            .to_string(),
        )
        .unwrap();
        let runner = Arc::new(CommandRunner::new(
            Arc::new(ConfigStore::new(path)),
            dir.path().into(),
        ));
        let clock = Instant::now();
        let _ = runner.get("p", "bg", false);
        let mut result = None;
        while clock.elapsed() < Duration::from_secs(15) {
            result = runner.get("p", "bg", false);
            if result.as_ref().is_some_and(|r| r.at > 0 && !r.running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let elapsed = clock.elapsed();
        if let Ok(text) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            crate::tool_process::kill_tree(Some(pid));
        }
        let result = result.unwrap();
        assert!(
            result.at > 0 && !result.running,
            "no result after {elapsed:?} (the background process holds the pipe)"
        );
        assert!(elapsed < Duration::from_secs(12), "took {elapsed:?}");
        if !cfg!(windows) {
            assert_eq!((result.output.as_str(), result.ok), ("hi", true));
        }
    }

    #[tokio::test]
    async fn output_falls_back_to_errors_and_failures_are_not_ok() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, expected) = if cfg!(windows) {
            ("[Console]::Error.WriteLine('broken'); exit 3", "broken")
        } else {
            ("echo broken >&2; exit 3", "broken")
        };
        let result = run_command(cmd, dir.path()).await;
        assert_eq!(
            (result.output.as_str(), result.ok, result.running),
            (expected, false, false)
        );
        assert!(result.at > 0 && result.duration_ms > 0.0);
        let missing = run_command("echo hi", &dir.path().join("no-such-folder")).await;
        assert!(
            missing.ok,
            "a missing home falls back to the neutral folder: {missing:?}"
        );
    }
}
