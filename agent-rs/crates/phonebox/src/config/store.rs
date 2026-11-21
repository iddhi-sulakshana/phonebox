//! Port of `ConfigStore` (`Config.cs`): reads and writes `config.json`; external edits are picked up on the next request.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::normalize::{is_blank, normalize};
use super::write::to_dotnet_json;
use super::{AgentConfig, ConsoleProfile, MAX_CAMERAS, read};
use crate::{app_paths, icon_text};

/// Save refused: the file changed after the caller loaded it.
pub const CONFLICT: &str = "The settings were changed in another tab or window after this one loaded them. Reload the page to see them; the changes made here were not saved.";

/// .NET ticks (100 ns since 0001-01-01) of 1970-01-01, and of 1601-01-01 (what `File.GetLastWriteTimeUtc` returns for a
/// missing file).
pub const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
pub const MISSING_FILE_TICKS: i64 = 504_911_232_000_000_000;

pub struct ConfigStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    config: Arc<AgentConfig>,
    /// Write time (ticks) of the file the settings were read from; None until a read succeeded.
    stamp: Option<i64>,
    /// Write time of the file as last looked at, readable or not.
    seen: i64,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        let config = Arc::new(normalize(AgentConfig::default(), None));
        ConfigStore {
            path,
            inner: Mutex::new(Inner {
                config,
                stamp: None,
                seen: 0,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get_locked(&self, inner: &mut Inner) -> Arc<AgentConfig> {
        let stamp = file_ticks(&self.path);
        inner.seen = stamp;
        if inner.stamp != Some(stamp) {
            match app_paths::read_all_text(&self.path)
                .map_err(|e| e.to_string())
                .and_then(|t| read::parse(&t))
            {
                Ok(cfg) => {
                    inner.config = Arc::new(normalize(cfg, None));
                    inner.stamp = Some(stamp);
                }
                Err(e) => eprintln!("could not read config ({}): {}", self.path.display(), e),
            }
        }
        inner.config.clone()
    }

    pub fn get(&self) -> Arc<AgentConfig> {
        let mut inner = self.lock();
        self.get_locked(&mut inner)
    }

    /// The file's last write time in .NET ticks, as last looked at.
    pub fn version(&self) -> i64 {
        let mut inner = self.lock();
        self.get_locked(&mut inner);
        inner.seen
    }

    /// The version for a JSON reply: as text, since the ticks are beyond what a JavaScript number holds exactly.
    pub fn version_text(&self) -> String {
        self.version().to_string()
    }

    /// True when the header names a version other than the current one; a missing or unparsable one never conflicts.
    pub fn version_conflict(&self, header: Option<&str>) -> bool {
        header
            .and_then(|h| h.trim().parse::<i64>().ok())
            .is_some_and(|expected| expected != self.version())
    }

    /// Advances the version without changing the settings (icon uploads), so other tabs notice.
    pub fn touch(&self) {
        let _inner = self.lock();
        if let Ok(file) = std::fs::File::options().write(true).open(&self.path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// Validates and saves; Ok is the version written. With `expected`, a save over a newer file is refused (CONFLICT).
    pub fn save(&self, json: &str, expected: Option<i64>) -> Result<i64, String> {
        let cfg = read::parse_for_save(json)?;
        let stale: HashSet<String> = self.get().shortcuts.iter().map(|s| s.id.clone()).collect();
        let cfg = normalize(cfg, Some(&stale));
        validate(&cfg)?;

        let mut inner = self.lock();
        if let Some(expected) = expected
            && file_ticks(&self.path) != expected
        {
            return Err(CONFLICT.to_string());
        }
        let io = |e: std::io::Error| format!("could not save the settings: {e}");
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = app_paths::with_suffix(&self.path, ".tmp");
        // owner-only on macOS/Linux: the file holds the Wi-Fi, webhook and Home Assistant keys
        app_paths::write_all_text_private(&tmp, &to_dotnet_json(&cfg)).map_err(io)?;
        if self.path.exists() {
            app_paths::copy_private(&self.path, &app_paths::with_suffix(&self.path, ".bak"))
                .map_err(io)?;
        }
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        inner.config = Arc::new(cfg);
        let written = file_ticks(&self.path);
        inner.stamp = Some(written);
        inner.seen = written;
        Ok(written)
    }

    /// Writes generated keys (and repaired icons) to disk so they stay the same across restarts. A file that cannot be
    /// read is never replaced.
    pub fn persist_generated(&self) {
        let (c, version) = {
            let mut inner = self.lock();
            let c = self.get_locked(&mut inner);
            // the first save needs the missing-file stamp; an existing file is only overwritten from a snapshot we read
            let version = match inner.stamp {
                None if !self.path.exists() => inner.seen,
                None => 0,
                Some(stamp) => stamp,
            };
            (c, version)
        };
        if let Ok(on_disk) = app_paths::read_all_text(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|t| read::parse(&t))
        {
            let damaged_icons = on_disk
                .pages
                .iter()
                .any(|p| icon_text::repair(Some(&p.icon)) != p.icon)
                || on_disk
                    .shortcuts
                    .iter()
                    .any(|s| icon_text::repair(Some(&s.icon)) != s.icon);
            if !damaged_icons
                && on_disk.lan.token == c.lan.token
                && on_disk.lan.webhook_token == c.lan.webhook_token
            {
                return;
            }
        }
        let _ = self.save(&to_dotnet_json(&c), Some(version));
    }
}

/// The checks `ConfigStore.Save` makes after normalizing, with the same messages.
fn validate(cfg: &AgentConfig) -> Result<(), String> {
    for s in &cfg.shortcuts {
        if is_blank(&s.cmd) {
            return Err(format!("Shortcut '{}' has no command", s.label));
        }
    }
    if cfg.console.profiles.len() > MAX_CONSOLE_PROFILES {
        return Err(format!(
            "The app console has more than {MAX_CONSOLE_PROFILES} apps"
        ));
    }
    for p in &cfg.console.profiles {
        validate_console_profile(p)?;
    }
    if cfg.cameras.sources.len() > MAX_CAMERAS {
        return Err(format!("There are more than {MAX_CAMERAS} cameras"));
    }
    for cam in &cfg.cameras.sources {
        if !valid_camera_url(&cam.url) {
            let name = if cam.name.is_empty() {
                "(no name)"
            } else {
                &cam.name
            };
            return Err(format!(
                "Camera '{name}': the address must start with rtsp:// or rtsps://"
            ));
        }
    }
    let w = &cfg.weather;
    if w.lat < -90.0 || w.lat > 90.0 || w.lon < -180.0 || w.lon > 180.0 {
        return Err("Invalid weather location".into());
    }
    if !hh_mm(&cfg.night.start) || !hh_mm(&cfg.night.end) {
        return Err("Night mode times must be in HH:MM format".into());
    }
    Ok(())
}

/// `AppConsole.MaxApps`, `MaxSpec`, `MaxProfiles`.
const MAX_CONSOLE_APPS: usize = 300;
const MAX_CONSOLE_SPEC: usize = 200;
const MAX_CONSOLE_PROFILES: usize = 30;
/// `AppConsole.MaxCombos`: 20 presses x 5 dial clicks x 40 ms plus the 2 s queue wait stay well inside the phone's
/// 20 s deadline.
pub const MAX_CONSOLE_COMBOS: usize = 20;

/// `AppConsole.Validate`: an app, the lengths, and each binding's keys.
fn validate_console_profile(p: &ConsoleProfile) -> Result<(), String> {
    let label = &p.label;
    if p.app_names().is_empty() {
        return Err(format!("App console profile '{label}' has no app"));
    }
    if p.apps.encode_utf16().count() > MAX_CONSOLE_APPS {
        return Err(format!(
            "App console profile '{label}': the app list is longer than {MAX_CONSOLE_APPS} characters"
        ));
    }
    if p.keys.len() > ConsoleProfile::MAX_KEYS {
        return Err(format!(
            "App console profile '{label}' has more than {} keys",
            ConsoleProfile::MAX_KEYS
        ));
    }
    let specs = p
        .keys
        .iter()
        .enumerate()
        .map(|(i, k)| (Some(k.keys.as_str()), format!("key {}", i + 1)))
        .chain([
            (p.dial_left.as_deref(), "dial left".to_string()),
            (p.dial_right.as_deref(), "dial right".to_string()),
            (p.dial_press.as_deref(), "dial press".to_string()),
        ]);
    for (spec, what) in specs {
        let Some(spec) = spec else {
            continue;
        };
        if spec.encode_utf16().count() > MAX_CONSOLE_SPEC {
            return Err(format!(
                "App console profile '{label}', {what}: longer than {MAX_CONSOLE_SPEC} characters"
            ));
        }
        match crate::key_sender::parse(spec) {
            Ok(combos) if combos.len() > MAX_CONSOLE_COMBOS => {
                return Err(format!(
                    "App console profile '{label}', {what}: more than {MAX_CONSOLE_COMBOS} key presses"
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(format!("App console profile '{label}', {what}: {e}")),
        }
    }
    Ok(())
}

/// `CameraFeeds.ValidUrl`: an absolute rtsp:// or rtsps:// address with a host.
pub(crate) fn valid_camera_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("rtsp") || scheme.eq_ignore_ascii_case("rtsps")) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    !host.is_empty()
}

/// `^([01]\d|2[0-3]):[0-5]\d$` (.NET's `$` also matches before a final line feed).
fn hh_mm(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let b = s.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 2 || c.is_ascii_digit())
        && matches!((b[0], b[1]), (b'0' | b'1', _) | (b'2', b'0'..=b'3'))
        && b[3] <= b'5'
}

/// The .NET ticks of a file's last write time, or [`MISSING_FILE_TICKS`] when it does not exist.
pub fn file_ticks(path: &Path) -> i64 {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => UNIX_EPOCH_TICKS + (d.as_nanos() / 100) as i64,
            Err(e) => UNIX_EPOCH_TICKS - (e.duration().as_nanos() / 100) as i64,
        },
        Err(_) => MISSING_FILE_TICKS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::write::to_dotnet_json;
    use std::time::{Duration, SystemTime};

    fn new_store() -> (tempfile::TempDir, ConfigStore, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let store = ConfigStore::new(path.clone());
        (dir, store, path)
    }

    /// `With`: the current settings, edited, as a tab would send them.
    fn with(store: &ConfigStore, edit: impl FnOnce(&mut AgentConfig)) -> String {
        let mut c = (*store.get()).clone();
        edit(&mut c);
        to_dotnet_json(&c)
    }

    fn set_ticks(path: &Path, ticks: i64) {
        let t = SystemTime::UNIX_EPOCH
            + Duration::from_nanos(((ticks - UNIX_EPOCH_TICKS) * 100) as u64);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn pause() {
        std::thread::sleep(Duration::from_millis(20));
    }

    #[test]
    fn version_is_dotnet_ticks_of_mtime() {
        let (_d, store, p) = new_store();
        let t = std::fs::metadata(&p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        assert_eq!(store.version(), UNIX_EPOCH_TICKS + t / 100);
        assert_eq!(file_ticks(&p.with_extension("missing")), MISSING_FILE_TICKS);
    }

    #[test]
    fn save_keeps_backup_and_owner_only_modes() {
        let (_d, store, p) = new_store();
        store.save(r#"{"night":{"start":"01:11"}}"#, None).unwrap();
        let first = std::fs::read_to_string(&p).unwrap();
        store.save(r#"{"night":{"start":"02:22"}}"#, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(crate::app_paths::with_suffix(&p, ".bak")).unwrap(),
            first
        );
        assert_eq!(store.get().night.start, "02:22");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&p), 0o600);
            assert_eq!(mode(&crate::app_paths::with_suffix(&p, ".bak")), 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_and_its_backup_are_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, store, p) = new_store();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        store.save("{}", None).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&crate::app_paths::with_suffix(&p, ".bak")), 0o600);
    }

    #[test]
    fn a_stale_save_is_refused_and_the_newer_change_stays() {
        let (_d, s, _p) = new_store();
        let loaded_by_tab_a = s.version();
        let tab_a = with(&s, |c| c.night.brightness = 0.03);
        pause();
        s.save(
            &with(&s, |c| c.night.start = "01:11".into()),
            Some(s.version()),
        )
        .unwrap();
        assert_eq!(
            s.save(&tab_a, Some(loaded_by_tab_a)),
            Err(CONFLICT.to_string())
        );
        assert_eq!(s.get().night.start, "01:11");
        assert_eq!(s.get().night.brightness, 0.02);
    }

    #[test]
    fn touch_advances_version_so_another_tabs_conflict_check_notices_it() {
        let (_d, s, _p) = new_store();
        let loaded = s.version();
        assert!(!s.version_conflict(Some(&loaded.to_string())));
        assert!(!s.version_conflict(None));
        pause();
        s.touch();
        assert!(s.version_conflict(Some(&loaded.to_string())));
        assert!(!s.version_conflict(Some(&s.version().to_string())));
    }

    #[test]
    fn generated_keys_persist_for_a_new_config_without_replacing_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::new(path.clone());
        let generated = store.get().lan.clone();
        store.persist_generated();
        assert!(path.exists());
        let restarted = ConfigStore::new(path.clone()).get().lan.clone();
        assert_eq!(generated.token, restarted.token);
        assert_eq!(generated.webhook_token, restarted.webhook_token);
        let invalid = "{ incomplete";
        std::fs::write(&path, invalid).unwrap();
        ConfigStore::new(path.clone()).persist_generated();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }

    #[test]
    fn bare_relative_config_filename_persists_generated_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phonebox-config.json");
        let store = ConfigStore::new(path.clone());
        let generated = store.get().lan.clone();
        store.persist_generated();
        assert!(path.exists());
        let loaded = ConfigStore::new(path).get().lan.clone();
        assert_eq!(generated.token, loaded.token);
        assert_eq!(generated.webhook_token, loaded.webhook_token);
    }

    #[test]
    fn deleting_settings_invalidates_the_version_held_by_open_tabs() {
        let (_d, store, path) = new_store();
        let version = store.version();
        let stale = with(&store, |c| c.night.start = "01:11".into());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.save(&stale, Some(version)), Err(CONFLICT.to_string()));
        assert!(!path.exists());
        store.save(&stale, Some(store.version())).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn an_up_to_date_save_and_a_save_without_a_version_go_through() {
        let (_d, s, _p) = new_store();
        s.save(
            &with(&s, |c| c.night.start = "01:11".into()),
            Some(s.version()),
        )
        .unwrap();
        pause();
        s.save(&with(&s, |c| c.night.start = "02:22".into()), None)
            .unwrap();
        assert_eq!(s.get().night.start, "02:22");
    }

    #[test]
    fn display_language_defaults_and_exposes_to_public() {
        // the ToPublic half is ported with /api/config (server::routes_settings)
        let (_d, s, _p) = new_store();
        assert_eq!(s.get().display.language, "auto");
        s.save(r#"{"display":{"language":"tr"}}"#, None).unwrap();
        assert_eq!(s.get().display.language, "tr");
    }

    #[test]
    fn a_display_language_it_does_not_know_follows_the_phone() {
        for (json, expected) in [
            ("\"TR\"", "tr"),
            ("\" de \"", "de"),
            ("\"vi\"", "vi"),
            ("\"xx\"", "auto"),
            ("null", "auto"),
            ("\"\"", "auto"),
        ] {
            let (_d, s, _p) = new_store();
            s.save(&format!(r#"{{"display":{{"language":{json}}}}}"#), None)
                .unwrap();
            assert_eq!(s.get().display.language, expected, "{json}");
        }
    }

    #[test]
    fn embedded_i18n_resource_exists() {
        assert!(
            include_str!("../../../../../android/app/src/main/assets/www/i18n.js")
                .contains("code:")
        );
    }

    #[test]
    fn every_language_the_panel_has_can_be_saved() {
        let js = include_str!("../../../../../android/app/src/main/assets/www/i18n.js");
        let mut codes: Vec<String> = js
            .match_indices("code:")
            .filter_map(|(i, _)| {
                let before = js[..i].trim_end();
                if !before.ends_with('{') {
                    return None;
                }
                let rest = js[i + 5..].trim_start().strip_prefix('\'')?;
                let code: String = rest.chars().take_while(|c| *c != '\'').collect();
                (code.len() >= 2 && code.len() <= 3 && code.chars().all(|c| c.is_ascii_lowercase()))
                    .then_some(code)
            })
            .collect();
        codes.sort();
        codes.dedup();
        assert!(codes.contains(&"en".to_string()));
        let mut known: Vec<String> = crate::config::LANGUAGES
            .iter()
            .filter(|l| **l != "auto")
            .map(|s| s.to_string())
            .collect();
        known.sort();
        assert_eq!(codes, known);
    }

    #[test]
    fn display_hour12_normalizes_and_preserves_wire_types() {
        for (input, normalized, written) in [
            ("{}", "false", "false"),
            (r#"{"display":{"hour12":false}}"#, "false", "false"),
            (r#"{"display":{"hour12":true}}"#, "true", "true"),
            (r#"{"display":{"hour12":"true"}}"#, "true", "true"),
            (r#"{"display":{"hour12":"12h"}}"#, "true", "true"),
            (r#"{"display":{"hour12":"false"}}"#, "false", "false"),
            (r#"{"display":{"hour12":"auto"}}"#, "auto", "\"auto\""),
            (r#"{"display":{"hour12":" AUTO "}}"#, "auto", "\"auto\""),
            (r#"{"display":{"hour12":"bogus"}}"#, "false", "false"),
            (r#"{"display":{"hour12":null}}"#, "false", "false"),
            (r#"{"display":{"hour12":123}}"#, "false", "false"),
            (r#"{"display":{"hour12":["bad"]}}"#, "false", "false"),
        ] {
            let (_d, store, path) = new_store();
            store.save(input, None).unwrap();
            assert_eq!(store.get().display.hour12, normalized, "{input}");
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(doc["display"]["hour12"].to_string(), written, "{input}");
        }
    }

    #[test]
    fn page_references_follow_id_normalization_on_save_and_reload() {
        for (reference, expected) in [
            ("Desk / Live", "desk-live"),
            ("Desk + Live", "desk-live-2"),
            ("kitchen", "kitchen"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            let json = serde_json::json!({
                "pages": [{ "id": "Desk / Live" }, { "id": "Desk + Live" }, { "id": "kitchen" }],
                "display": { "homePage": reference },
                "phones": [{ "id": "desk", "pages": [reference] }],
                "rules": [{ "when": "time", "then": "goto", "page": reference }],
            });
            let store = ConfigStore::new(path.clone());
            store.save(&json.to_string(), None).unwrap();
            for config in [store.get(), ConfigStore::new(path.clone()).get()] {
                assert_eq!(
                    config
                        .pages
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>(),
                    ["desk-live", "desk-live-2", "kitchen"]
                );
                assert_eq!(config.display.home_page.as_deref(), Some(expected));
                assert_eq!(
                    config
                        .pages_for(Some("desk"))
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    expected
                );
                assert_eq!(config.rules[0].page.as_deref(), Some(expected));
            }
        }
    }

    #[test]
    fn a_new_shortcut_never_reuses_a_just_deleted_ones_id() {
        let (_d, store, _p) = new_store();
        store
            .save(
                r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "cmd.exe" } ] }"#,
                None,
            )
            .unwrap();
        assert_eq!(store.get().shortcuts[0].id, "terminal");
        store
            .save(
                r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "wt.exe" } ] }"#,
                None,
            )
            .unwrap();
        assert_eq!(store.get().shortcuts[0].id, "terminal-2");
    }

    #[test]
    fn a_stale_id_rejected_for_a_new_shortcut_does_not_steal_a_kept_shortcuts_id() {
        let (_d, store, _p) = new_store();
        store.save(r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "a.exe" }, { "label": "Terminal", "cmd": "b.exe" } ] }"#, None).unwrap();
        assert_eq!(
            store
                .get()
                .shortcuts
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["terminal", "terminal-2"]
        );
        store
            .save(r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "c.exe" }, { "id": "terminal-2", "label": "Terminal", "cmd": "b.exe" } ] }"#, None)
            .unwrap();
        assert_eq!(
            store
                .get()
                .shortcuts
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["terminal-3", "terminal-2"]
        );
    }

    #[test]
    fn a_blank_id_shortcut_listed_first_does_not_steal_an_explicit_id_listed_after_it() {
        let (_d, store, _p) = new_store();
        store
            .save(
                r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "a.exe" } ] }"#,
                None,
            )
            .unwrap();
        assert_eq!(store.get().shortcuts[0].id, "terminal");
        store
            .save(r#"{ "shortcuts": [ { "label": "Terminal", "cmd": "new.exe" }, { "id": "terminal-2", "label": "Other", "cmd": "x.exe" } ] }"#, None)
            .unwrap();
        assert_eq!(
            store
                .get()
                .shortcuts
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["terminal-3", "terminal-2"]
        );
    }

    #[test]
    fn the_version_goes_out_as_text_that_comes_back_exactly() {
        let (_d, store, _p) = new_store();
        assert!(store.version() > (1i64 << 53));
        let reply = serde_json::json!({ "version": store.version_text() });
        let version = reply["version"].as_str().unwrap();
        assert!(!store.version_conflict(Some(version)));
        store
            .save(
                &to_dotnet_json(&store.get()),
                Some(version.parse().unwrap()),
            )
            .unwrap();
    }

    #[test]
    fn a_rounded_version_is_refused() {
        let (_d, _s, path) = new_store();
        set_ticks(&path, 639_000_000_000_000_123);
        let store = ConfigStore::new(path);
        let rounded = ((store.version() as f64) as i64).to_string();
        assert_ne!(store.version_text(), rounded);
        assert!(store.version_conflict(Some(&rounded)));
    }

    #[test]
    fn save_reports_the_csharp_validation_messages() {
        let (_d, s, _p) = new_store();
        assert_eq!(
            s.save(r#"{"shortcuts":[{"label":"X","cmd":" "}]}"#, None),
            Err("Shortcut 'X' has no command".into())
        );
        assert_eq!(
            s.save(r#"{"weather":{"lat":91}}"#, None),
            Err("Invalid weather location".into())
        );
        assert_eq!(
            s.save(r#"{"night":{"start":"25:99"}}"#, None),
            Err("Night mode times must be in HH:MM format".into())
        );
        assert_eq!(
            s.save(
                r#"{"cameras":{"sources":[{"name":"Door","url":"http://x"}]}}"#,
                None
            ),
            Err("Camera 'Door': the address must start with rtsp:// or rtsps://".into())
        );
        assert!(
            s.save("{ not json", None)
                .unwrap_err()
                .starts_with("Invalid JSON: ")
        );
        let many: Vec<String> = (0..31)
            .map(|i| format!(r#"{{"label":"a{i}","apps":"x"}}"#))
            .collect();
        assert_eq!(
            s.save(
                &format!(r#"{{"console":{{"profiles":[{}]}}}}"#, many.join(",")),
                None
            ),
            Err("The app console has more than 30 apps".into())
        );
        assert_eq!(
            s.save(
                r#"{"console":{"profiles":[{"label":"Edit","apps":" , "}]}}"#,
                None
            ),
            Err("App console profile 'Edit' has no app".into())
        );
    }

    /// Review #6: `File.ReadAllText` reads a UTF-16 file by its byte order mark (PowerShell 5.1's `>` writes one) and
    /// replaces bytes that are not UTF-8; such a file must not read as "no settings" (a later save would replace it).
    #[test]
    fn a_utf16_file_or_stray_bytes_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let text = "{ \"weather\": { \"city\": \"Kandy\", \"lat\": 7.29, \"lon\": 80.63 } }";
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path, &utf16).unwrap();
        assert_eq!(ConfigStore::new(path.clone()).get().weather.city, "Kandy");
        // Windows-1252 "é" in a file saved by an old editor
        let ansi = b"{ \"weather\": { \"city\": \"Pel\xe9\", \"lat\": 7.29, \"lon\": 80.63 } }";
        std::fs::write(&path, ansi).unwrap();
        assert_eq!(ConfigStore::new(path).get().weather.city, "Pel\u{fffd}");
    }
}
