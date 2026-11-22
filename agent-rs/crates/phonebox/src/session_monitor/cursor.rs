//! Cursor sessions (opt-in): recent agent transcripts under `~/.cursor/projects/*/agent-transcripts`. File times and
//! the last record's role give the status; message text is never returned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use super::{AgentSession, SessionMonitor};

const CURSOR_ROLE_INITIAL_BYTES: u64 = 8192;
const CURSOR_ROLE_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub(super) struct CursorState {
    discovery_root: Option<PathBuf>,
    next_discovery: Option<Instant>,
    candidates: Vec<PathBuf>,
    roles: HashMap<PathBuf, (Option<SystemTime>, u64, Option<String>)>,
    names: Option<HashMap<String, String>>,
    next_names: Option<Instant>,
}

impl SessionMonitor {
    /// The Cursor chats written in the last 3 hours; project names from Cursor's workspace storage.
    pub fn read_cursor(
        &self,
        projects_root: &Path,
        workspace_storage_root: &Path,
    ) -> Vec<AgentSession> {
        use std::collections::HashSet;
        if !projects_root.is_dir() {
            return Vec::new();
        }
        let now = SystemTime::now();
        let cutoff = now - std::time::Duration::from_secs(3 * 3600);
        let mut st = self.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if st.discovery_root.as_deref() != Some(projects_root)
            || st.next_discovery.is_none_or(|n| Instant::now() >= n)
        {
            st.candidates = super::walk_files(projects_root, &|path, meta| {
                let jsonl = path.extension().is_some_and(|e| e == "jsonl");
                let transcripts = path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|n| {
                        n.to_string_lossy()
                            .eq_ignore_ascii_case("agent-transcripts")
                    });
                let cleanup = path
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_lowercase()
                    .contains("/.agent-data-cleanup-");
                jsonl && transcripts && !cleanup && meta.modified().is_ok_and(|m| m > cutoff)
            });
            st.discovery_root = Some(projects_root.to_path_buf());
            st.next_discovery = Some(Instant::now() + std::time::Duration::from_secs(30));
        }
        let names = project_names(&mut st, workspace_storage_root);
        let mut active = HashSet::new();
        let mut found = Vec::new();
        for path in st.candidates.clone() {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let modified = meta.modified().ok();
            if !meta.is_file() || modified.is_none_or(|m| m <= cutoff) {
                continue;
            }
            active.insert(path.clone());
            let id = path
                .parent()
                .and_then(Path::file_name)
                .or_else(|| path.file_stem())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if id.trim().is_empty() {
                continue;
            }
            let slug = path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let project = names
                .get(&slug.to_lowercase())
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| cursor_project_fallback(&slug));
            let age = modified
                .and_then(|m| now.duration_since(m).ok())
                .unwrap_or_default();
            let role = match st.roles.get(&path) {
                Some((w, l, role)) if *w == modified && *l == meta.len() => role.clone(),
                _ => {
                    let role = read_cursor_last_role(&path);
                    st.roles
                        .insert(path.clone(), (modified, meta.len(), role.clone()));
                    role
                }
            };
            // a recent assistant message means the turn finished (idle); a recent user message means the agent is
            // still working on a reply (busy); "waiting" is Claude's (the user's attention)
            let busy =
                age < std::time::Duration::from_secs(180) && role.as_deref() != Some("assistant");
            let short: String = id.chars().take(4).collect();
            let ms = |t: Option<SystemTime>| {
                t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64)
            };
            found.push(AgentSession::new(
                &format!("cursor:{id}"),
                "cursor",
                &format!("{}-{short}", project.to_lowercase()),
                &project,
                if busy { "busy" } else { "idle" },
                None,
                ms(modified),
                ms(meta.created().ok().or(modified)),
            ));
        }
        st.roles.retain(|p, _| active.contains(p));
        found
    }

    /// True when the role cache holds this path at the given write time and length (tests).
    pub fn has_cached_cursor_role(&self, path: &Path, written: SystemTime, length: u64) -> bool {
        let st = self.cursor.lock().unwrap_or_else(|e| e.into_inner());
        st.roles
            .get(path)
            .is_some_and(|(w, l, _)| *w == Some(written) && *l == length)
    }
}

/// Where Cursor keeps its workspace storage: `%APPDATA%` on Windows, `~/.config` elsewhere, and on macOS
/// `~/Library/Application Support` when the first does not exist.
pub fn default_workspace_storage(home: &Path) -> PathBuf {
    let tail = ["Cursor", "User", "workspaceStorage"];
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map_or_else(|| home.join("AppData").join("Roaming"), PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let root = tail.iter().fold(base, |p, t| p.join(t));
    // on macOS Application Support is in Library, not the XDG folder .NET calls ApplicationData
    if cfg!(target_os = "macos") && !root.is_dir() {
        return tail
            .iter()
            .fold(home.join("Library").join("Application Support"), |p, t| {
                p.join(t)
            });
    }
    root
}

/// Maps Cursor project-folder slugs (lower case) to workspace folder names via workspaceStorage, read again every 60 s.
fn project_names(st: &mut CursorState, root: &Path) -> HashMap<String, String> {
    if let Some(names) = &st.names
        && st.next_names.is_some_and(|n| Instant::now() < n)
    {
        return names.clone();
    }
    let mut map = HashMap::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let file = dir.path().join("workspace.json");
        let Ok(text) = crate::app_paths::read_all_text(&file) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(folder) = doc
            .get("folder")
            .and_then(|f| f.as_str())
            .filter(|f| !f.is_empty())
        else {
            continue;
        };
        let Ok(url) = reqwest::Url::parse(folder) else {
            continue;
        };
        let local = if url.scheme() == "file" {
            url.to_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| folder.to_string())
        } else {
            folder.to_string()
        };
        if local.is_empty() {
            continue;
        }
        let normalized = local.replace('\\', "/");
        let trimmed = normalized.trim_end_matches('/');
        let slug = cursor_project_slug(trimmed);
        if slug.is_empty() {
            continue;
        }
        map.insert(
            slug.to_lowercase(),
            crate::dotnet_text::file_name(trimmed).to_string(),
        );
    }
    st.names = Some(map.clone());
    st.next_names = Some(Instant::now() + std::time::Duration::from_secs(60));
    map
}

/// The slug of a workspace folder path, as Cursor names its projects folder (no drive colon).
pub fn cursor_project_slug(local_path: &str) -> String {
    let normalized = local_path.replace('\\', "/");
    // Windows: "C:/Users/…" → "C-Users-…" so the key matches the projects folder name (':' is illegal in names)
    normalized
        .trim_end_matches('/')
        .trim_start_matches('/')
        .replace('/', "-")
        .replace(':', "")
}

pub fn cursor_project_fallback(project_slug: &str) -> String {
    if project_slug.trim().is_empty() || project_slug.chars().all(char::is_numeric) {
        return "Cursor".into();
    }
    project_slug
        .split('-')
        .rfind(|p| !p.is_empty())
        .unwrap_or("Cursor")
        .to_string()
}

/// The last record's `role` in a bounded tail window (8 KiB, growing to 1 MiB until the last record fits).
pub fn read_cursor_last_role(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let mut take = len.min(CURSOR_ROLE_INITIAL_BYTES);
    loop {
        file.seek(SeekFrom::Start(len - take)).ok()?;
        let mut buf = Vec::with_capacity(take as usize);
        (&mut file).take(take).read_to_end(&mut buf).ok()?;
        if buf.is_empty() {
            return None;
        }
        let grow = |take: u64| -> Option<u64> {
            (take < len && take < CURSOR_ROLE_MAX_BYTES)
                .then(|| len.min(CURSOR_ROLE_MAX_BYTES).min(take * 2))
        };
        let mut start = 0;
        if take < len {
            // the window begins mid-record: drop the incomplete first line (or grow when there is no newline)
            match buf.iter().position(|&b| b == b'\n') {
                Some(nl) => start = nl + 1,
                None => {
                    take = grow(take)?;
                    continue;
                }
            }
        }
        let mut last_role = None;
        for line in buf[start..].split(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
                && let Some(role) = v.get("role").and_then(|r| r.as_str())
            {
                last_role = Some(role.to_string());
            }
        }
        if last_role.is_some() {
            return last_role;
        }
        take = grow(take)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn monitor() -> std::sync::Arc<SessionMonitor> {
        SessionMonitor::new(None, PathBuf::from("/nonexistent"))
    }

    fn only(list: Vec<AgentSession>) -> AgentSession {
        assert_eq!(list.len(), 1, "{list:?}");
        list.into_iter().next().unwrap()
    }

    fn transcript(root: &Path, slug: &str, id: &str, text: &str, age: Duration) -> PathBuf {
        let dir = root
            .join("projects")
            .join(slug)
            .join("agent-transcripts")
            .join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{id}.jsonl"));
        std::fs::write(&file, text).unwrap();
        set_age(&file, age);
        file
    }

    fn set_age(file: &Path, age: Duration) {
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn cursor_recent_assistant_turn_is_idle_without_exposing_message_text() {
        let root = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        transcript(
            root.path(),
            "Users-test-Projects-phonebox",
            id,
            "{\"role\":\"user\",\"message\":\"PRIVATE PROMPT\"}\n{\"role\":\"assistant\",\"message\":\"PRIVATE REPLY\"}\n",
            Duration::from_secs(60),
        );
        let ws = root.path().join("workspaceStorage").join("abc");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("workspace.json"),
            r#"{"folder":"file:///Users/test/Projects/phonebox"}"#,
        )
        .unwrap();

        let s = only(monitor().read_cursor(
            &root.path().join("projects"),
            &root.path().join("workspaceStorage"),
        ));
        assert_eq!(s.tool, "cursor");
        assert_eq!(s.project, "phonebox");
        assert_eq!(s.status, "idle");
        assert_eq!(s.waiting_for, None);
        assert_eq!(s.id, format!("cursor:{id}"));
        assert_eq!(s.name, "phonebox-aaaa");
        assert!(!serde_json::to_string(&s).unwrap().contains("PRIVATE"));
    }

    #[test]
    fn cursor_last_user_message_within_busy_window_is_busy() {
        let root = tempfile::tempdir().unwrap();
        transcript(
            root.path(),
            "my-app",
            "11111111-2222-3333-4444-555555555555",
            "{\"role\":\"user\",\"message\":\"hi\"}\n",
            Duration::from_secs(30),
        );
        let s = only(monitor().read_cursor(
            &root.path().join("projects"),
            &root.path().join("missing-ws"),
        ));
        assert_eq!(s.status, "busy");
        assert_eq!(s.waiting_for, None);
        assert_eq!(s.project, "app");
    }

    #[test]
    fn cursor_final_user_record_larger_than_tail_window_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let huge = "x".repeat(12_000);
        let file = transcript(
            root.path(),
            "big-app",
            "22222222-3333-4444-5555-666666666666",
            &format!(
                "{{\"role\":\"assistant\",\"message\":\"earlier\"}}\n{{\"role\":\"user\",\"message\":\"{huge}\"}}\n"
            ),
            Duration::from_secs(20),
        );
        assert_eq!(read_cursor_last_role(&file).as_deref(), Some("user"));
        let s = only(monitor().read_cursor(
            &root.path().join("projects"),
            &root.path().join("missing-ws"),
        ));
        assert_eq!(s.status, "busy");
        assert!(!serde_json::to_string(&s).unwrap().contains(&huge[..32]));
    }

    #[test]
    fn cursor_stale_transcript_is_idle() {
        let root = tempfile::tempdir().unwrap();
        transcript(
            root.path(),
            "demo",
            "99999999-aaaa-bbbb-cccc-dddddddddddd",
            "{\"role\":\"assistant\",\"message\":\"done\"}\n",
            Duration::from_secs(20 * 60),
        );
        let s = only(monitor().read_cursor(
            &root.path().join("projects"),
            &root.path().join("missing-ws"),
        ));
        assert_eq!(s.status, "idle");
    }

    #[test]
    fn cursor_project_fallback_uses_last_slug_segment() {
        for (slug, expected) in [
            ("Users-test-Projects-phonebox", "phonebox"),
            ("1794465413728", "Cursor"),
            ("", "Cursor"),
        ] {
            assert_eq!(cursor_project_fallback(slug), expected, "{slug}");
        }
    }

    #[test]
    fn cursor_project_slug_strips_drive_colon() {
        for (path, expected) in [
            (
                "/Users/test/Projects/phonebox",
                "Users-test-Projects-phonebox",
            ),
            (
                "C:/Users/test/Projects/phonebox-agent",
                "C-Users-test-Projects-phonebox-agent",
            ),
            (
                r"C:\Users\test\Projects\phonebox-agent",
                "C-Users-test-Projects-phonebox-agent",
            ),
        ] {
            assert_eq!(cursor_project_slug(path), expected, "{path}");
        }
    }

    #[test]
    fn cursor_unchanged_transcript_reuses_cached_last_role() {
        let root = tempfile::tempdir().unwrap();
        let file = transcript(
            root.path(),
            "cache-app",
            "33333333-4444-5555-6666-777777777777",
            "{\"role\":\"user\",\"message\":\"go\"}\n",
            Duration::from_secs(15),
        );
        let m = monitor();
        let projects = root.path().join("projects");
        let ws = root.path().join("missing-ws");
        assert_eq!(only(m.read_cursor(&projects, &ws)).status, "busy");
        let meta = std::fs::metadata(&file).unwrap();
        assert!(m.has_cached_cursor_role(&file, meta.modified().unwrap(), meta.len()));
        assert_eq!(only(m.read_cursor(&projects, &ws)).status, "busy");
        assert!(m.has_cached_cursor_role(&file, meta.modified().unwrap(), meta.len()));

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(&mut f, b"{\"role\":\"assistant\",\"message\":\"done\"}\n")
            .unwrap();
        drop(f);
        set_age(&file, Duration::from_secs(5));
        assert_eq!(only(m.read_cursor(&projects, &ws)).status, "idle");
        let meta = std::fs::metadata(&file).unwrap();
        assert!(m.has_cached_cursor_role(&file, meta.modified().unwrap(), meta.len()));
    }

    #[test]
    fn cleanup_folders_and_other_jsonl_files_are_not_sessions() {
        let root = tempfile::tempdir().unwrap();
        let text = "{\"role\":\"user\"}\n";
        transcript(
            root.path(),
            "a/.agent-data-cleanup-1",
            "44444444-0000",
            text,
            Duration::from_secs(5),
        );
        let other = root.path().join("projects").join("b").join("notes");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("x.jsonl"), text).unwrap();
        assert!(
            monitor()
                .read_cursor(
                    &root.path().join("projects"),
                    &root.path().join("missing-ws")
                )
                .is_empty()
        );
    }
}
