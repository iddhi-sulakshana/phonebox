//! Port of `DockerMonitor.cs`: Docker (or Podman) containers for the Docker widget — `ps -a` every few seconds and
//! `stats` every 15 seconds, only while a phone is asking. Start / stop / restart only when a Docker widget on an
//! enabled page has "Allow start / stop" on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::time::Instant;

use crate::config::store::ConfigStore;
use crate::dotnet_text::{
    format_custom, lower_invariant, parse_date_ms, upper_invariant, utf16_prefix,
};
use crate::json_text::{Node, parse};
use crate::server::Stop;
use crate::tool_process::{self, RunError, ToolCommand};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub health: Option<String>,
    pub cpu: Option<f64>,
    pub mem: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerSnapshot {
    pub available: bool,
    pub error: Option<String>,
    pub containers: Vec<DockerContainer>,
    pub updated: i64,
}

/// CPU percent and memory text by short container id (case-insensitively).
pub type Stats = HashMap<String, (f64, String)>;

/// Runs the CLI with these arguments: (exit code, output, error output), or why it could not (C#'s exception message).
pub type Runner = Arc<
    dyn Fn(PathBuf, Vec<String>) -> BoxFuture<'static, Result<(i32, String, String), String>>
        + Send
        + Sync,
>;

pub struct DockerMonitor {
    cfg: Arc<ConfigStore>,
    snap: Mutex<DockerSnapshot>,
    /// When a phone last asked.
    last_request: Mutex<Option<Instant>>,
    stats: Mutex<Stats>,
    stats_at: Mutex<Option<Instant>>,
    gate: tokio::sync::Semaphore,
    clis: OnceLock<Vec<PathBuf>>,
    active_cli: Mutex<Option<PathBuf>>,
    runner: Option<Runner>,
}

impl DockerMonitor {
    pub fn new(cfg: Arc<ConfigStore>) -> Self {
        DockerMonitor {
            cfg,
            snap: Mutex::new(DockerSnapshot {
                available: false,
                error: None,
                containers: vec![],
                updated: 0,
            }),
            last_request: Mutex::new(None),
            stats: Mutex::new(Stats::new()),
            stats_at: Mutex::new(None),
            gate: tokio::sync::Semaphore::new(1),
            clis: OnceLock::new(),
            active_cli: Mutex::new(None),
            runner: None,
        }
    }

    /// Tests: this CLI instead of the one found.
    pub fn with_cli(self, cli: impl Into<PathBuf>) -> Self {
        *self.active_cli.lock().unwrap() = Some(cli.into());
        self
    }

    /// Tests: a fake CLI run.
    pub fn with_runner(mut self, runner: Runner) -> Self {
        self.runner = Some(runner);
        self
    }

    pub fn snapshot(&self) -> DockerSnapshot {
        self.stamp();
        self.current()
    }

    fn stamp(&self) {
        *lock(&self.last_request) = Some(Instant::now());
    }

    fn current(&self) -> DockerSnapshot {
        lock(&self.snap).clone()
    }

    fn set(&self, snap: DockerSnapshot) {
        *lock(&self.snap) = snap;
    }

    fn set_error(&self, error: String) {
        lock(&self.snap).error = Some(error);
    }

    fn clis(&self) -> &[PathBuf] {
        self.clis.get_or_init(find_clis)
    }

    fn cli(&self) -> Option<PathBuf> {
        lock(&self.active_cli)
            .clone()
            .or_else(|| self.clis().first().cloned())
    }

    /// The snapshot; a cold one first waits up to 2 s for a listing (which carries on in the background).
    pub async fn snapshot_cold(self: &Arc<Self>) -> DockerSnapshot {
        self.stamp();
        if self.current().updated == 0 {
            let me = self.clone();
            let refresh = tokio::spawn(async move { me.refresh_cold_cache(true).await });
            let _ = tokio::time::timeout(Duration::from_secs(2), refresh).await;
        }
        self.current()
    }

    /// A listing when the cache is still cold, if the gate frees up within 2 s.
    async fn refresh_cold_cache(&self, with_stats: bool) {
        let Ok(Ok(_permit)) =
            tokio::time::timeout(Duration::from_secs(2), self.gate.acquire()).await
        else {
            return;
        };
        if self.current().updated == 0
            && let Err(e) = self.refresh_core(with_stats).await
        {
            self.set_error(e);
        }
    }

    async fn refresh_core(&self, with_stats: bool) -> Result<(), String> {
        let clis = self.clis().to_vec();
        let Some(mut cli) = self.cli() else {
            self.set(DockerSnapshot {
                available: false,
                error: Some("Docker is not installed (no docker or podman command found)".into()),
                containers: vec![],
                updated: now_ms(),
            });
            return Ok(());
        };
        let mut fmt = format_arg(&cli);
        let (mut code, mut output, mut error) =
            self.exec(&cli, &["ps", "-a", "--format", fmt]).await?;
        if code != 0 && clis.len() > 1 {
            for alt in clis.iter().filter(|c| **c != cli) {
                let alt_fmt = format_arg(alt);
                let res = self.exec(alt, &["ps", "-a", "--format", alt_fmt]).await?;
                if res.0 == 0 {
                    cli = alt.clone();
                    *lock(&self.active_cli) = Some(alt.clone());
                    fmt = alt_fmt;
                    (code, output, error) = res;
                    break;
                }
            }
        } else if code == 0 {
            *lock(&self.active_cli) = Some(cli.clone());
        }
        if code != 0 {
            let containers = self.current().containers;
            let error = first_line(&error).unwrap_or("docker ps failed").to_string();
            self.set(DockerSnapshot {
                available: true,
                error: Some(error),
                containers,
                updated: now_ms(),
            });
            return Ok(());
        }
        let stats = lock(&self.stats).clone();
        self.set(DockerSnapshot {
            available: true,
            error: None,
            containers: parse_containers(&output, Some(&stats)),
            updated: now_ms(),
        });

        let due = lock(&self.stats_at).is_none_or(|at| at.elapsed() > Duration::from_secs(15));
        if with_stats && due {
            // stamped before the call, so a stats timeout does not make every later poll wait for it again
            *lock(&self.stats_at) = Some(Instant::now());
            if let Ok((0, out, _)) = self
                .exec(&cli, &["stats", "--no-stream", "--format", fmt])
                .await
            {
                let stats = parse_stats(&out);
                *lock(&self.stats) = stats.clone();
                self.set(DockerSnapshot {
                    available: true,
                    error: None,
                    containers: parse_containers(&output, Some(&stats)),
                    updated: now_ms(),
                });
            }
        }
        Ok(())
    }

    async fn exec(&self, cli: &Path, args: &[&str]) -> Result<(i32, String, String), String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        match &self.runner {
            Some(run) => run(cli.to_path_buf(), args).await,
            None => run(cli, args).await,
        }
    }

    /// Some Docker widget on an enabled page (Smart Stack children included) allows start / stop / restart.
    pub fn control_allowed(&self) -> bool {
        self.cfg
            .get()
            .widgets(true)
            .iter()
            .any(|w| w.r#type == "docker" && w.bool("control") == Some(true))
    }

    /// start, stop or restart a listed container: None, or why not.
    pub async fn act(&self, id: &str, action: &str) -> Option<String> {
        const OFF: &str = "starting and stopping containers is turned off in the widget's settings";
        if !self.control_allowed() {
            return Some(OFF.into());
        }
        if !matches!(action, "start" | "stop" | "restart") {
            return Some("unknown action".into());
        }
        // a cold action needs the container list only; metrics come with the next poll, so a slow stats call can't
        // hold the action back
        if self.current().updated == 0 {
            self.refresh_cold_cache(false).await;
        }
        if !self.current().containers.iter().any(|c| c.id == id) {
            return Some("unknown container".into());
        }
        let Some(cli) = self.cli() else {
            return Some("no docker or podman command found".into());
        };
        // the cold refresh awaited docker: the widget may have been hidden or its control turned off meanwhile
        if !self.control_allowed() {
            return Some(OFF.into());
        }
        let (code, error) = match self.exec(&cli, &[action, id]).await {
            Ok((code, _, error)) => (code, error),
            Err(e) => (-1, e),
        };
        let result = if code == 0 { "ok" } else { error.as_str() };
        crate::log::info(CATEGORY, &format!("docker {action} {id}: {result}"));
        *lock(&self.stats_at) = None;
        self.stamp();
        // wait for a poll in flight instead of skipping, so the phone sees the new state right after the action; the
        // bound is longer than a run's 20 s timeout, so a stuck poll can't hang the request
        if let Ok(Ok(_permit)) =
            tokio::time::timeout(Duration::from_secs(25), self.gate.acquire()).await
        {
            let _ = self.refresh_core(true).await;
        }
        if code == 0 {
            None
        } else {
            Some(first_line(&error).map_or_else(|| format!("docker {action} failed"), String::from))
        }
    }

    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let asked =
                    lock(&me.last_request).is_some_and(|at| at.elapsed() < Duration::from_secs(30));
                let pause = if asked {
                    if let Ok(_permit) = me.gate.try_acquire() {
                        let result = tokio::select! {
                            _ = stop.stopped() => return,
                            result = me.refresh_core(true) => result,
                        };
                        if let Err(e) = result {
                            me.set_error(e);
                        }
                    }
                    Duration::from_secs(4)
                } else {
                    Duration::from_millis(500)
                };
                tokio::select! {
                    _ = stop.stopped() => return,
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        })
    }
}

const CATEGORY: &str = "PhoneBox.Agent.DockerMonitor";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    crate::server::now_ms()
}

/// `json` for podman, `{{json .}}` for docker.
fn format_arg(cli: &Path) -> &'static str {
    let stem = cli
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem == "podman" {
        "json"
    } else {
        "{{json .}}"
    }
}

/// The first non-empty line, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.split('\n').find(|l| !l.is_empty()).map(str::trim)
}

/// `docker`, then `podman`: the first folder of PATH (and the usual install folders) that has each.
fn find_clis() -> Vec<PathBuf> {
    let (exe, sep) = if cfg!(windows) {
        (".exe", ';')
    } else {
        ("", ':')
    };
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<String> = path
        .split(sep)
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    if cfg!(windows) {
        dirs.push(r"C:\Program Files\Docker\Docker\resources\bin".into());
    } else {
        dirs.extend(
            [
                "/usr/local/bin",
                "/opt/homebrew/bin",
                "/usr/bin",
                "/Applications/Docker.app/Contents/Resources/bin",
            ]
            .map(String::from),
        );
    }
    let mut found = Vec::new();
    for name in ["docker", "podman"] {
        if let Some(file) = dirs
            .iter()
            .map(|d| Path::new(d.trim()).join(format!("{name}{exe}")))
            .find(|f| f.is_file())
        {
            found.push(file);
        }
    }
    found
}

/// The CLI with Homebrew's folders added to PATH where missing (a GUI-started agent often lacks them, and podman's
/// helpers need them); 20 s.
async fn run(cli: &Path, args: Vec<String>) -> Result<(i32, String, String), String> {
    let mut cmd = ToolCommand::new(cli).args(args);
    if !cfg!(windows) {
        let mut path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let parts: Vec<String> = path
            .split(':')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
            if !parts.iter().any(|p| p == dir) {
                path.push(':');
                path.push_str(dir);
            }
        }
        cmd = cmd.env("PATH", path);
    }
    match tool_process::run(&cmd, Duration::from_secs(20), std::future::pending()).await {
        Ok(out) => Ok((out.code, out.stdout, out.stderr)),
        Err(RunError::Start(e)) => Err(e),
        Err(_) => Err("The operation was canceled.".into()),
    }
}

/// `stats --no-stream` output (a JSON array, or one object per line).
pub fn parse_stats(raw: &str) -> Stats {
    let mut stats = Stats::new();
    for j in enumerate_json(raw) {
        let id = j
            .text("ID")
            .or_else(|| j.text("id"))
            .or_else(|| j.text("ContainerID"))
            .or_else(|| j.text("Container"));
        let id = id.unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let cpu = match j
            .text("CPUPerc")
            .or_else(|| j.text("cpu_percent"))
            .or_else(|| j.text("avg_cpu"))
        {
            Some(text) => text
                .trim_end_matches('%')
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|c| c.is_finite())
                .unwrap_or(0.0),
            None => j.num("CPU").or_else(|| j.num("AvgCPU")).unwrap_or(0.0),
        };
        let mem = match j.text("MemUsage").or_else(|| j.text("mem_usage")) {
            Some(text) => {
                let part = text.split('/').next().unwrap_or("").trim();
                if part == "--" || part == "0B" {
                    String::new()
                } else {
                    part.to_string()
                }
            }
            None => j
                .num("MemUsage")
                .filter(|b| *b > 0.0)
                .map(format_bytes)
                .unwrap_or_default(),
        };
        stats.insert(lower_invariant(short_id(id)), (cpu, mem));
    }
    stats
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1e9 {
        format!("{}GB", format_custom(bytes / 1e9, 2, false))
    } else if bytes >= 1e6 {
        format!("{}MB", format_custom(bytes / 1e6, 1, false))
    } else if bytes >= 1e3 {
        format!("{}kB", format_custom(bytes / 1e3, 0, false))
    } else {
        format!("{}B", format_custom(bytes, 0, false))
    }
}

/// The first 12 characters (UTF-16 units) of an id.
fn short_id(id: &str) -> &str {
    utf16_prefix(id, 12)
}

/// A JSON array of objects, or one object per line.
fn enumerate_json(raw: &str) -> Vec<Node> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.starts_with('[')
        && let Ok(doc) = parse(trimmed, false)
    {
        return match doc {
            Node::Array(items) => items.into_iter().filter(Node::is_object).collect(),
            _ => Vec::new(),
        };
    }
    trimmed
        .split('\n')
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| parse(line, false).ok())
        .filter(Node::is_object)
        .collect()
}

/// `ps -a` output: infrastructure containers left out, running ones first, then by name.
pub fn parse_containers(raw: &str, stats: Option<&Stats>) -> Vec<DockerContainer> {
    let mut list = Vec::new();
    for j in enumerate_json(raw) {
        if j.get("IsInfra") == Some(&Node::Bool(true)) {
            continue;
        }
        let id = j
            .text("ID")
            .or_else(|| j.text("Id"))
            .or_else(|| j.text("id"))
            .unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let short = short_id(id).to_string();
        let name = first_text(&j, "Names")
            .or_else(|| j.text("Name"))
            .unwrap_or(&short)
            .trim_start_matches('/')
            .to_string();
        let image = clean_image(j.text("Image").unwrap_or("")).to_string();
        let state = lower_invariant(j.text("State").unwrap_or(""));
        let mut status = j.text("Status").unwrap_or("").to_string();
        if status.is_empty() {
            status = fallback_status(&j, &state);
        }
        let health = j
            .text("HealthStatus")
            .or_else(|| j.text("Health"))
            .or_else(|| j.text("Healthcheck"))
            .filter(|h| !h.is_empty() && !h.eq_ignore_ascii_case("none"))
            .map(String::from);
        let stat = stats.and_then(|s| s.get(&lower_invariant(&short)));
        list.push(DockerContainer {
            project: extract_project(&j),
            cpu: stat.map(|s| s.0),
            mem: stat.map(|s| s.1.clone()),
            id: short,
            name,
            image,
            state,
            status,
            health,
        });
    }
    list.sort_by(|a, b| {
        (a.state != "running")
            .cmp(&(b.state != "running"))
            .then_with(|| {
                upper_invariant(&a.name)
                    .encode_utf16()
                    .cmp(upper_invariant(&b.name).encode_utf16())
            })
    });
    list
}

/// `Names` as a string, or its first non-empty string entry.
fn first_text<'a>(j: &'a Node, key: &str) -> Option<&'a str> {
    match j.get(key)? {
        Node::String(s) => Some(s),
        Node::Array(items) => items
            .iter()
            .filter_map(Node::as_str)
            .find(|s| !s.is_empty()),
        _ => None,
    }
}

fn clean_image(image: &str) -> &str {
    image
        .strip_prefix("docker.io/library/")
        .or_else(|| image.strip_prefix("docker.io/"))
        .unwrap_or(image)
}

/// The compose (or podman-compose) project from the labels, else the pod's name.
fn extract_project(j: &Node) -> Option<String> {
    match j.get("Labels") {
        Some(Node::String(labels)) => {
            for label in labels.split(',').map(str::trim).filter(|l| !l.is_empty()) {
                if (label.starts_with("com.docker.compose.project=")
                    || label.starts_with("io.podman.compose.project="))
                    && let Some((_, value)) = label.split_once('=')
                    && !value.trim().is_empty()
                {
                    return Some(value.trim().to_string());
                }
            }
        }
        Some(labels @ Node::Object(_)) => {
            let project = labels
                .text("com.docker.compose.project")
                .or_else(|| labels.text("io.podman.compose.project"));
            if let Some(project) = project.filter(|p| !p.trim().is_empty()) {
                return Some(project.to_string());
            }
        }
        _ => {}
    }
    j.text("PodName")
        .filter(|p| !p.is_empty())
        .map(String::from)
}

/// "Up 3 hours" from `StartedAt` for a running container without a status, else `CreatedAt`, else the state.
fn fallback_status(j: &Node, state: &str) -> String {
    if state == "running"
        && let Some(started) = started_unix_seconds(j)
        && started > 0
    {
        let sec = (chrono::Utc::now().timestamp() - started).max(0);
        return match sec {
            s if s >= 86400 => format!("Up {} days", s / 86400),
            s if s >= 3600 => format!("Up {} hours", s / 3600),
            s if s >= 60 => format!("Up {} minutes", s / 60),
            s => format!("Up {s} seconds"),
        };
    }
    j.text("CreatedAt")
        .filter(|c| !c.is_empty())
        .unwrap_or(state)
        .to_string()
}

/// `StartedAt` as Unix seconds: a positive number, or a date after 1970.
fn started_unix_seconds(j: &Node) -> Option<i64> {
    if let Some(n) = j.num("StartedAt").filter(|n| *n > 0.0) {
        return Some(n as i64);
    }
    let ms = parse_date_ms(j.text("StartedAt").filter(|s| !s.is_empty())?, false)?;
    // after 1970 (31 536 000 s into it): Docker's zero time is year 1
    (ms >= 31_536_000_000).then(|| ms.div_euclid(1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parse_podman_json_array_and_stats() {
        let ps = r#"[
          {"Id":"704cad0fb81e2195b08264115dc5b290057662668208a9401ad48283ea7f4c54","Image":"","IsInfra":true,
           "Names":["9e7869cac400-infra"],"PodName":"gh-mailpit","State":"running","Status":"Up 3 hours"},
          {"Id":"b741041b580bbe12bd52137c4a3ed48d8204cf0af46468f3cdecfee58d2263fb","Image":"localhost/atlas:dev","IsInfra":false,
           "Labels":{"org.opencontainers.image.title":"atlas"},"Names":["gh-dev-app"],"PodName":"gh-dev","State":"running","Status":"Up 3 hours"},
          {"Id":"0e7cc9c523ea09538781677443b9d66bb080c84748f59d796b4d1b4b652e3824","Image":"docker.io/library/postgres:17","IsInfra":false,
           "Labels":null,"Names":["fh-postgres"],"PodName":"","State":"exited","Status":"Exited (0) 2 days ago"}
        ]"#;
        let stats_json = r#"[{"id":"b741041b580b","name":"gh-dev-app","cpu_percent":"1.12%","avg_cpu":"1.12%","mem_usage":"107.6MB / 8.29GB"}]"#;
        let stats = parse_stats(stats_json);
        let containers = parse_containers(ps, Some(&stats));
        assert_eq!(containers.len(), 2);

        let running = &containers[0];
        assert_eq!(
            (running.id.as_str(), running.name.as_str()),
            ("b741041b580b", "gh-dev-app")
        );
        assert_eq!(
            (running.image.as_str(), running.state.as_str()),
            ("localhost/atlas:dev", "running")
        );
        assert_eq!(running.status, "Up 3 hours");
        assert_eq!(running.project.as_deref(), Some("gh-dev"));
        assert_eq!(running.cpu, Some(1.12));
        assert_eq!(running.mem.as_deref(), Some("107.6MB"));

        let exited = &containers[1];
        assert_eq!(
            (exited.id.as_str(), exited.name.as_str()),
            ("0e7cc9c523ea", "fh-postgres")
        );
        assert_eq!(
            (exited.image.as_str(), exited.state.as_str()),
            ("postgres:17", "exited")
        );
        assert_eq!(exited.status, "Exited (0) 2 days ago");
        assert_eq!((exited.project.as_deref(), exited.cpu), (None, None));
    }

    #[test]
    fn parse_podman_template_ndjson_fallback() {
        let started = chrono::Utc::now().timestamp() - 7200;
        let ps = format!(
            r#"{{"Id":"fb0f00d224c5e64456d795385d1449b66c8e58184543f1312635199cc934cac8","Image":"docker.io/axllent/mailpit@sha256:98b9","IsInfra":false,"Labels":{{"io.podman.compose.project":"mail-stack"}},"Names":["gh-mailpit-smtp"],"PodName":"gh-mailpit","StartedAt":{started},"State":"running","Status":""}}"#
        );
        let stats = parse_stats(
            r#"{"ContainerID":"fb0f00d224c5e64456d795385d1449b66c8e58184543f1312635199cc934cac8","Name":"gh-mailpit-smtp","CPU":0.45,"MemUsage":6549504}"#,
        );
        let containers = parse_containers(&ps, Some(&stats));
        assert_eq!(containers.len(), 1);
        let c = &containers[0];
        assert_eq!(
            (c.id.as_str(), c.name.as_str()),
            ("fb0f00d224c5", "gh-mailpit-smtp")
        );
        assert!(c.image.starts_with("axllent/mailpit"));
        assert_eq!(c.project.as_deref(), Some("mail-stack"));
        assert!(c.status.starts_with("Up 2 hours"), "{}", c.status);
        assert_eq!(c.cpu, Some(0.45));
        assert_eq!(c.mem.as_deref(), Some("6.5MB"));
    }

    #[test]
    fn parse_docker_ndjson_format() {
        let ps = r#"{"ID":"a1b2c3d4e5f6","Image":"redis:7-alpine","Labels":"com.docker.compose.project=webapp,com.docker.compose.service=cache","Names":"webapp-cache-1","State":"running","Status":"Up 45 minutes","HealthStatus":"healthy"}"#;
        let stats = parse_stats(
            r#"{"ID":"a1b2c3d4e5f6","CPUPerc":"2.50%","MemUsage":"24.5MiB / 7.77GiB"}"#,
        );
        let containers = parse_containers(ps, Some(&stats));
        assert_eq!(containers.len(), 1);
        let c = &containers[0];
        assert_eq!(
            (c.id.as_str(), c.name.as_str(), c.image.as_str()),
            ("a1b2c3d4e5f6", "webapp-cache-1", "redis:7-alpine")
        );
        assert_eq!(
            (c.project.as_deref(), c.health.as_deref()),
            (Some("webapp"), Some("healthy"))
        );
        assert_eq!((c.cpu, c.mem.as_deref()), (Some(2.5), Some("24.5MiB")));
    }

    const CONTAINER: &str = "a1b2c3d4e5f6";
    const PS: &str = "{\"ID\":\"a1b2c3d4e5f6\",\"Image\":\"redis:7\",\"Names\":\"cache-1\",\"State\":\"running\",\"Status\":\"Up 5 minutes\"}\n";

    fn store(dir: &std::path::Path, json: &str) -> Arc<ConfigStore> {
        let path = dir.join("config.json");
        std::fs::write(&path, json).unwrap();
        Arc::new(ConfigStore::new(path))
    }

    fn runner(
        f: impl Fn(Vec<String>) -> BoxFuture<'static, Result<(i32, String, String), String>>
        + Send
        + Sync
        + 'static,
    ) -> Runner {
        Arc::new(move |_cli, args| f(args))
    }

    #[tokio::test]
    async fn cold_cache_act_warms_snapshot_spaced_labels_extract_project_and_iso_started_at_formats_status()
     {
        let dir = tempfile::tempdir().unwrap();
        let cfg = store(
            dir.path(),
            r#"{"pages":[{"id":"p1","title":"Main","enabled":true,"widgets":[{"id":"d1","type":"docker","x":0,"y":0,"w":6,"h":4,"options":{"control":true}}]}]}"#,
        );
        let restarted = Arc::new(AtomicUsize::new(0));
        let r = restarted.clone();
        let mon = DockerMonitor::new(cfg)
            .with_cli("docker")
            .with_runner(runner(move |args| {
                let r = r.clone();
                Box::pin(async move {
                    match args.first().map(String::as_str) {
                        Some("ps") => Ok((0, PS.to_string(), String::new())),
                        Some("stats") => Err("simulated stats timeout".to_string()),
                        Some("restart") if args.get(1).map(String::as_str) == Some(CONTAINER) => {
                            r.fetch_add(1, Ordering::SeqCst);
                            Ok((0, String::new(), String::new()))
                        }
                        _ => Ok((1, String::new(), "unexpected".to_string())),
                    }
                })
            }));
        assert_eq!(mon.act(CONTAINER, "restart").await, None);
        assert_eq!(restarted.load(Ordering::SeqCst), 1);
        assert_eq!(mon.snapshot().error, None);
        assert_eq!(
            mon.act("missing00000", "stop").await.as_deref(),
            Some("unknown container")
        );
        assert_eq!(
            mon.act(CONTAINER, "kill").await.as_deref(),
            Some("unknown action")
        );

        // half an hour from the boundary of "Up 3 hours"
        let iso_started = (chrono::Utc::now() - chrono::Duration::minutes(210)).to_rfc3339();
        let parsed = parse_containers(
            &format!(
                r#"{{"ID":"b2c3d4e5f6a1","Image":"nginx:alpine","Labels":"role=edge, com.docker.compose.project= , io.podman.compose.project=edge-stack","Names":"edge-1","State":"running","Status":"","StartedAt":"{iso_started}","CreatedAt":"2026-01-01T00:00:00Z"}}"#
            ),
            None,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].project.as_deref(), Some("edge-stack"));
        assert!(
            parsed[0].status.starts_with("Up 3 hours"),
            "{}",
            parsed[0].status
        );
    }

    fn action_config(page_enabled: bool) -> String {
        serde_json::json!({"pages": [
            {"id": "home", "title": "Home", "enabled": true, "widgets": [{"id": "c1", "type": "clock", "x": 0, "y": 0, "w": 2, "h": 2}]},
            {"id": "p1", "title": "Main", "enabled": page_enabled, "widgets": [
                {"id": "d1", "type": "docker", "x": 0, "y": 0, "w": 6, "h": 4, "options": {"control": true}}]},
        ]})
        .to_string()
    }

    #[tokio::test]
    async fn hiding_the_widget_during_cold_discovery_stops_the_action() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = store(dir.path(), "{}");
        cfg.save(&action_config(true), None).unwrap();
        let ps_entered = Arc::new(tokio::sync::Notify::new());
        let release_ps = Arc::new(tokio::sync::Notify::new());
        let actions = Arc::new(AtomicUsize::new(0));
        let (entered, release, count) = (ps_entered.clone(), release_ps.clone(), actions.clone());
        let mon = Arc::new(
            DockerMonitor::new(cfg.clone())
                .with_cli("docker")
                .with_runner(runner(move |args| {
                    let (entered, release, count) =
                        (entered.clone(), release.clone(), count.clone());
                    Box::pin(async move {
                        match args.first().map(String::as_str) {
                            Some("ps") => {
                                entered.notify_one();
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(10),
                                    release.notified(),
                                )
                                .await;
                                Ok((0, PS.to_string(), String::new()))
                            }
                            Some("restart") => {
                                count.fetch_add(1, Ordering::SeqCst);
                                Ok((0, String::new(), String::new()))
                            }
                            _ => Ok((0, String::new(), String::new())),
                        }
                    })
                })),
        );
        let act = tokio::spawn({
            let mon = mon.clone();
            async move { mon.act(CONTAINER, "restart").await }
        });
        tokio::time::timeout(Duration::from_secs(5), ps_entered.notified())
            .await
            .unwrap();
        cfg.save(&action_config(false), None).unwrap();
        release_ps.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(30), act)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_some());
        assert_eq!(actions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_cold_action_does_not_wait_for_container_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = store(dir.path(), "{}");
        cfg.save(&action_config(true), None).unwrap();
        let release_stats = Arc::new(tokio::sync::Notify::new());
        let restarted = Arc::new(tokio::sync::Notify::new());
        let (release, done) = (release_stats.clone(), restarted.clone());
        let mon = Arc::new(
            DockerMonitor::new(cfg)
                .with_cli("docker")
                .with_runner(runner(move |args| {
                    let (release, done) = (release.clone(), done.clone());
                    Box::pin(async move {
                        match args.first().map(String::as_str) {
                            Some("ps") => Ok((0, PS.to_string(), String::new())),
                            Some("stats") => {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(10),
                                    release.notified(),
                                )
                                .await;
                                Ok((0, String::new(), String::new()))
                            }
                            Some("restart")
                                if args.get(1).map(String::as_str) == Some(CONTAINER) =>
                            {
                                done.notify_one();
                                Ok((0, String::new(), String::new()))
                            }
                            _ => Ok((0, String::new(), String::new())),
                        }
                    })
                })),
        );
        let act = tokio::spawn({
            let mon = mon.clone();
            async move { mon.act(CONTAINER, "restart").await }
        });
        // sent while stats is still stalled
        tokio::time::timeout(Duration::from_secs(5), restarted.notified())
            .await
            .unwrap();
        release_stats.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), act)
                .await
                .unwrap()
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_missing_cli_and_a_failing_listing_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = store(dir.path(), "{}");
        let mon = Arc::new(
            DockerMonitor::new(cfg.clone())
                .with_cli("docker")
                .with_runner(runner(|_| {
                    Box::pin(async {
                        Ok((
                            1,
                            String::new(),
                            "Cannot connect to the Docker daemon\nmore".to_string(),
                        ))
                    })
                })),
        );
        let snap = mon.snapshot_cold().await;
        assert!(snap.available);
        assert_eq!(
            snap.error.as_deref(),
            Some("Cannot connect to the Docker daemon")
        );
        assert!(snap.updated > 0);
        assert_eq!(
            mon.act(CONTAINER, "stop").await.as_deref(),
            Some("starting and stopping containers is turned off in the widget's settings")
        );
    }
}
