//! Codex sessions: the latest events in recent rollout files under `~/.codex/sessions` (no prompt or tool argument
//! leaves the computer).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use super::{AgentSession, SessionMonitor};

/// The first record carries the base instructions and tool definitions, so it can be far longer than one read.
const MAX_CODEX_META_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexDetails {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub last_input_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub last_activity_at: Option<i64>,
    pub turn_started_at: Option<i64>,
    pub lifecycle: Option<String>,
    pub last_tool: Option<String>,
    pub read_offset: u64,
}

#[derive(Default)]
pub(super) struct CodexState {
    discovery_root: Option<PathBuf>,
    next_discovery: Option<Instant>,
    candidates: Vec<PathBuf>,
    cwd: HashMap<PathBuf, String>,
    details: HashMap<PathBuf, (Option<SystemTime>, u64, CodexDetails)>,
}

impl SessionMonitor {
    /// The Codex sessions whose rollout changed in the last 3 hours.
    pub fn read_codex(&self, root: &Path) -> Vec<AgentSession> {
        use std::collections::HashSet;
        if !root.is_dir() {
            return Vec::new();
        }
        let now = SystemTime::now();
        let cutoff = now - std::time::Duration::from_secs(3 * 3600);
        let mut st = self.codex.lock().unwrap_or_else(|e| e.into_inner());
        // a resumed session appends to its original folder, even months later
        if st.discovery_root.as_deref() != Some(root)
            || st.next_discovery.is_none_or(|n| Instant::now() >= n)
        {
            st.candidates = super::walk_files(root, &|path, meta| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                name.starts_with("rollout-")
                    && name.ends_with(".jsonl")
                    && meta.modified().is_ok_and(|m| m > cutoff)
            });
            st.discovery_root = Some(root.to_path_buf());
            st.next_discovery = Some(Instant::now() + std::time::Duration::from_secs(30));
        }
        let files: Vec<(PathBuf, std::fs::Metadata)> = st
            .candidates
            .iter()
            .filter_map(|p| {
                let meta = std::fs::metadata(p).ok()?;
                (meta.is_file() && meta.modified().is_ok_and(|m| m > cutoff))
                    .then(|| (p.clone(), meta))
            })
            .collect();
        let active: HashSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        st.details.retain(|p, _| active.contains(p));
        st.cwd.retain(|p, _| active.contains(p));

        let mut found = Vec::new();
        for (path, meta) in files {
            let modified = meta.modified().ok();
            let length = meta.len();
            let cached = st.details.get(&path).cloned();
            let unchanged = cached
                .as_ref()
                .is_some_and(|c| c.0 == modified && c.1 == length);
            let appended = cached.as_ref().is_some_and(|c| length > c.1);
            let mut cwd = st.cwd.get(&path).cloned().unwrap_or_default();
            if cwd.is_empty() || (!unchanged && !appended) {
                cwd = read_codex_cwd(&path);
                if cwd.is_empty() {
                    st.cwd.remove(&path);
                } else {
                    st.cwd.insert(path.clone(), cwd.clone());
                }
            }
            let details = match (&cached, unchanged, appended) {
                (Some(c), true, _) => c.2.clone(),
                (Some(c), _, true) => {
                    match read_codex_details(&path, Some(&c.2), c.2.read_offset) {
                        Ok(d) => d,
                        Err(_) => continue,
                    }
                }
                _ => match read_codex_details(&path, None, 0) {
                    Ok(d) => d,
                    Err(_) => continue,
                },
            };
            st.details
                .insert(path.clone(), (modified, length, details.clone()));
            if let Some(s) = codex_session(&path, &meta, &cwd, details, now) {
                found.push(s);
            }
        }
        found
    }
}

/// The project folder from a rollout's first record; "" while that record is still being written.
pub fn read_codex_cwd(path: &Path) -> String {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let newline = loop {
        if buf.len() >= MAX_CODEX_META_BYTES {
            return String::new();
        }
        let room = (MAX_CODEX_META_BYTES - buf.len()).min(chunk.len());
        let n = match file.read(&mut chunk[..room]) {
            Ok(0) | Err(_) => return String::new(), // no complete record yet: the next poll reads it again
            Ok(n) => n,
        };
        let from = buf.len();
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[from..].iter().position(|&b| b == b'\n') {
            break from + i;
        }
    };
    let mut record = &buf[..newline];
    if let Some(rest) = record.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        record = rest;
    }
    if record.is_empty() {
        return String::new();
    }
    serde_json::from_slice::<serde_json::Value>(record)
        .ok()
        .and_then(|v| v.get("payload")?.get("cwd")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn ms(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// One rollout as the phone sees it; None for a file name C# cannot cut an id from.
fn codex_session(
    path: &Path,
    meta: &std::fs::Metadata,
    cwd: &str,
    d: CodexDetails,
    now: SystemTime,
) -> Option<AgentSession> {
    let modified = meta.modified().ok();
    let age = modified
        .and_then(|m| now.duration_since(m).ok())
        .unwrap_or_default();
    let busy =
        d.lifecycle.as_deref() == Some("task_started") && age < std::time::Duration::from_secs(180);
    let project = crate::dotnet_text::file_name(cwd.trim_end_matches(['\\', '/'])).to_string();
    let name: Vec<char> = path.file_name()?.to_string_lossy().chars().collect();
    // the 8 characters before ".jsonl"; C# throws for names of 9 to 13 characters (and ids under 4)
    let id: String = if name.len() > 8 {
        if name.len() < 14 {
            return None;
        }
        name[name.len() - 14..name.len() - 6].iter().collect()
    } else {
        name.iter().collect()
    };
    let short: String = id.chars().take(4).collect();
    if short.chars().count() < 4 {
        return None;
    }
    let outcome = match d.lifecycle.as_deref() {
        Some("task_complete") => Some("completed".to_string()),
        Some("turn_aborted") => Some("aborted".to_string()),
        _ => None,
    };
    let file_name: String = name.iter().collect();
    Some(AgentSession {
        model: d.model,
        effort: d.effort,
        total_tokens: d.total_tokens,
        input_tokens: d.input_tokens,
        output_tokens: d.output_tokens,
        cached_input_tokens: d.cached_input_tokens,
        reasoning_tokens: d.reasoning_tokens,
        last_input_tokens: d.last_input_tokens,
        context_window: d.context_window,
        turn_started_at: d.turn_started_at,
        outcome,
        last_tool: d.last_tool,
        ..AgentSession::new(
            &format!("codex:{file_name}"),
            "codex",
            &format!("{}-{short}", project.to_lowercase()),
            &project,
            if busy { "busy" } else { "idle" },
            None,
            d.last_activity_at.unwrap_or_else(|| ms(modified)),
            ms(meta.created().ok().or(modified)),
        )
    })
}

/// Scans metadata and counters once, then follows appended lines (complete lines only, from `offset`).
pub fn read_codex_details(
    path: &Path,
    previous: Option<&CodexDetails>,
    offset: u64,
) -> std::io::Result<CodexDetails> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let start = if previous.is_some() { offset } else { 0 };
    file.seek(SeekFrom::Start(start))?;
    let mut d = previous.cloned().unwrap_or_default();
    let mut carry: Vec<u8> = Vec::new();
    let mut consumed = 0u64;
    let mut chunk = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&chunk[..n]);
        // only complete lines count: a record still being written is read again next time
        if let Some(last) = carry.iter().rposition(|&b| b == b'\n') {
            for line in carry[..last].split(|&b| b == b'\n') {
                apply_codex_line(&mut d, &String::from_utf8_lossy(line));
            }
            consumed += last as u64 + 1;
            carry.drain(..=last);
        }
    }
    d.read_offset = start + consumed;
    Ok(d)
}

fn apply_codex_line(d: &mut CodexDetails, l: &str) {
    use serde_json::Value;
    if ![
        "\"event_msg\"",
        "\"turn_context\"",
        "\"custom_tool_call\"",
        "\"function_call\"",
    ]
    .iter()
    .any(|k| l.contains(k))
    {
        return;
    }
    let Ok(root) = serde_json::from_str::<Value>(l) else {
        return;
    };
    let Some(payload) = root.get("payload") else {
        return;
    };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |v: Option<&Value>, k: &str| {
        v.filter(|v| v.is_object())
            .and_then(|v| v.get(k))
            .and_then(Value::as_i64)
    };
    let kind = s(&root, "type");
    let event = s(payload, "type");
    let ts = s(&root, "timestamp").and_then(|t| crate::dotnet_text::parse_date_ms(&t, false));
    match kind.as_deref() {
        Some("event_msg") => {
            if event.as_deref() != Some("token_count") {
                d.last_activity_at = ts;
            }
            if event.as_deref() == Some("token_count")
                && let Some(info) = payload.get("info").filter(|i| i.is_object())
            {
                let total = info.get("total_token_usage");
                let last = info.get("last_token_usage");
                d.total_tokens = n(total, "total_tokens").or(d.total_tokens);
                d.input_tokens = n(total, "input_tokens").or(d.input_tokens);
                d.output_tokens = n(total, "output_tokens").or(d.output_tokens);
                d.cached_input_tokens = n(total, "cached_input_tokens").or(d.cached_input_tokens);
                d.reasoning_tokens = n(total, "reasoning_output_tokens").or(d.reasoning_tokens);
                d.last_input_tokens = n(last, "input_tokens").or(d.last_input_tokens);
                d.context_window = n(Some(info), "model_context_window").or(d.context_window);
            }
            if let Some(e @ ("task_started" | "task_complete" | "turn_aborted")) = event.as_deref()
            {
                d.lifecycle = Some(e.to_string());
                if e == "task_started" {
                    d.turn_started_at = ts;
                }
            }
        }
        Some("turn_context") => {
            d.model = s(payload, "model").or(d.model.take());
            d.effort = s(payload, "effort").or(d.effort.take());
        }
        Some("response_item")
            if matches!(event.as_deref(), Some("function_call" | "custom_tool_call")) =>
        {
            d.last_tool = s(payload, "name").or(d.last_tool.take());
        }
        _ => {}
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::{Value, json};

    pub(crate) fn line(kind: &str, payload: Value) -> String {
        json!({"type": kind, "timestamp": chrono::Utc::now().to_rfc3339(), "payload": payload})
            .to_string()
            + "\n"
    }

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
    }

    fn rollout(root: &Path, days_ago: i64) -> PathBuf {
        let day = chrono::Local::now() - chrono::Duration::days(days_ago);
        let dir = root
            .join(day.format("%Y").to_string())
            .join(day.format("%m").to_string())
            .join(day.format("%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-10-01-12345678-abcd.jsonl");
        std::fs::write(&path, line("session_meta", json!({"cwd": "/work/project"}))).unwrap();
        path
    }

    fn monitor() -> std::sync::Arc<SessionMonitor> {
        SessionMonitor::new(None, PathBuf::from("/nonexistent"))
    }

    fn only(list: Vec<AgentSession>) -> AgentSession {
        assert_eq!(list.len(), 1, "{list:?}");
        list.into_iter().next().unwrap()
    }

    #[test]
    fn resumed_session_is_found_in_an_old_directory() {
        let root = tempfile::tempdir().unwrap();
        let file = rollout(root.path(), 40);
        append(
            &file,
            &(line(
                "turn_context",
                json!({"model": "test-model", "effort": "high"}),
            ) + &line("event_msg", json!({"type": "task_started"}))),
        );
        let s = only(monitor().read_codex(root.path()));
        assert_eq!(s.model.as_deref(), Some("test-model"));
        assert_eq!(s.project, "project");
        assert_eq!(s.status, "busy");
    }

    #[test]
    fn follows_appends_and_completion_without_sending_prompts_or_tool_arguments() {
        let root = tempfile::tempdir().unwrap();
        let file = rollout(root.path(), 0);
        let m = monitor();
        append(
            &file,
            &[
                line("turn_context", json!({"model": "test-model", "effort": "high"})),
                line("event_msg", json!({"type": "task_started"})),
                line("event_msg", json!({"type": "user_message", "message": "PRIVATE PROMPT"})),
                line("response_item", json!({"type": "function_call", "name": "read", "arguments": "PRIVATE ARGUMENT"})),
                line(
                    "event_msg",
                    json!({"type": "token_count", "info": {"total_token_usage": {"total_tokens": 123, "input_tokens": 100,
                           "output_tokens": 23}, "model_context_window": 1000}}),
                ),
            ]
            .concat(),
        );
        let s = only(m.read_codex(root.path()));
        assert_eq!(s.total_tokens, Some(123));
        assert_eq!(s.last_tool.as_deref(), Some("read"));
        assert!(!serde_json::to_string(&s).unwrap().contains("PRIVATE"));
        append(&file, &line("event_msg", json!({"type": "task_complete"})));
        let s = only(m.read_codex(root.path()));
        assert_eq!(s.outcome.as_deref(), Some("completed"));
        assert_eq!(s.status, "idle");
        assert_eq!(s.total_tokens, Some(123));
    }

    #[test]
    fn malformed_token_counters_do_not_hide_the_session() {
        let root = tempfile::tempdir().unwrap();
        let file = rollout(root.path(), 0);
        append(
            &file,
            &(line("event_msg", json!({"type": "task_started"}))
                + &line(
                    "event_msg",
                    json!({"type": "token_count", "info": {"total_token_usage": {"total_tokens": "unknown"}}}),
                )),
        );
        only(monitor().read_codex(root.path()));
    }

    #[test]
    fn completes_long_partial_records_on_the_next_read() {
        let root = tempfile::tempdir().unwrap();
        let file = rollout(root.path(), 0);
        let m = monitor();
        append(&file, &line("event_msg", json!({"type": "task_started"})));
        only(m.read_codex(root.path()));
        let long = line(
            "response_item",
            json!({"type": "function_call", "name": "read", "arguments": "x".repeat(90000)}),
        );
        append(&file, &long[..85000]);
        assert_eq!(only(m.read_codex(root.path())).last_tool, None);
        append(&file, &long[85000..]);
        assert_eq!(
            only(m.read_codex(root.path())).last_tool.as_deref(),
            Some("read")
        );
        std::fs::write(
            &file,
            line("session_meta", json!({"cwd": "/work/project"}))
                + &line("event_msg", json!({"type": "turn_aborted"})),
        )
        .unwrap();
        assert_eq!(
            only(m.read_codex(root.path())).outcome.as_deref(),
            Some("aborted")
        );
    }

    #[test]
    fn codex_empty_or_partial_initial_rollout_recovers_project_once_session_meta_flushes() {
        let root = tempfile::tempdir().unwrap();
        let day = chrono::Utc::now();
        let dir = root
            .path()
            .join(day.format("%Y").to_string())
            .join(day.format("%m").to_string())
            .join(day.format("%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rollout-2026-11-17-0000-12345678.jsonl");
        std::fs::write(&file, "").unwrap();
        let m = monitor();
        let _ = m.read_codex(root.path());
        append(
            &file,
            &(line("session_meta", json!({"cwd": "/work/recovered-app"}))
                + &line("event_msg", json!({"type": "task_started"}))),
        );
        let later = SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let s = only(m.read_codex(root.path()));
        assert_eq!(s.project, "recovered-app");
        assert_eq!(s.name, "recovered-app-1234");
    }

    #[test]
    fn codex_project_comes_from_a_long_or_bom_prefixed_first_record_and_waits_for_an_incomplete_one()
     {
        let dir = tempfile::tempdir().unwrap();
        let cwd = |content: &[u8]| {
            let file = dir.path().join(format!("rollout-{}.jsonl", uuid_like()));
            std::fs::write(&file, content).unwrap();
            read_codex_cwd(&file)
        };
        let meta =
            r#"{"type":"session_meta","payload":{"cwd":"/work/demo","instructions":"__PAD__"}}"#;
        let long_meta = meta.replace("__PAD__", &"x".repeat(70 * 1024));
        assert_eq!(
            cwd(format!("{long_meta}\n{{\"type\":\"event_msg\"}}\n").as_bytes()),
            "/work/demo"
        );
        let bom = [
            &[0xEF, 0xBB, 0xBF][..],
            format!("{}\n", meta.replace("__PAD__", "")).as_bytes(),
        ]
        .concat();
        assert_eq!(cwd(&bom), "/work/demo");
        // still being written: nothing yet, so the next poll reads it again
        assert_eq!(cwd(long_meta.as_bytes()), "");
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{:x}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn details_follow_the_file_in_chunks_and_keep_old_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rollout-x-12345678.jsonl");
        // more than one read chunk of other records before the interesting one
        let filler = line(
            "response_item",
            json!({"type": "message", "content": "y".repeat(5000)}),
        );
        let mut body = filler.repeat(400);
        body.push_str(&line("turn_context", json!({"model": "m1"})));
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_st"); // a partial last line
        std::fs::write(&file, &body).unwrap();
        let first = read_codex_details(&file, None, 0).unwrap();
        assert_eq!(first.model.as_deref(), Some("m1"));
        assert_eq!(first.read_offset as usize, body.rfind('\n').unwrap() + 1);
        append(&file, "arted\"}}\n");
        let next = read_codex_details(&file, Some(&first), first.read_offset).unwrap();
        assert_eq!(next.lifecycle.as_deref(), Some("task_started"));
        assert_eq!(next.model.as_deref(), Some("m1"));
    }
}
