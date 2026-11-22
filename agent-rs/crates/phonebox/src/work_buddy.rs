//! Port of `WorkBuddy.cs`: WorkBuddy answers the same PermissionRequest hook Claude Code does, but keeps its sessions
//! in `~/.workbuddy`, so the approval card would show the folder name ("2026-11-16-08-55-00"). WorkBuddy stores the
//! name you gave each workspace in `.workbuddy/workspace-display-names.json`; reading that keeps the card on
//! "Quarterly report" instead. The cache is per instance (one for the app).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type Reader = Box<dyn Fn(&Path) -> std::io::Result<String> + Send + Sync>;

#[derive(Default)]
struct Cached {
    stamp: String,
    file: Option<PathBuf>,
    names: HashMap<String, String>,
}

pub struct WorkBuddyNames {
    cached: Mutex<Cached>,
    read: Reader,
}

impl Default for WorkBuddyNames {
    fn default() -> Self {
        Self::with_reader(Box::new(|p: &Path| crate::app_paths::read_all_text(p)))
    }
}

impl WorkBuddyNames {
    pub fn new() -> Self {
        Self::default()
    }

    /// With another file reader (tests: a read that fails like a locked file).
    pub fn with_reader(read: Reader) -> Self {
        WorkBuddyNames {
            cached: Mutex::new(Cached::default()),
            read,
        }
    }

    /// The display name of a WorkBuddy workspace, or None when WorkBuddy is not installed or has no name for it.
    pub fn for_cwd(&self, cwd: Option<&str>, home: &Path) -> Option<String> {
        let cwd = cwd.filter(|c| !c.trim().is_empty())?;
        self.load(home)
            .get(&normalize(cwd))
            .filter(|n| !n.is_empty())
            .cloned()
    }

    fn load(&self, home: &Path) -> HashMap<String, String> {
        if home.as_os_str().is_empty() {
            return HashMap::new();
        }
        let file = home.join(".workbuddy").join("workspace-display-names.json");
        // the hook runs on every permission request, so the file is only re-read when it actually changed; the path is
        // part of the stamp: two homes can hold files of the same age and size
        let stamp = match std::fs::metadata(&file) {
            Ok(m) if m.is_file() => {
                let ticks = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{}|{ticks}|{}", file.display(), m.len())
            }
            _ => format!("{}|-", file.display()),
        };
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if stamp == cached.stamp {
            return cached.names.clone();
        }
        match self.read_names(&file) {
            // a read that failed is not a refresh: the old names stay (for this file) and the next request tries again
            None => {
                if cached.file.as_deref() == Some(file.as_path()) {
                    cached.names.clone()
                } else {
                    HashMap::new()
                }
            }
            Some(names) => {
                cached.file = Some(file);
                cached.stamp = stamp;
                cached.names = names.clone();
                names
            }
        }
    }

    /// The names, or None when the read did not settle (locked or half-written: try again on the next request). No
    /// WorkBuddy, or no file yet, is a definite empty answer.
    fn read_names(&self, file: &Path) -> Option<HashMap<String, String>> {
        let text = match (self.read)(file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(HashMap::new()),
            Err(_) => return None,
        };
        let root: serde_json::Value = serde_json::from_str(&text).ok()?;
        let mut names = HashMap::new();
        let Some(workspaces) = root.get("workspaces").and_then(|w| w.as_object()) else {
            return Some(names);
        };
        for (key, entry) in workspaces {
            if !entry.is_object() {
                continue;
            }
            let Some(name) = entry
                .get("displayName")
                .and_then(|v| v.as_str())
                .filter(|n| !n.trim().is_empty())
            else {
                continue;
            };
            names.insert(normalize(key), name.to_string());
            // the entry carries the real path as well: use it in case the key is ever encoded differently
            if let Some(path) = entry
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|p| !p.is_empty())
            {
                names.insert(normalize(path), name.to_string());
            }
        }
        Some(names)
    }
}

/// WorkBuddy keys its map with lowercase forward slashes, so every lookup and key goes through this.
pub fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspaces(home: &Path, entries: &[(&str, &str)]) -> PathBuf {
        let dir = home.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let mut map = serde_json::Map::new();
        for (path, name) in entries {
            let key = path.replace('\\', "/").to_lowercase();
            map.insert(
                key.clone(),
                json!({"id": key, "path": path, "displayName": name, "updatedAt": 1}),
            );
        }
        let file = dir.join("workspace-display-names.json");
        std::fs::write(&file, json!({"version": 1, "workspaces": map}).to_string()).unwrap();
        file
    }

    const DIR: &str = r"C:\Users\me\WorkBuddy\2026-11-16-08-55-00";

    #[test]
    fn reads_the_workspace_name() {
        let home = tempfile::tempdir().unwrap();
        workspaces(home.path(), &[(DIR, "季度报告整理")]);
        assert_eq!(
            WorkBuddyNames::new()
                .for_cwd(Some(DIR), home.path())
                .as_deref(),
            Some("季度报告整理")
        );
    }

    #[test]
    fn ignores_case_slashes_and_a_trailing_slash() {
        let home = tempfile::tempdir().unwrap();
        workspaces(home.path(), &[(DIR, "季度报告整理")]);
        let got = WorkBuddyNames::new().for_cwd(
            Some("c:/users/me/workbuddy/2026-11-16-08-55-00/"),
            home.path(),
        );
        assert_eq!(got.as_deref(), Some("季度报告整理"));
    }

    #[test]
    fn null_when_work_buddy_has_no_name_for_the_directory() {
        let home = tempfile::tempdir().unwrap();
        workspaces(
            home.path(),
            &[(r"C:\Users\me\WorkBuddy\other", "Another workspace")],
        );
        assert_eq!(WorkBuddyNames::new().for_cwd(Some(DIR), home.path()), None);
    }

    #[test]
    fn null_when_the_file_is_missing_or_broken() {
        let home = tempfile::tempdir().unwrap();
        let names = WorkBuddyNames::new();
        assert_eq!(names.for_cwd(Some(r"C:\work\project"), home.path()), None);
        let dir = home.path().join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workspace-display-names.json"), "{ not json").unwrap();
        assert_eq!(names.for_cwd(Some(r"C:\work\project"), home.path()), None);
        assert_eq!(names.for_cwd(Some("  "), home.path()), None);
        assert_eq!(names.for_cwd(None, home.path()), None);
    }

    #[test]
    fn picks_up_a_new_name_without_a_restart() {
        let home = tempfile::tempdir().unwrap();
        let file = workspaces(home.path(), &[(r"C:\work\project", "First name")]);
        let names = WorkBuddyNames::new();
        assert_eq!(
            names
                .for_cwd(Some(r"C:\work\project"), home.path())
                .as_deref(),
            Some("First name")
        );
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("First name", "Renamed");
        std::fs::write(&file, text).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            names
                .for_cwd(Some(r"C:\work\project"), home.path())
                .as_deref(),
            Some("Renamed")
        );
    }

    #[test]
    fn broken_workspace_file_never_uses_another_homes_names() {
        let home = tempfile::tempdir().unwrap();
        let project = r"C:\work\project";
        workspaces(home.path(), &[(project, "First workspace")]);
        let names = WorkBuddyNames::new();
        assert_eq!(
            names.for_cwd(Some(project), home.path()).as_deref(),
            Some("First workspace")
        );

        let other = home.path().join("other");
        let dir = other.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("workspace-display-names.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(
            names.for_cwd(Some(project), &other),
            None,
            "no workspace name from the other home"
        );
        assert_eq!(
            names.for_cwd(Some(project), home.path()).as_deref(),
            Some("First workspace")
        );

        std::fs::write(
            &file,
            json!({"workspaces": {project: {"displayName": "Second name"}}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            names.for_cwd(Some(project), &other).as_deref(),
            Some("Second name")
        );
        std::fs::write(&file, "{ half-written").unwrap();
        assert_eq!(
            names.for_cwd(Some(project), &other).as_deref(),
            Some("Second name")
        );
    }

    #[test]
    fn a_read_that_failed_is_retried_instead_of_remembered() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let home = tempfile::tempdir().unwrap();
        workspaces(home.path(), &[(r"C:\work\project", "First name")]);
        let locked = std::sync::Arc::new(AtomicBool::new(true));
        let held = locked.clone();
        let names = WorkBuddyNames::with_reader(Box::new(move |p: &Path| {
            if held.load(Ordering::SeqCst) {
                Err(std::io::Error::other(
                    "The process cannot access the file because it is being used",
                ))
            } else {
                std::fs::read_to_string(p)
            }
        }));
        // this read fails: remembering that failure would leave the card on the folder name
        assert_eq!(names.for_cwd(Some(r"C:\work\project"), home.path()), None);
        locked.store(false, Ordering::SeqCst);
        assert_eq!(
            names
                .for_cwd(Some(r"C:\work\project"), home.path())
                .as_deref(),
            Some("First name")
        );
    }

    #[test]
    fn keys_and_paths_are_normalized_the_same_way() {
        assert_eq!(normalize(r"C:\Users\Me\Dir\\"), "c:/users/me/dir");
        assert_eq!(normalize("/Home/X/"), "/home/x");
    }
}
