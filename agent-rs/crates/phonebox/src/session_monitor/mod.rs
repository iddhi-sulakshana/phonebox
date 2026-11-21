//! Port of `SessionMonitor.cs`: the AI coding sessions running on this PC (Claude Code, Codex, Cursor), for the
//! phone's Sessions widget (`/api/work` and the stream's `work` event).

mod codex;
mod cursor;

pub use codex::{CodexDetails, read_codex_cwd, read_codex_details};
pub use cursor::{
    cursor_project_fallback, cursor_project_slug, default_workspace_storage, read_cursor_last_role,
};

use serde::Serialize;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde_json::Value;

use crate::claude_transcript::ClaudeTranscript;
use crate::config::store::ConfigStore;

/// One running session. `status` is busy | waiting | shell | idle; `title` is the chat's title (Claude Code only),
/// set only while Settings › Claude Code › "Show session titles on the phone" is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub id: String,
    pub tool: String,
    pub name: String,
    pub project: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub since: i64,
    pub started_at: i64,
    pub pid: Option<i32>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub last_input_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub turn_started_at: Option<i64>,
    pub outcome: Option<String>,
    pub last_tool: Option<String>,
    pub title: Option<String>,
}

impl AgentSession {
    /// The C# record's positional part; everything else starts out null.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        tool: &str,
        name: &str,
        project: &str,
        status: &str,
        waiting_for: Option<&str>,
        since: i64,
        started_at: i64,
    ) -> Self {
        AgentSession {
            id: id.into(),
            tool: tool.into(),
            name: name.into(),
            project: project.into(),
            status: status.into(),
            waiting_for: waiting_for.map(Into::into),
            since,
            started_at,
            ..Default::default()
        }
    }
}

/// Titles are written again and again as the chat goes on, so the end of the file is enough.
const TITLE_TAIL_BYTES: u64 = 512 * 1024;
pub const MAX_TITLE_CHARS: usize = 200;

struct TitleEntry {
    length: u64,
    written: Option<SystemTime>,
    custom: Option<String>,
    ai: Option<String>,
}

/// Watches running AI coding sessions (local and read-only): Claude Code's status files in `~/.claude/sessions`, the
/// latest events of recent Codex rollouts and, opted in, recent Cursor agent transcripts.
pub struct SessionMonitor {
    cfg: Option<Arc<ConfigStore>>,
    home: PathBuf,
    sessions: RwLock<Vec<AgentSession>>,
    titles: Mutex<HashMap<PathBuf, TitleEntry>>,
    transcripts: Arc<ClaudeTranscript>,
    codex: Mutex<codex::CodexState>,
    cursor: Mutex<cursor::CursorState>,
}

impl SessionMonitor {
    pub fn new(cfg: Option<Arc<ConfigStore>>, home: PathBuf) -> Arc<SessionMonitor> {
        Arc::new(SessionMonitor {
            cfg,
            home,
            sessions: RwLock::new(Vec::new()),
            titles: Mutex::new(HashMap::new()),
            transcripts: Arc::new(ClaudeTranscript::new()),
            codex: Mutex::new(codex::CodexState::default()),
            cursor: Mutex::new(cursor::CursorState::default()),
        })
    }

    /// The conversation cache the phone's session details read through (trimmed on every pass).
    pub fn transcripts(&self) -> Arc<ClaudeTranscript> {
        self.transcripts.clone()
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The sessions, for everything that sends them anywhere (the live stream, /api/work, the rules). This is the one
    /// place they leave the monitor, so the title setting is applied here: with it off no title comes out.
    pub fn snapshot(&self) -> Vec<AgentSession> {
        let all = self
            .sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.share_titles() || all.iter().all(|s| s.title.is_none()) {
            return all;
        }
        all.into_iter()
            .map(|s| AgentSession { title: None, ..s })
            .collect()
    }

    /// What the monitor has read (the loop; tests seed it).
    pub fn set_sessions(&self, sessions: Vec<AgentSession>) {
        *self.sessions.write().unwrap_or_else(|e| e.into_inner()) = sessions;
    }

    /// One pass: Claude Code, Codex and (when Settings › General › Cursor sessions is on) Cursor, then the
    /// conversation cache drops what can no longer be asked for. Blocking file reads: run it off the async workers.
    pub fn tick(&self) {
        let titles = self.share_titles();
        let mut list = self.read_claude(&self.home, &alive, titles);
        list.extend(self.read_codex(&self.home.join(".codex").join("sessions")));
        // Cursor sessions are off until Settings › General › Cursor sessions is on
        if self.cfg.as_ref().is_some_and(|c| c.get().cursor.sessions) {
            let storage = cursor::default_workspace_storage(&self.home);
            list.extend(self.read_cursor(&self.home.join(".cursor").join("projects"), &storage));
        }
        let claude: Vec<String> = list
            .iter()
            .filter(|s| s.tool == "claude")
            .map(|s| s.id.strip_prefix("claude:").unwrap_or(&s.id).to_string())
            .collect();
        let share = self
            .cfg
            .as_ref()
            .is_some_and(|c| c.get().claude.share_conversation);
        self.set_sessions(list);
        self.transcripts.trim(&claude, share, None);
    }

    fn share_titles(&self) -> bool {
        self.cfg
            .as_ref()
            .is_some_and(|c| c.get().claude.share_session_titles)
    }

    /// The Claude Code sessions in `home/.claude/sessions`, each file a running session's status.
    pub fn read_claude(
        &self,
        home: &Path,
        is_alive: &dyn Fn(i32) -> bool,
        titles: bool,
    ) -> Vec<AgentSession> {
        let Ok(entries) = std::fs::read_dir(home.join(".claude").join("sessions")) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "json") || !path.is_file() {
                continue;
            }
            // a file being written is retried on the next pass
            if let Some(s) = self.read_claude_file(&path, home, is_alive, titles) {
                found.push(s);
            }
        }
        found
    }

    fn read_claude_file(
        &self,
        path: &Path,
        home: &Path,
        is_alive: &dyn Fn(i32) -> bool,
        titles: bool,
    ) -> Option<AgentSession> {
        let r: Value = serde_json::from_str(&crate::app_paths::read_all_text(path).ok()?).ok()?;
        // C# skips the file on a pid that is not a whole number (TryGetInt32 throws or fails)
        let pid = r
            .get("pid")?
            .as_i64()
            .and_then(|p| i32::try_from(p).ok())
            .filter(|&p| p != 0)?;
        if !is_alive(pid) {
            return None;
        }
        // a session that was killed leaves its file behind; Windows may give its pid to another program later
        #[cfg(windows)]
        if let Some(start) = str_of(&r, "procStart").and_then(|s| s.trim().parse::<i64>().ok())
            && !same_start(pid, start)
        {
            return None;
        }
        let cwd = str_of(&r, "cwd").unwrap_or("");
        let folder = crate::dotnet_text::file_name(cwd.trim_end_matches(['\\', '/'])).to_string();
        let name = str_of(&r, "name").map_or_else(|| folder.clone(), str::to_string);
        if is_codex_bar_probe(cwd) || is_claude_mem_observer(cwd) {
            return None;
        }
        let title = if titles {
            self.claude_title(home, &r, cwd, &name, &folder)
        } else {
            None
        };
        Some(AgentSession {
            pid: Some(pid),
            title,
            ..AgentSession::new(
                &format!(
                    "claude:{}",
                    str_of(&r, "sessionId").map_or_else(|| pid.to_string(), str::to_string)
                ),
                "claude",
                &name,
                &folder,
                str_of(&r, "status").unwrap_or("idle"),
                str_of(&r, "waitingFor"),
                long_of(&r, "statusUpdatedAt")
                    .or_else(|| long_of(&r, "updatedAt"))
                    .unwrap_or(0),
                long_of(&r, "startedAt").unwrap_or(0),
            )
        })
    }

    /// The chat's title: what the user named it, else the title Claude Code wrote (a derived name in the status file
    /// is only a placeholder such as "develop-int-61"). None when there is none yet, or it is just the folder name.
    fn claude_title(
        &self,
        home: &Path,
        status: &Value,
        cwd: &str,
        name: &str,
        project: &str,
    ) -> Option<String> {
        let from_name =
            str_of(status, "nameSource") != Some("derived") && str_of(status, "name").is_some();
        let title = if from_name {
            clean_title(Some(name))
        } else {
            match str_of(status, "sessionId").filter(|id| !id.is_empty()) {
                Some(id) => clean_title(self.read_claude_title(home, cwd, id).as_deref()),
                None => None,
            }
        };
        title.filter(|t| t != project)
    }

    /// The title Claude Code shows for a chat (a name you gave it, else the one it wrote itself), from the transcript
    /// at `~/.claude/projects/<folder>/<sessionId>.jsonl`. Only the two title records are read. None while the chat
    /// has no title yet.
    pub fn read_claude_title(&self, home: &Path, cwd: &str, session_id: &str) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        // Claude Code names the folder after the working directory with every other character turned into "-"
        // (one per UTF-16 unit)
        let folder: String = cwd
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_string()
                } else {
                    "-".repeat(c.len_utf16())
                }
            })
            .collect();
        let path = home
            .join(".claude")
            .join("projects")
            .join(folder)
            .join(format!("{session_id}.jsonl"));
        let info = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
        let written = info.modified().ok();
        let mut titles = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        let cached = titles.get(&path);
        if let Some(c) = cached
            && c.length == info.len()
            && c.written == written
        {
            return c.custom.clone().or_else(|| c.ai.clone());
        }
        // a title that has scrolled out of the tail is carried over, but only while the file only grew; a shorter or
        // replaced file starts again, and a file read whole has nothing to carry over
        let carry = cached.filter(|c| info.len() >= c.length && info.len() > TITLE_TAIL_BYTES);
        let (mut custom, mut ai) = carry.map_or((None, None), |c| (c.custom.clone(), c.ai.clone()));
        let mut file = std::fs::File::open(&path).ok()?;
        let length = file.metadata().ok()?.len();
        let start = length.saturating_sub(TITLE_TAIL_BYTES);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        file.take(length - start).read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.split('\n').skip(usize::from(start > 0)) {
            let is_custom = line.contains("\"custom-title\"");
            if !is_custom && !line.contains("\"ai-title\"") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue; // a line still being written
            };
            match str_of(&rec, "type") {
                Some("custom-title") => {
                    custom = clean_title(str_of(&rec, "customTitle")).or(custom)
                }
                Some("ai-title") => ai = clean_title(str_of(&rec, "aiTitle")).or(ai),
                _ => {}
            }
        }
        titles.insert(
            path,
            TitleEntry {
                length,
                written,
                custom: custom.clone(),
                ai: ai.clone(),
            },
        );
        custom.or(ai)
    }
}

/// CodexBar runs `claude` in `<Application Support or temp>/CodexBar/ClaudeProbe` to read usage: only that exact folder.
pub fn is_codex_bar_probe(cwd: &str) -> bool {
    let normalized = cwd.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/').to_lowercase();
    trimmed.ends_with("/codexbar/claudeprobe")
}

/// claude-mem starts short background Claude Code sessions in `~/.claude-mem/observer-sessions/<n>` (#275).
pub fn is_claude_mem_observer(cwd: &str) -> bool {
    let normalized = cwd.replace('\\', "/");
    format!("{}/", normalized.trim_end_matches('/'))
        .to_lowercase()
        .contains("/.claude-mem/observer-sessions/")
}

/// A title as the phone gets it, whichever file it came from: trimmed and no longer than [`MAX_TITLE_CHARS`].
pub fn clean_title(title: Option<&str>) -> Option<String> {
    let t = title.filter(|t| !t.trim().is_empty())?.trim();
    Some(crate::dotnet_text::utf16_prefix(t, MAX_TITLE_CHARS).to_string())
}

/// A process with that pid is running (on Unix also one this user may not signal).
pub fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // a process of another user answers EPERM: it is there
        unsafe { libc::kill(pid, 0) == 0 || *libc_errno() == libc::EPERM }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let Ok(handle) =
            (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) })
        else {
            return false;
        };
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        let _ = unsafe { CloseHandle(handle) };
        ok && code == STILL_ACTIVE.0 as u32
    }
}

#[cfg(unix)]
unsafe fn libc_errno() -> *mut i32 {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::__errno_location() }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe { libc::__error() }
    }
}

/// Claude Code writes its process' start time (a FILETIME) into the session file; another process with the same pid
/// started later. Anything unreadable counts as the same process.
#[cfg(windows)]
fn same_start(pid: i32, file_time_utc: i64) -> bool {
    process_start_filetime(pid).is_none_or(|start| (start - file_time_utc).abs() < 10_000_000)
}

/// When a process started, as a FILETIME (what Claude Code writes as `procStart` on Windows).
#[cfg(windows)]
pub fn process_start_filetime(pid: i32) -> Option<i64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) }.ok()?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
        .is_ok();
    let _ = unsafe { CloseHandle(handle) };
    ok.then_some(((created.dwHighDateTime as i64) << 32) | created.dwLowDateTime as i64)
}

fn str_of<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e.get(key).and_then(Value::as_str)
}

fn long_of(e: &Value, key: &str) -> Option<i64> {
    e.get(key).and_then(Value::as_i64)
}

/// Every file under `root` (folders followed recursively, symbolic links not) that `accept` takes.
pub(crate) fn walk_files(
    root: &Path,
    accept: &dyn Fn(&Path, &std::fs::Metadata) -> bool,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
                && accept(&path, &meta)
            {
                found.push(path);
            }
        }
    }
    found
}

/// The "work" data the phone gets, on the live stream and on the polling path alike: sessions and GitHub.
pub fn work_feed(sessions: &SessionMonitor, github: Option<Value>) -> Value {
    serde_json::json!({ "sessions": sessions.snapshot(), "github": github })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TITLE: &str = "Quarterly invoice numbers for Acme";

    fn monitor() -> Arc<SessionMonitor> {
        SessionMonitor::new(None, PathBuf::from("/nonexistent"))
    }

    fn only(list: Vec<AgentSession>) -> AgentSession {
        assert_eq!(list.len(), 1, "{list:?}");
        list.into_iter().next().unwrap()
    }

    /// A session file like Claude Code writes it, with a transcript that has a generated title in it.
    fn chat(root: &Path, name: &str, name_source: &str, transcript: Option<&str>) {
        let sessions = root.join(".claude").join("sessions");
        let chats = root
            .join(".claude")
            .join("projects")
            .join("-Users-test-develop-int");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            sessions.join("7.json"),
            json!({"pid": 7, "sessionId": "sess-1", "cwd": "/Users/test/develop-int", "name": name,
                   "nameSource": name_source, "status": "idle"})
            .to_string(),
        )
        .unwrap();
        let default = format!(
            "{{\"type\":\"last-prompt\",\"lastPrompt\":\"PRIVATE PROMPT\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"{TITLE}\"}}\n"
        );
        std::fs::write(chats.join("sess-1.jsonl"), transcript.unwrap_or(&default)).unwrap();
    }

    fn seven(pid: i32) -> bool {
        pid == 7
    }

    #[test]
    fn titles_are_off_by_default() {
        assert!(!crate::config::ClaudeConfig::default().share_session_titles);
    }

    #[test]
    fn a_derived_name_is_replaced_by_the_chat_title_only_when_titles_are_on() {
        let root = tempfile::tempdir().unwrap();
        chat(root.path(), "develop-int-61", "derived", None);
        let m = monitor();
        let on = only(m.read_claude(root.path(), &seven, true));
        assert_eq!(on.title.as_deref(), Some(TITLE));
        assert_eq!(on.name, "develop-int-61");
        assert_eq!(on.project, "develop-int");
        assert_eq!(only(m.read_claude(root.path(), &seven, false)).title, None);
    }

    #[test]
    fn a_name_the_user_gave_beats_a_generated_title_and_the_newest_generated_title_wins() {
        let root = tempfile::tempdir().unwrap();
        let m = monitor();
        chat(root.path(), "My own name", "user", None);
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("My own name")
        );
        chat(
            root.path(),
            "develop-int-61",
            "derived",
            Some(
                "{\"type\":\"ai-title\",\"aiTitle\":\"Old\"}\n{\"type\":\"last-prompt\",\"lastPrompt\":\"PRIVATE PROMPT\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Newer\"}\n",
            ),
        );
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Newer")
        );
        chat(
            root.path(),
            "develop-int-61",
            "derived",
            Some(
                "{\"type\":\"ai-title\",\"aiTitle\":\"Generated\"}\n{\"type\":\"custom-title\",\"customTitle\":\"Renamed by me\"}\n",
            ),
        );
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Renamed by me")
        );
    }

    #[test]
    fn no_title_yet_and_a_long_title() {
        let root = tempfile::tempdir().unwrap();
        let m = monitor();
        chat(
            root.path(),
            "develop-int-61",
            "derived",
            Some("{\"type\":\"last-prompt\",\"lastPrompt\":\"PRIVATE PROMPT\"}\n"),
        );
        assert_eq!(only(m.read_claude(root.path(), &seven, true)).title, None);
        let long = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"{}\"}}\n",
            "x".repeat(500)
        );
        chat(root.path(), "develop-int-61", "derived", Some(&long));
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .unwrap()
                .len(),
            200
        );
    }

    fn transcript_file(root: &Path) -> PathBuf {
        root.join(".claude")
            .join("projects")
            .join("-Users-test-develop-int")
            .join("sess-1.jsonl")
    }

    #[test]
    fn a_rewritten_or_shorter_chat_file_does_not_keep_the_old_title() {
        let root = tempfile::tempdir().unwrap();
        let m = monitor();
        let text = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"Old name I gave it\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Old generated\"}}\n{}\n",
            " ".repeat(200)
        );
        chat(root.path(), "develop-int-61", "derived", Some(&text));
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Old name I gave it")
        );
        std::fs::write(
            transcript_file(root.path()),
            "{\"type\":\"ai-title\",\"aiTitle\":\"Brand new\"}\n",
        )
        .unwrap();
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Brand new")
        );
    }

    #[test]
    fn a_growing_large_file_keeps_a_title_that_scrolled_out_of_the_tail_but_not_after_it_shrinks() {
        let root = tempfile::tempdir().unwrap();
        let m = monitor();
        let line = format!(
            "{{\"type\":\"last-prompt\",\"lastPrompt\":\"{}\"}}",
            "x".repeat(900)
        );
        let filler = vec![line; 1000].join("\n") + "\n"; // about 1 MB
        chat(
            root.path(),
            "develop-int-61",
            "derived",
            Some(&format!(
                "{{\"type\":\"ai-title\",\"aiTitle\":\"Early title\"}}\n{filler}"
            )),
        );
        // the title is outside the last 512 KB, so the first read cannot see it
        assert_eq!(only(m.read_claude(root.path(), &seven, true)).title, None);

        let file = transcript_file(root.path());
        std::fs::write(
            &file,
            format!(
                "{{\"type\":\"ai-title\",\"aiTitle\":\"Seen at the start\"}}\n{}\n",
                " ".repeat(10)
            ),
        )
        .unwrap();
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Seen at the start")
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(&mut f, format!("{filler}{filler}").as_bytes()).unwrap();
        drop(f);
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("Seen at the start")
        );

        std::fs::write(
            &file,
            "{\"type\":\"last-prompt\",\"lastPrompt\":\"short\"}\n",
        )
        .unwrap();
        assert_eq!(only(m.read_claude(root.path(), &seven, true)).title, None);
    }

    #[test]
    fn a_long_name_in_the_status_file_is_cut_like_a_transcript_title() {
        let root = tempfile::tempdir().unwrap();
        let m = monitor();
        chat(root.path(), &"n".repeat(500), "user", None);
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .unwrap()
                .len(),
            MAX_TITLE_CHARS
        );
        chat(root.path(), "   padded name   ", "user", None);
        assert_eq!(
            only(m.read_claude(root.path(), &seven, true))
                .title
                .as_deref(),
            Some("padded name")
        );
    }

    fn session(title: Option<&str>) -> AgentSession {
        AgentSession {
            pid: Some(7),
            title: title.map(Into::into),
            ..AgentSession::new(
                "claude:sess-1",
                "claude",
                "develop-int-61",
                "develop-int",
                "idle",
                None,
                1,
                1,
            )
        }
    }

    fn config(dir: &Path, setting: Option<bool>) -> Arc<ConfigStore> {
        let path = dir.join(format!(
            "config-{}.json",
            setting.map_or("none".to_string(), |s| s.to_string())
        ));
        let text = match setting {
            None => "{}".to_string(),
            Some(on) => format!("{{\"claude\":{{\"shareSessionTitles\":{on}}}}}"),
        };
        std::fs::write(&path, text).unwrap();
        Arc::new(ConfigStore::new(path))
    }

    #[test]
    fn snapshot_is_the_one_place_the_setting_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        for (setting, comes_out) in [(None, false), (Some(false), false), (Some(true), true)] {
            let m = SessionMonitor::new(Some(config(dir.path(), setting)), dir.path().into());
            m.set_sessions(vec![session(Some(TITLE))]);
            let s = only(m.snapshot());
            assert_eq!(
                s.title.as_deref(),
                comes_out.then_some(TITLE),
                "{setting:?}"
            );
            assert_eq!(s.name, "develop-int-61");
        }
    }

    #[test]
    fn without_a_config_no_title_comes_out() {
        let m = monitor();
        m.set_sessions(vec![session(Some(TITLE))]);
        assert_eq!(only(m.snapshot()).title, None);
    }

    #[test]
    fn the_work_data_for_the_phone_carries_the_title_only_when_titles_are_on() {
        let dir = tempfile::tempdir().unwrap();
        for on in [false, true] {
            let m = SessionMonitor::new(Some(config(dir.path(), Some(on))), dir.path().into());
            m.set_sessions(vec![session(Some(TITLE))]);
            let json = crate::config::write::to_dotnet_json_compact(&work_feed(&m, None));
            assert_eq!(json.contains(TITLE), on);
            assert!(json.contains("develop-int"));
            assert!(json.contains(r#""github":null"#));
        }
    }

    #[test]
    fn claude_normal_session_is_reported() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1122.json"),
            json!({"pid": 1122, "sessionId": "sess-1", "cwd": "/Users/test/workspace/my-app", "status": "waiting",
                   "waitingFor": "permission prompt"})
            .to_string(),
        )
        .unwrap();
        let s = only(monitor().read_claude(root.path(), &|p| p == 1122, false));
        assert_eq!(s.tool, "claude");
        assert_eq!(s.project, "my-app");
        assert_eq!(s.status, "waiting");
        assert_eq!(s.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(s.pid, Some(1122));
        assert_eq!(s.id, "claude:sess-1");
        assert_eq!(
            serde_json::to_value(&s)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            [
                "id",
                "tool",
                "name",
                "project",
                "status",
                "waitingFor",
                "since",
                "startedAt",
                "pid",
                "model",
                "effort",
                "totalTokens",
                "inputTokens",
                "outputTokens",
                "cachedInputTokens",
                "reasoningTokens",
                "lastInputTokens",
                "contextWindow",
                "turnStartedAt",
                "outcome",
                "lastTool",
                "title"
            ]
        );
    }

    #[test]
    fn claude_claude_probe_and_codex_bar_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2233.json"),
            json!({"pid": 2233, "sessionId": "sess-probe",
                   "cwd": "/Users/test/Library/Application Support/CodexBar/ClaudeProbe", "status": "waiting",
                   "waitingFor": "dialog open"})
            .to_string(),
        )
        .unwrap();
        assert!(
            monitor()
                .read_claude(root.path(), &|p| p == 2233, false)
                .is_empty()
        );
    }

    #[test]
    fn claude_only_the_background_tool_directories_are_ignored() {
        for (cwd, ignored) in [
            (
                "/Users/test/Library/Application Support/CodexBar/ClaudeProbe",
                true,
            ),
            ("/var/folders/xy/T/CodexBar/ClaudeProbe/", true),
            (
                r"C:\Users\test\AppData\Local\Temp\CodexBar\ClaudeProbe",
                true,
            ),
            ("/Users/test/Projects/CodexBar", false),
            ("/Users/test/Projects/CodexBar/Sources", false),
            ("/Users/test/Projects/ClaudeProbeKit", false),
            ("/Users/test/CodexBar/ClaudeProbe/subdir", false),
            ("/Users/test/.claude-mem/observer-sessions/108", true),
            (r"C:\Users\test\.claude-mem\observer-sessions\110\", true),
            ("/home/test/.claude-mem/observer-sessions", true),
            ("/home/test/.claude-mem", false),
            ("/home/test/Projects/claude-mem/observer-sessions", false),
            ("/home/test/Projects/claude-mem", false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join(".claude").join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("3344.json"),
                json!({"pid": 3344, "sessionId": "sess-x", "cwd": cwd, "status": "busy"})
                    .to_string(),
            )
            .unwrap();
            let found = monitor()
                .read_claude(root.path(), &|p| p == 3344, false)
                .len();
            assert_eq!(found, usize::from(!ignored), "{cwd}");
        }
    }

    #[test]
    fn a_session_file_that_is_not_a_live_session_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in [
            ("a.json", json!({"pid": "7", "sessionId": "s"}).to_string()), // not a number
            ("b.json", json!({"pid": 0, "sessionId": "s"}).to_string()),
            ("c.json", json!({"pid": 8, "sessionId": "s"}).to_string()), // not running
            ("d.json", "{ half-written".to_string()),
            ("e.txt", json!({"pid": 7, "sessionId": "s"}).to_string()),
        ] {
            std::fs::write(dir.join(file), text).unwrap();
        }
        assert!(monitor().read_claude(root.path(), &seven, false).is_empty());
        // the id falls back to the pid, the name to the folder; times come from the status file
        std::fs::write(
            dir.join("f.json"),
            json!({"pid": 7, "cwd": "/w/app/", "statusUpdatedAt": 5, "updatedAt": 4, "startedAt": 3}).to_string(),
        )
        .unwrap();
        let s = only(monitor().read_claude(root.path(), &seven, false));
        assert_eq!(
            (
                s.id.as_str(),
                s.name.as_str(),
                s.status.as_str(),
                s.since,
                s.started_at
            ),
            ("claude:7", "app", "idle", 5, 3)
        );
    }

    #[test]
    fn a_pass_reads_every_tool_and_trims_the_conversation_cache() {
        let home = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let sessions = home.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("a.json"),
            json!({"pid": pid, "sessionId": "abcdef12-3456", "cwd": "/w/app", "status": "busy"})
                .to_string(),
        )
        .unwrap();
        let codex = home.path().join(".codex").join("sessions").join("2026");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-2026-11-17-0000-12345678.jsonl"),
            codex::tests::line("session_meta", json!({"cwd": "/w/other"})),
        )
        .unwrap();
        // a cached chat whose session is not running goes on the pass
        let chats = home.path().join(".claude").join("projects").join("-w");
        std::fs::create_dir_all(&chats).unwrap();
        let ended = chats.join("bbbbbbbb-0001.jsonl");
        std::fs::write(
            &ended,
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        std::fs::write(&cfg, r#"{"claude":{"shareConversation":true}}"#).unwrap();
        let m = SessionMonitor::new(Some(Arc::new(ConfigStore::new(cfg))), home.path().into());
        m.transcripts().read(&ended);
        assert!(m.transcripts().is_cached(&ended));
        m.tick();
        let tools: Vec<String> = m.snapshot().iter().map(|s| s.id.clone()).collect();
        assert_eq!(
            tools,
            [
                "claude:abcdef12-3456",
                "codex:rollout-2026-11-17-0000-12345678.jsonl"
            ]
        );
        assert!(!m.transcripts().is_cached(&ended));
    }

    #[test]
    fn this_process_is_alive_and_a_free_pid_is_not() {
        assert!(alive(std::process::id() as i32));
        assert!(!alive(i32::MAX - 7));
    }

    #[cfg(windows)]
    #[test]
    fn claude_session_file_of_a_reused_pid_is_not_shown_as_a_live_session() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id() as i32;
        // the session's own process started a week before the one that has the pid now
        let week = 7 * 24 * 3600 * 10_000_000_i64;
        let started = process_start_filetime(pid).unwrap() - week;
        std::fs::write(
            dir.join(format!("{pid}.json")),
            json!({"pid": pid, "sessionId": "s1", "cwd": r"D:\work\demo", "procStart": started.to_string(),
                   "pidDomain": "win32:white", "status": "waiting", "waitingFor": "permission"})
            .to_string(),
        )
        .unwrap();
        assert!(monitor().read_claude(root.path(), &alive, false).is_empty());
    }
}
