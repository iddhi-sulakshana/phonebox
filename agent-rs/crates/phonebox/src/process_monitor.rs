//! Port of `ProcessMonitor.cs`: groups processes by name and measures their CPU (%) and memory use. Sampling only runs
//! while the phone is asking for the list, to avoid needless load.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcGroup {
    pub name: String,
    pub title: String,
    pub count: i32,
    pub cpu: f64,
    pub mem_mb: f64,
    pub locked: bool,
    pub has_icon: bool,
}

/// `session`: the agent session's name when the instance is one; `window`: the main window title; `args`: the
/// command-line arguments (without the executable), shortened.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcInstance {
    pub pid: u32,
    pub cpu: f64,
    pub mem_mb: f64,
    pub started_at: Option<i64>,
    pub session: Option<String>,
    pub window: Option<String>,
    pub args: Option<String>,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcDetail {
    pub name: String,
    pub title: String,
    pub locked: bool,
    pub instances: Vec<ProcInstance>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KillResult {
    pub killed: i32,
    pub failed: i32,
    pub error: Option<String>,
}

/// What `ProcessMonitor` reads from the session monitor (plan 5): an agent session's process, name and last activity.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSessionRef {
    pub pid: Option<u32>,
    pub title: String,
    pub updated: i64,
}

pub trait AgentSessions: Send + Sync {
    fn sessions(&self) -> Vec<AgentSessionRef>;
}

/// Until the session monitor is ported: no sessions.
pub struct NoSessions;
impl AgentSessions for NoSessions {
    fn sessions(&self) -> Vec<AgentSessionRef> {
        vec![]
    }
}

/// One process as listed: `cpu_ms` is its accumulated CPU time (None when it cannot be read), `mem` its working set in
/// bytes, `started_ms` its start in Unix ms.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcRow {
    pub pid: u32,
    pub name: String,
    pub cpu_ms: Option<u64>,
    pub mem: u64,
    pub started_ms: Option<i64>,
    pub parent: Option<u32>,
}

/// What one sample produces: the groups, each pid's CPU time (the next sample's baseline) and each pid's CPU %.
pub(crate) struct Sampled {
    pub groups: Vec<ProcGroup>,
    pub cpu_now: HashMap<u32, u64>,
    pub pid_cpu: HashMap<u32, f64>,
}

/// Process names compare without regard to case, as .NET's `StringComparer.OrdinalIgnoreCase` does.
fn fold(name: &str) -> String {
    name.to_uppercase()
}

/// Protected names: case-insensitive on Windows (its list), case-sensitive on Linux and macOS (UnixProc's list).
pub fn is_protected(name: &str) -> bool {
    if cfg!(windows) {
        WINDOWS_PROTECTED
            .iter()
            .any(|p| p.eq_ignore_ascii_case(name))
    } else {
        crate::unix::unix_proc::PROTECTED.contains(&name)
    }
}

/// Processes Windows needs, or whose termination would break the user session.
const WINDOWS_PROTECTED: [&str; 40] = [
    "System",
    "Idle",
    "Registry",
    "smss",
    "csrss",
    "wininit",
    "winlogon",
    "services",
    "lsass",
    "lsaiso",
    "svchost",
    "dwm",
    "fontdrvhost",
    "sihost",
    "ctfmon",
    "explorer",
    "taskhostw",
    "audiodg",
    "spoolsv",
    "Memory Compression",
    "Secure System",
    "LsaIso",
    "conhost",
    "dllhost",
    "WUDFHost",
    "SearchHost",
    "StartMenuExperienceHost",
    "ShellExperienceHost",
    "TextInputHost",
    "LockApp",
    "SecurityHealthService",
    "MsMpEng",
    "NisSrv",
    "smartscreen",
    "RuntimeBroker",
    "ApplicationFrameHost",
    "SystemSettingsBroker",
    "PhoneBoxAgent",
    "adb",
    "dotnet",
];

/// `Sample`'s grouping: rows by name (case-insensitive, under the first spelling seen), CPU % from the change in CPU
/// time over `elapsed` seconds across all cores, memory in MB; most CPU first, then most memory. Rows without a name
/// (the process ended while it was read) and pid 0 are left out. `meta` gives the (title, has a path) of a group from
/// its first pid.
pub(crate) fn group_rows(
    rows: &[ProcRow],
    last_cpu: &HashMap<u32, u64>,
    elapsed: f64,
    cores: usize,
    locked: impl Fn(&ProcRow) -> bool,
    mut meta: impl FnMut(u32, &str) -> (String, bool),
) -> Sampled {
    struct Acc {
        name: String,
        count: i32,
        cpu: f64,
        mem: u64,
        locked: bool,
        pid: u32,
    }
    let mut cpu_now = HashMap::new();
    let mut pid_cpu = HashMap::new();
    let mut acc: Vec<Acc> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    for r in rows {
        if r.pid == 0 || r.name.is_empty() {
            continue;
        }
        let mut cpu = 0.0;
        if let Some(t) = r.cpu_ms {
            cpu_now.insert(r.pid, t);
            if elapsed > 0.0
                && let Some(prev) = last_cpu.get(&r.pid)
            {
                let secs = (t as f64 - *prev as f64) / 1000.0;
                cpu = (secs / elapsed / cores.max(1) as f64 * 100.0).max(0.0);
            }
        }
        pid_cpu.insert(r.pid, cpu);
        let is_locked = locked(r);
        let i = *index.entry(fold(&r.name)).or_insert_with(|| {
            acc.push(Acc {
                name: r.name.clone(),
                count: 0,
                cpu: 0.0,
                mem: 0,
                locked: false,
                pid: r.pid,
            });
            acc.len() - 1
        });
        let a = &mut acc[i];
        a.count += 1;
        a.cpu += cpu;
        a.mem += r.mem;
        a.locked |= is_locked;
    }
    let mut groups: Vec<ProcGroup> = acc
        .into_iter()
        .map(|a| {
            let (title, has_icon) = meta(a.pid, &a.name);
            ProcGroup {
                title,
                count: a.count,
                cpu: round_dotnet(a.cpu, 1),
                mem_mb: round_dotnet(a.mem as f64 / 1048576.0, 0),
                locked: a.locked,
                has_icon,
                name: a.name,
            }
        })
        .collect();
    // stable, as LINQ's OrderByDescending().ThenByDescending()
    groups.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem_mb.total_cmp(&a.mem_mb)));
    Sampled {
        groups,
        cpu_now,
        pid_cpu,
    }
}

use crate::system_monitor::round_dotnet;

/// The process table as .NET's `Process.GetProcesses` sees it. `only` limits it to some pids.
fn list_processes(only: Option<&[u32]>) -> Vec<ProcRow> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let Ok(mut sys) = System::new() else {
        return vec![];
    };
    let pids: Vec<Pid> = only
        .unwrap_or_default()
        .iter()
        .map(|p| Pid::from_u32(*p))
        .collect();
    let which = if only.is_some() {
        ProcessesToUpdate::Some(&pids)
    } else {
        ProcessesToUpdate::All
    };
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet);
    sys.refresh_processes_specifics(which, true, kind);
    sys.processes()
        .values()
        .map(|p| {
            let pid = p.pid().as_u32();
            ProcRow {
                pid,
                name: dotnet_name(pid, &p.name().to_string_lossy()),
                cpu_ms: Some(p.accumulated_cpu_time()),
                mem: p.memory(),
                started_ms: (p.start_time() > 0).then(|| p.start_time() as i64 * 1000),
                parent: p.parent().map(|pp| pp.as_u32()),
            }
        })
        .collect()
}

/// .NET's `ProcessName`: on Windows without ".exe"; on Linux the kernel's 15-character name, or argv[0]'s file name
/// when the kernel name is a cut-off start of it.
fn dotnet_name(pid: u32, os_name: &str) -> String {
    if cfg!(windows) {
        let n = os_name.len();
        if n > 4 && os_name.is_char_boundary(n - 4) && os_name[n - 4..].eq_ignore_ascii_case(".exe")
        {
            return os_name[..n - 4].to_string();
        }
        return os_name.to_string();
    }
    if cfg!(any(target_os = "linux", target_os = "android")) && os_name.chars().count() == 15 {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        return untruncated_name(os_name, &cmdline);
    }
    os_name.to_string()
}

/// `Process.GetUntruncatedProcessName`: argv[0] (up to the first NUL) without its directories, if it starts with the
/// kernel's cut-off name.
fn untruncated_name(comm: &str, cmdline: &[u8]) -> String {
    let argv0 = cmdline.split(|b| *b == 0).next().unwrap_or_default();
    let start = argv0.iter().rposition(|b| *b == b'/').map_or(0, |i| i + 1);
    let name = String::from_utf8_lossy(&argv0[start..]);
    if name.starts_with(comm) {
        name.into_owned()
    } else {
        comm.to_string()
    }
}

/// The process's name as .NET's `Process.ProcessName` gives it, or None when there is no such process.
pub fn process_name(pid: u32) -> Option<String> {
    list_processes(Some(&[pid]))
        .into_iter()
        .find(|r| r.pid == pid)
        .map(|r| r.name)
}

/// .NET's `Process.GetProcessesByName`: every process with this name, compared without regard to case.
pub(crate) fn processes_named(name: &str) -> Vec<ProcRow> {
    let want = fold(name);
    list_processes(None)
        .into_iter()
        .filter(|r| fold(&r.name) == want)
        .collect()
}

pub struct ProcessMonitor {
    sessions: Arc<dyn AgentSessions>,
    self_pid: u32,
    cores: usize,
    /// (title, image path) by folded name; only entries with a path are kept, so a miss is retried.
    meta: Mutex<HashMap<String, (String, Option<String>)>>,
    /// PNG icons by folded name; a miss is never stored, so a process seen later still gets its icon.
    icons: Mutex<HashMap<String, Vec<u8>>>,
    args: Mutex<HashMap<u32, Option<String>>>,
    sampler: Mutex<Sampler>,
    pid_cpu: Mutex<Arc<HashMap<u32, f64>>>,
    groups: Mutex<Arc<Vec<ProcGroup>>>,
    /// Milliseconds since `epoch` of the last request; sampling runs for 15 s after one.
    last_request: AtomicI64,
    epoch: Instant,
}

#[derive(Default)]
struct Sampler {
    last_cpu: HashMap<u32, u64>,
    last_ts: Option<Instant>,
}

/// Sampling stops this long after the last request.
const ACTIVE_MS: i64 = 15_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ProcessMonitor {
    pub fn new(sessions: Arc<dyn AgentSessions>) -> Arc<ProcessMonitor> {
        Arc::new(ProcessMonitor {
            sessions,
            self_pid: std::process::id(),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            meta: Mutex::default(),
            icons: Mutex::default(),
            args: Mutex::default(),
            sampler: Mutex::default(),
            pid_cpu: Mutex::default(),
            groups: Mutex::default(),
            last_request: AtomicI64::new(-ACTIVE_MS),
            epoch: Instant::now(),
        })
    }

    fn now_ms(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }

    fn touch(&self) {
        self.last_request.store(self.now_ms(), Ordering::Relaxed);
    }

    /// The groups of the last sample; asking keeps sampling going for 15 s.
    pub fn snapshot(&self) -> Vec<ProcGroup> {
        self.touch();
        lock(&self.groups).as_ref().clone()
    }

    /// The sampling loop: every 2 s while the phone asks (a second sample 700 ms after a cold start, which has
    /// nothing to diff against), otherwise it forgets its state and checks back every 250 ms.
    pub fn start(self: &Arc<Self>, stop: crate::server::Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let pause = |ms: u64| {
                let stop = stop.clone();
                async move {
                    tokio::select! {
                        _ = stop.stopped() => false,
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => true,
                    }
                }
            };
            loop {
                if me.now_ms() - me.last_request.load(Ordering::Relaxed) < ACTIVE_MS {
                    if lock(&me.sampler).last_ts.is_none() {
                        me.sample_off_thread().await;
                        if !pause(700).await {
                            break;
                        }
                    }
                    me.sample_off_thread().await;
                    if !pause(2000).await {
                        break;
                    }
                } else {
                    if lock(&me.sampler).last_ts.is_some() {
                        *lock(&me.sampler) = Sampler::default(); // idle: start fresh on the next request
                        *lock(&me.pid_cpu) = Arc::default();
                        *lock(&me.groups) = Arc::default();
                        lock(&me.args).clear();
                    }
                    if !pause(250).await {
                        break;
                    }
                }
            }
        })
    }

    async fn sample_off_thread(self: &Arc<Self>) {
        let me = self.clone();
        let _ = tokio::task::spawn_blocking(move || me.sample()).await;
    }

    fn sample(&self) {
        let now = Instant::now();
        let mut sampler = lock(&self.sampler);
        let elapsed = sampler
            .last_ts
            .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        sampler.last_ts = Some(now);
        let rows = list_processes(None);
        let sampled = group_rows(
            &rows,
            &sampler.last_cpu,
            elapsed,
            self.cores,
            |r| self.locked(r.pid, &r.name),
            |pid, name| {
                let (title, path) = self.meta_for(pid, name);
                (title, path.is_some())
            },
        );
        sampler.last_cpu = sampled.cpu_now;
        *lock(&self.pid_cpu) = Arc::new(sampled.pid_cpu);
        *lock(&self.groups) = Arc::new(sampled.groups);
    }

    /// The agent itself, protected names, and processes of other sessions or users cannot be ended.
    fn locked(&self, pid: u32, name: &str) -> bool {
        pid == self.self_pid || is_protected(name) || !mine(pid)
    }

    /// The cached (title, path) of a name, read from `pid` when there is none with a path yet.
    fn meta_for(&self, pid: u32, name: &str) -> (String, Option<String>) {
        if let Some(m) = lock(&self.meta).get(&fold(name)).filter(|m| m.1.is_some()) {
            return m.clone();
        }
        let m = read_meta(pid, name);
        if m.1.is_some() {
            lock(&self.meta).insert(fold(name), m.clone());
        }
        m
    }

    /// Every instance of a process name, with what identifies each one.
    pub fn detail(&self, name: &str) -> ProcDetail {
        self.touch();
        let mut by_pid: HashMap<u32, (i64, String)> = HashMap::new();
        for s in self.sessions.sessions() {
            let Some(pid) = s.pid else { continue };
            // the most recently active session of a pid; on a tie the first one listed
            match by_pid.get(&pid) {
                Some((updated, _)) if *updated >= s.updated => {}
                _ => {
                    by_pid.insert(pid, (s.updated, s.title));
                }
            }
        }
        let cpu = lock(&self.pid_cpu).clone();
        let protected = is_protected(name);
        let mut list: Vec<ProcInstance> = processes_named(name)
            .into_iter()
            .map(|p| ProcInstance {
                pid: p.pid,
                cpu: round_dotnet(cpu.get(&p.pid).copied().unwrap_or(0.0), 1),
                mem_mb: round_dotnet(p.mem as f64 / 1048576.0, 0),
                started_at: p.started_ms,
                session: by_pid.get(&p.pid).map(|(_, t)| t.clone()),
                window: main_window_title(p.pid),
                args: lock(&self.args)
                    .entry(p.pid)
                    .or_insert_with(|| read_args(p.pid))
                    .clone(),
                locked: p.pid == self.self_pid || protected || !mine(p.pid),
            })
            .collect();
        let cached = lock(&self.meta)
            .get(&fold(name))
            .filter(|m| m.1.is_some())
            .cloned();
        let title = match (cached, list.first()) {
            (Some(m), _) => m.0,
            (None, Some(first)) => self.meta_for(first.pid, name).0,
            (None, None) => name.to_string(),
        };
        let locked = protected || (!list.is_empty() && list.iter().all(|i| i.locked));
        list.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem_mb.total_cmp(&a.mem_mb)));
        ProcDetail {
            name: name.to_string(),
            title,
            locked,
            instances: list,
        }
    }

    /// Ends every unprotected process with this name in the current user session.
    pub fn kill(&self, name: &str) -> KillResult {
        let result = |killed, failed, error: Option<&str>| KillResult {
            killed,
            failed,
            error: error.map(String::from),
        };
        if name.trim().is_empty() {
            return result(0, 0, Some("no name given"));
        }
        if is_protected(name) {
            return KillResult {
                killed: 0,
                failed: 0,
                error: Some(format!("{name} is protected and cannot be terminated")),
            };
        }
        let (mut killed, mut failed) = (0, 0);
        for p in processes_named(name) {
            if p.pid == self.self_pid || !mine(p.pid) {
                failed += 1;
            } else if end_process(p.pid).is_ok() {
                killed += 1;
            } else {
                failed += 1;
            }
        }
        crate::log::info(
            "ProcessMonitor",
            &format!("terminated all {name}: {killed} ok, {failed} failed"),
        );
        if killed == 0 && failed == 0 {
            return result(0, 0, Some("process not found"));
        }
        result(
            killed,
            failed,
            (killed == 0).then_some("access denied (it may be running as administrator)"),
        )
    }

    /// Ends one process together with its child processes. Every descendant passes the same checks as the process
    /// itself (protected names, other sessions or users, the agent): those are left running.
    pub fn kill_pid(&self, pid: u32) -> KillResult {
        let refuse = |error: String| KillResult {
            killed: 0,
            failed: 0,
            error: Some(error),
        };
        if pid == self.self_pid {
            return refuse("that is the PhoneBox agent itself".into());
        }
        let Some(name) = process_name(pid) else {
            return refuse("process not found".into());
        };
        if is_protected(&name) {
            return refuse(format!("{name} is protected and cannot be terminated"));
        }
        if !mine(pid) {
            return refuse("process belongs to another session or user".into());
        }
        let descendants = descendants(pid);
        if let Err(e) = end_process(pid) {
            return KillResult {
                killed: 0,
                failed: 1,
                error: Some(format!("could not terminate: {e}")),
            };
        }
        let (mut killed, mut skipped) = (1, 0);
        for d in descendants {
            let Some(child) = process_name(d) else {
                continue;
            }; // already gone
            if d == self.self_pid || is_protected(&child) || !mine(d) {
                skipped += 1;
            } else if end_process(d).is_ok() {
                killed += 1;
            } else {
                skipped += 1;
            }
        }
        crate::log::info(
            "ProcessMonitor",
            &format!(
                "terminated {name} (pid {pid}) and {} child processes ({skipped} left running)",
                killed - 1
            ),
        );
        KillResult {
            killed,
            failed: skipped,
            error: None,
        }
    }

    /// A PNG icon for the name (from the cache, which never stores a miss).
    pub fn icon(&self, name: &str) -> Option<Vec<u8>> {
        if let Some(png) = lock(&self.icons).get(&fold(name)) {
            return Some(png.clone());
        }
        let mut path = lock(&self.meta).get(&fold(name)).and_then(|m| m.1.clone());
        if path.is_none() {
            for p in processes_named(name) {
                path = self.meta_for(p.pid, name).1;
                if path.is_some() {
                    break;
                }
            }
        }
        let png = os_icon(name, path.as_deref());
        if let Some(png) = &png {
            lock(&self.icons).insert(fold(name), png.clone());
        }
        png
    }

    #[cfg(test)]
    fn icon_cached(&self, name: &str) -> bool {
        lock(&self.icons).contains_key(&fold(name))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn list_now(&self) -> Vec<ProcRow> {
        list_processes(None)
    }
}

/// Every process below `root`. A child counts only if it started after its parent, so a parent pid the OS has since
/// given to an unrelated process does not pull that process in.
pub fn descendants(root: u32) -> Vec<u32> {
    let rows = list_processes(None);
    let mut started: HashMap<u32, i64> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in &rows {
        let Some(start) = r.started_ms else { continue }; // not readable: not ours to end anyway
        started.insert(r.pid, start);
        if let Some(parent) = r.parent.filter(|p| *p != r.pid) {
            children.entry(parent).or_default().push(r.pid);
        }
    }
    let mut result = vec![];
    let mut seen = std::collections::HashSet::from([root]);
    let mut queue = std::collections::VecDeque::from([root]);
    while let Some(parent) = queue.pop_front() {
        let (Some(kids), Some(parent_start)) = (children.get(&parent), started.get(&parent)) else {
            continue;
        };
        for kid in kids {
            if started[kid] >= *parent_start && seen.insert(*kid) {
                result.push(*kid);
                queue.push_back(*kid);
            }
        }
    }
    result
}

/// The arguments of a command line without the executable (quoted or not), shortened to 140 characters.
pub fn shorten_args(cmd: Option<&str>) -> Option<String> {
    let cmd = cmd?.trim();
    if cmd.is_empty() {
        return None;
    }
    let rest = if let Some(quoted) = cmd.strip_prefix('"') {
        quoted.find('"').map_or("", |q| &quoted[q + 1..])
    } else {
        match cmd.find([' ', '\t']) {
            Some(s) if s > 0 => &cmd[s..],
            _ => "",
        }
    };
    crate::unix::unix_proc::shorten(rest.trim())
}

/// Windows: the same logon session as the agent. Linux / macOS: the same user.
fn mine(pid: u32) -> bool {
    #[cfg(windows)]
    {
        win::session_of(pid).is_some()
            && win::session_of(pid) == win::session_of(std::process::id())
    }
    #[cfg(not(windows))]
    {
        crate::unix::unix_proc::is_own(pid)
    }
}

/// (title, image path): the bundle or launcher name on Linux and macOS, the file description (40 characters at most)
/// on Windows; the process name otherwise.
fn read_meta(pid: u32, name: &str) -> (String, Option<String>) {
    #[cfg(windows)]
    {
        let path = win::image_path(pid);
        let title = path
            .as_deref()
            .and_then(win::file_description)
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty() && d.chars().count() <= 40)
            .unwrap_or_else(|| name.to_string());
        (title, path)
    }
    #[cfg(not(windows))]
    {
        let path = crate::unix::unix_proc::image_path(pid);
        (crate::unix::unix_proc::title(name, path.as_deref()), path)
    }
}

fn read_args(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        shorten_args(win::command_line(pid).as_deref())
    }
    #[cfg(not(windows))]
    {
        crate::unix::unix_proc::args(pid)
    }
}

/// The main window's title (Windows; there is none on Linux and macOS, as in .NET).
fn main_window_title(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        win::main_window_title(pid).filter(|t| !t.trim().is_empty())
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

fn os_icon(name: &str, path: Option<&str>) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        let _ = name;
        path.and_then(crate::win_icon::associated_icon_png)
    }
    #[cfg(not(windows))]
    {
        crate::unix::unix_proc::icon(name, path)
    }
}

/// `Process.Kill()`: SIGKILL on Unix, TerminateProcess on Windows. The error reads like .NET's message.
fn end_process(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid).map_err(|_| "No such process".to_string())?;
        // SAFETY: kill(2) on one pid; the result is checked
        if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(os_message(std::io::Error::last_os_error()))
        }
    }
    #[cfg(windows)]
    {
        win::terminate(pid).map_err(os_message)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("not supported".into())
    }
}

/// An OS error as .NET words it: without Rust's " (os error N)".
fn os_message(e: std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsWindowVisible,
    };
    use windows::core::{BOOL, PCWSTR, PWSTR};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn open(
        pid: u32,
        access: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS,
    ) -> Option<Handle> {
        // SAFETY: plain OpenProcess; the handle is owned by the guard
        unsafe { OpenProcess(access, false, pid) }.ok().map(Handle)
    }

    pub(super) fn session_of(pid: u32) -> Option<u32> {
        let mut session = 0u32;
        // SAFETY: `session` is a valid out pointer
        unsafe { ProcessIdToSessionId(pid, &mut session) }
            .ok()
            .map(|_| session)
    }

    pub(super) fn image_path(pid: u32) -> Option<String> {
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        // SAFETY: the buffer holds `size` UTF-16 units
        unsafe {
            QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size)
        }
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }

    /// `FileVersionInfo.FileDescription`: the first language the file lists, else the usual English code pages.
    pub(super) fn file_description(path: &str) -> Option<String> {
        use windows::Win32::Storage::FileSystem::{
            GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
        };
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated; the data buffer has the size the first call reported; VerQueryValueW
        // returns pointers into that buffer, which outlives their use
        unsafe {
            let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast())
                .ok()?;
            let query = |sub: &str| -> Option<(*const u8, u32)> {
                let sub: Vec<u16> = sub.encode_utf16().chain([0]).collect();
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                VerQueryValueW(
                    data.as_ptr().cast(),
                    PCWSTR(sub.as_ptr()),
                    &mut ptr,
                    &mut len,
                )
                .as_bool()
                .then_some((ptr as *const u8, len))
            };
            let mut codes = vec![];
            if let Some((ptr, len)) = query("\\VarFileInfo\\Translation")
                && len >= 4
            {
                let lang = u16::from_le_bytes([*ptr, *ptr.add(1)]);
                let page = u16::from_le_bytes([*ptr.add(2), *ptr.add(3)]);
                codes.push(format!("{lang:04X}{page:04X}"));
            }
            codes.extend(["040904B0", "040904E4", "04090000"].map(String::from));
            for code in codes {
                if let Some((ptr, len)) =
                    query(&format!("\\StringFileInfo\\{code}\\FileDescription"))
                    && len > 0
                {
                    let units = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
                    return Some(String::from_utf16_lossy(&units[..end]));
                }
            }
            None
        }
    }

    /// The command line from ProcessCommandLineInformation (60): a UNICODE_STRING followed by its text.
    pub(super) fn command_line(pid: u32) -> Option<String> {
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtQueryInformationProcess(
                h: HANDLE,
                class: u32,
                info: *mut u8,
                len: u32,
                ret: *mut u32,
            ) -> i32;
        }
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut len = 0u32;
        // SAFETY: a zero-length query only reports the size; the second call gets a buffer of that size (u64s keep
        // the UNICODE_STRING's pointer aligned), whose Buffer pointer points inside it
        unsafe {
            NtQueryInformationProcess(h.0, 60, std::ptr::null_mut(), 0, &mut len);
            if len == 0 || len > 1 << 20 {
                return None;
            }
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if NtQueryInformationProcess(h.0, 60, buf.as_mut_ptr().cast(), len, &mut len) != 0 {
                return None;
            }
            let bytes = buf.as_ptr() as *const u8;
            let chars = u16::from_le_bytes([*bytes, *bytes.add(1)]) as usize / 2;
            let text = *(bytes.add(std::mem::size_of::<usize>()) as *const *const u16);
            if text.is_null() {
                return None;
            }
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                text, chars,
            )))
        }
    }

    /// .NET's `MainWindowTitle`: the first visible top-level window of the process that has no owner.
    pub(super) fn main_window_title(pid: u32) -> Option<String> {
        struct Search {
            pid: u32,
            found: Option<HWND>,
        }
        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: lparam is the &mut Search passed to EnumWindows below, alive for the whole call
            let search = unsafe { &mut *(lparam.0 as *mut Search) };
            let mut owner_pid = 0u32;
            // SAFETY: plain window queries on a window handle EnumWindows passed in
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
                if owner_pid == search.pid
                    && IsWindowVisible(hwnd).as_bool()
                    && GetWindow(hwnd, GW_OWNER).map_or(true, |o| o.0.is_null())
                {
                    search.found = Some(hwnd);
                    return BOOL(0);
                }
            }
            BOOL(1)
        }
        let mut search = Search { pid, found: None };
        // SAFETY: `search` outlives the enumeration; the callback only reads and writes it
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
        let hwnd = search.found?;
        // SAFETY: the buffer holds len + 1 units
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            Some(String::from_utf16_lossy(&buf[..n.max(0) as usize]))
        }
    }

    pub(super) fn terminate(pid: u32) -> Result<(), std::io::Error> {
        let h = open(pid, PROCESS_TERMINATE).ok_or_else(std::io::Error::last_os_error)?;
        // SAFETY: a process handle opened with PROCESS_TERMINATE; exit code -1 as .NET's Process.Kill
        unsafe { TerminateProcess(h.0, u32::MAX) }
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xffff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSessions(Vec<AgentSessionRef>);
    impl AgentSessions for FakeSessions {
        fn sessions(&self) -> Vec<AgentSessionRef> {
            self.0.clone()
        }
    }

    fn monitor() -> Arc<ProcessMonitor> {
        ProcessMonitor::new(Arc::new(NoSessions))
    }

    #[test]
    fn process_detail_tolerates_duplicate_session_pids_and_locks_agent_process() {
        let pid = std::process::id();
        let sessions = FakeSessions(vec![
            AgentSessionRef {
                pid: Some(pid),
                title: "older".into(),
                updated: 1000,
            },
            AgentSessionRef {
                pid: Some(pid),
                title: "newest".into(),
                updated: 5000,
            },
        ]);
        let procs = ProcessMonitor::new(Arc::new(sessions));
        let name = process_name(pid).expect("own process name");
        let detail = procs.detail(&name);
        let mine: Vec<&ProcInstance> = detail.instances.iter().filter(|i| i.pid == pid).collect();
        assert_eq!(mine.len(), 1, "{detail:?}");
        assert_eq!(mine[0].session.as_deref(), Some("newest"));
        assert!(mine[0].locked);
        // the group lock depends on every same-name process (another test binary may be running), so only check that
        // it follows the instances it was computed from
        if detail.instances.iter().all(|i| i.locked) {
            assert!(detail.locked);
        }
    }

    #[test]
    fn cold_process_icon_does_not_cache_null_and_shorten_args_handles_tab_separators() {
        let procs = monitor();
        let missing = "missing-proc-f390";
        assert_eq!(procs.icon(missing), None);
        assert!(!procs.icon_cached(missing));

        assert_eq!(
            shorten_args(Some("tool.exe\t--daemon")).as_deref(),
            Some("--daemon")
        );
        assert_eq!(
            shorten_args(Some("\"C:\\Program Files\\My App\\tool.exe\"\t--port 9000")).as_deref(),
            Some("--port 9000")
        );
        assert_eq!(shorten_args(Some("tool.exe")), None);
        assert_eq!(shorten_args(None), None);
        assert_eq!(shorten_args(Some("   ")), None);
        let long = format!("tool {}", "x".repeat(200));
        assert_eq!(
            shorten_args(Some(&long)),
            Some(format!("{}…", "x".repeat(137)))
        );
    }

    /// A child we may end: `sleep 60`, or `sh -c 'sleep 60 & wait'` for a parent with a child of its own.
    #[cfg(unix)]
    fn spawn(cmd: &str, args: &[&str]) -> std::process::Child {
        std::process::Command::new(cmd).args(args).spawn().unwrap()
    }

    #[cfg(unix)]
    fn wait_until(what: &str, check: impl Fn() -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !check() {
            assert!(
                std::time::Instant::now() < end,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // Review Focus 2: a process that is gone by the time it would be read is left out, without failing the rest.
    #[cfg(unix)]
    #[test]
    fn a_process_that_exits_while_listing_is_left_out() {
        let procs = monitor();
        let mut child = spawn("sleep", &["60"]);
        let pid = child.id();
        let name = process_name(pid).expect("the child's name");
        assert!(procs.detail(&name).instances.iter().any(|i| i.pid == pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(process_name(pid).is_none());
        assert!(!procs.detail(&name).instances.iter().any(|i| i.pid == pid));
        assert!(!procs.list_now().iter().any(|r| r.pid == pid));
        // a row whose read came back empty (no name) is dropped from the groups
        let rows = vec![
            ProcRow {
                pid: 10,
                name: String::new(),
                cpu_ms: Some(5),
                mem: 1 << 20,
                started_ms: None,
                parent: None,
            },
            ProcRow {
                pid: 11,
                name: "worker".into(),
                cpu_ms: Some(5),
                mem: 1 << 20,
                started_ms: None,
                parent: None,
            },
        ];
        let sampled = group_rows(
            &rows,
            &Default::default(),
            0.0,
            1,
            |_| false,
            |_, n| (n.to_string(), false),
        );
        assert_eq!(
            sampled
                .groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["worker"]
        );
    }

    // Review Focus 3
    #[test]
    fn kill_refuses_the_agent_and_protected_processes() {
        let procs = monitor();
        let me = procs.kill_pid(std::process::id());
        assert_eq!((me.killed, me.failed), (0, 0));
        assert_eq!(
            me.error.as_deref(),
            Some("that is the PhoneBox agent itself")
        );
        let agent = procs.kill("PhoneBoxAgent");
        assert_eq!(
            agent.error.as_deref(),
            Some("PhoneBoxAgent is protected and cannot be terminated")
        );
        assert_eq!(
            procs.kill("adb").error.as_deref(),
            Some("adb is protected and cannot be terminated")
        );
        assert_eq!(procs.kill("  ").error.as_deref(), Some("no name given"));
        assert_eq!(
            procs.kill("missing-proc-f390").error.as_deref(),
            Some("process not found")
        );
        assert_eq!(
            procs.kill_pid(u32::MAX - 7).error.as_deref(),
            Some("process not found")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ending_a_process_ends_its_children_too() {
        let procs = monitor();
        let mut parent = spawn("sh", &["-c", "sleep 60 & wait"]);
        let pid = parent.id();
        wait_until("the child sleep", || !descendants(pid).is_empty());
        let kids = descendants(pid);
        let r = procs.kill_pid(pid);
        assert_eq!(
            (r.killed, r.failed, r.error),
            (1 + kids.len() as i32, 0, None)
        );
        parent.wait().unwrap();
        wait_until("the child to end", || {
            kids.iter().all(|k| process_name(*k).is_none())
        });
    }

    #[test]
    fn linux_names_cut_by_the_kernel_come_back_from_argv0() {
        let cmd = b"/usr/libexec/gnome-session-binary\0--session=ubuntu\0";
        assert_eq!(
            untruncated_name("gnome-session-b", cmd),
            "gnome-session-binary"
        );
        assert_eq!(untruncated_name("kworker/0:1-eve", b""), "kworker/0:1-eve");
        // argv[0] that does not start with the kernel's name is not used
        assert_eq!(
            untruncated_name("python3.12-wrap", b"/usr/bin/python3\0x\0"),
            "python3.12-wrap"
        );
    }

    #[test]
    fn group_order_and_rounding_follow_csharp() {
        let rows = vec![
            ProcRow {
                pid: 1,
                name: "a".into(),
                cpu_ms: Some(1500),
                mem: 3 << 20,
                started_ms: None,
                parent: None,
            },
            ProcRow {
                pid: 2,
                name: "B".into(),
                cpu_ms: Some(1000),
                mem: 1 << 20,
                started_ms: None,
                parent: None,
            },
            ProcRow {
                pid: 3,
                name: "b".into(),
                cpu_ms: Some(1000),
                mem: (1 << 20) + (1 << 19),
                started_ms: None,
                parent: None,
            },
        ];
        let last: std::collections::HashMap<u32, u64> = [(1, 1000), (2, 0), (3, 1000)].into();
        let mut meta_pids = vec![];
        // 2 s elapsed on 2 cores: 0.5 s of CPU is 12.5 %, 1 s is 25 %
        let sampled = group_rows(
            &rows,
            &last,
            2.0,
            2,
            |r| r.name == "b",
            |pid, name| {
                meta_pids.push(pid);
                (format!("{name} app"), pid == 1)
            },
        );
        let groups: Vec<(&str, &str, i32, f64, f64, bool, bool)> = sampled
            .groups
            .iter()
            .map(|g| {
                (
                    g.name.as_str(),
                    g.title.as_str(),
                    g.count,
                    g.cpu,
                    g.mem_mb,
                    g.locked,
                    g.has_icon,
                )
            })
            .collect();
        // names group without regard to case under the first spelling seen, whose first pid gives the metadata;
        // CPU first, then memory (2.5 MB rounds to 2, half to even)
        assert_eq!(
            groups,
            [
                ("B", "B app", 2, 25.0, 2.0, true, false),
                ("a", "a app", 1, 12.5, 3.0, false, true)
            ]
        );
        assert_eq!(meta_pids, [1, 2]);
        assert_eq!(sampled.cpu_now, [(1, 1500), (2, 1000), (3, 1000)].into());
        assert_eq!(sampled.pid_cpu, [(1, 12.5), (2, 25.0), (3, 0.0)].into());
        // the first sample has nothing to diff against: 0 %
        let first = group_rows(
            &rows,
            &Default::default(),
            0.0,
            2,
            |_| false,
            |_, n| (n.to_string(), false),
        );
        assert!(first.groups.iter().all(|g| g.cpu == 0.0));
    }
}
