//! `ShortcutIcons`: real app icons for shortcuts whose command is an actual program on disk (an .exe, an .app bundle, a
//! Linux binary). Shell addresses (`shell:AppsFolder\…`), protocol handlers (`ms-settings:`), URLs, `builtin:`
//! commands, and Windows' App Execution Alias stubs for packaged apps (wt.exe: a reparse point with no icon of its own)
//! have no real icon to read, so these give None for them and the phone keeps showing the chosen emoji.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::config::Shortcut;
use crate::shortcut_icon_uploads::ShortcutIconUploads;

/// Keyed by command, not shortcut id: resolving a path walks every PATH folder on Windows, so repeating it for every
/// shortcut on every `/api/config` request is wasteful. A command only changes when the config is saved, so the
/// settings PUT clears this then. Only a successful resolution is cached, and it is checked again on use: a program
/// installed later must be found without waiting for the next save, and one removed must stop being advertised.
fn resolved_paths() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn cached_resolve_path(cmd: &str) -> Option<String> {
    let cached = resolved_paths().get(cmd).cloned();
    if let Some(cached) = cached {
        if is_resolved_program(&cached) {
            return Some(cached);
        }
        let mut cache = resolved_paths();
        if cache.get(cmd) == Some(&cached) {
            cache.remove(cmd);
        }
    }
    let resolved = resolve_path(cmd)?;
    resolved_paths().insert(cmd.to_string(), resolved.clone());
    Some(resolved)
}

/// Cheap check for whether [`icon`] is worth asking for, without extracting one.
pub fn has_icon(sc: &Shortcut) -> bool {
    cached_resolve_path(&sc.cmd).is_some()
}

/// A value that changes when the detected program's own icon could have changed (it was reinstalled): the path, the
/// size and the last write time together, so a program that is merely touched without changing rarely moves it. A
/// macOS .app is a folder, so its size is 0. Folded to 32 bits, as the C# agent's `HashCode.Combine` (which is seeded
/// per process; this one is stable, and both only need to change when the program does).
pub fn revision(sc: &Shortcut) -> Option<i64> {
    use std::hash::{Hash, Hasher};
    let path = cached_resolve_path(&sc.cmd)?;
    let meta = std::fs::metadata(&path).ok()?;
    let size = if meta.is_dir() { 0 } else { meta.len() };
    let ticks = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() / 100)
        .unwrap_or(0);
    let mut h = std::hash::DefaultHasher::new();
    (&path, size, ticks).hash(&mut h);
    let full = h.finish();
    Some(i64::from((full ^ (full >> 32)) as u32 as i32))
}

pub fn clear_has_icon_cache() {
    resolved_paths().clear();
}

/// The picture `/api/shortcuts/icon` serves: an upload first, else the program's own icon. With `upload_only` (a Big
/// button filling itself with the picture) only the upload, so an upload removed after the phone read its config is
/// never replaced by a program icon stretched across the key.
pub fn picture(
    sc: &Shortcut,
    uploads: &ShortcutIconUploads,
    upload_only: bool,
) -> Option<(Vec<u8>, &'static str)> {
    uploads.read(&sc.id).or_else(|| {
        if upload_only {
            None
        } else {
            icon(sc).map(|png| (png, "image/png"))
        }
    })
}

/// The program's own icon as PNG bytes.
pub fn icon(sc: &Shortcut) -> Option<Vec<u8>> {
    let path = resolve_path(&sc.cmd)?;
    #[cfg(windows)]
    {
        crate::win_icon::associated_icon_png(&path)
    }
    #[cfg(not(windows))]
    {
        let file = path.rsplit('/').next().unwrap_or(&path);
        let name = file.rfind('.').map_or(file, |dot| &file[..dot]);
        // unix_proc::icon only recognises a macOS bundle when the path contains ".app/"; a bare "X.app" has nothing
        // after it
        let probe = if ends_with_ignore_case(&path, ".app") {
            format!("{path}/")
        } else {
            path.clone()
        };
        crate::unix::unix_proc::icon(name, Some(&probe))
    }
}

#[cfg(not(windows))]
fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.is_char_boundary(s.len() - suffix.len())
        && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// True for an actual file path (`C:\…\app.exe`, `/usr/bin/x`, `code`, `~/bin/x`); false for a `builtin:` command, a
/// URL, a protocol handler (`ms-settings:`) or a shell address (`shell:AppsFolder\…`), none of which name a file.
pub fn looks_like_file_path(cmd: &str) -> bool {
    if cmd.trim().is_empty() {
        return false;
    }
    let cmd = cmd.trim().trim_matches('"');
    if cmd
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("builtin:"))
    {
        return false;
    }
    // a drive letter ("C:\…") is a path; any other "word:" is a shell address or protocol handler
    match cmd.encode_utf16().position(|u| u == u16::from(b':')) {
        None => true,
        Some(1) => cmd.chars().next().is_some_and(char::is_alphabetic),
        Some(_) => false,
    }
}

/// The real file behind a shortcut's command, or None when it is not a program with its own icon (a shell address, a
/// protocol handler, a URL, a `builtin:` command, or an App Execution Alias stub).
fn resolve_path(cmd: &str) -> Option<String> {
    if !looks_like_file_path(cmd) {
        return None;
    }
    let mut cmd = cmd.trim().trim_matches('"').to_string();
    if let Some(rest) = cmd.strip_prefix('~') {
        cmd = format!("{}{rest}", crate::app_paths::home().to_string_lossy());
    }
    #[cfg(windows)]
    {
        if is_path_rooted(&cmd) {
            return is_resolved_program(&cmd).then_some(cmd);
        }
        let name = if cmd.contains('.') {
            cmd.clone()
        } else {
            format!("{cmd}.exe")
        };
        let windows = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("windir"))
            .unwrap_or_else(|_| r"C:\Windows".into());
        let path = std::env::var("PATH").unwrap_or_default();
        let dirs = path
            .split(';')
            .map(str::to_string)
            .chain([
                format!(r"{windows}\system32"),
                format!(r"{windows}\SysWOW64"),
            ])
            .collect::<Vec<_>>();
        dirs.iter()
            .map(|dir| std::path::Path::new(dir).join(&name))
            .map(|p| p.to_string_lossy().into_owned())
            .find(|full| is_resolved_program(full))
    }
    #[cfg(not(windows))]
    {
        if ends_with_ignore_case(&cmd, ".app") || cmd.contains('/') {
            return is_resolved_program(&cmd).then_some(cmd);
        }
        // a bare command name: resolve it through PATH, like the shell would
        crate::unix::sh::which(&cmd).map(|p| p.to_string_lossy().into_owned())
    }
}

/// `Path.IsPathRooted` on Windows: a leading separator, or a drive letter and colon.
#[cfg(windows)]
fn is_path_rooted(path: &str) -> bool {
    let b = path.as_bytes();
    matches!(b.first(), Some(b'\\' | b'/'))
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// A real executable with its own icon resource. An App Execution Alias (the stub Windows installs in WindowsApps\ for
/// a packaged app, e.g. wt.exe) is a reparse point, not a real PE file: Windows hands back a generic "unknown program"
/// icon for it instead of the app's real one, so those are rejected (as is any other reparse point).
#[cfg(windows)]
fn is_resolved_program(path: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| !m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// An .app bundle is a folder; anything else must be a file (`File.Exists`: whatever is there and is not a folder).
#[cfg(not(windows))]
fn is_resolved_program(path: &str) -> bool {
    if ends_with_ignore_case(path, ".app") {
        std::path::Path::new(path).is_dir()
    } else {
        std::fs::metadata(path).is_ok_and(|m| !m.is_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(cmd: &str) -> Shortcut {
        Shortcut {
            cmd: cmd.into(),
            ..Default::default()
        }
    }

    #[test]
    fn recognises_whether_a_command_names_a_real_file() {
        for (cmd, expected) in [
            ("", false),
            ("   ", false),
            ("builtin:lock", false),
            ("builtin:mute", false),
            ("BUILTIN:MONITOR-OFF", false),
            ("https://github.com", false),
            ("http://example.com", false),
            ("ms-settings:", false),
            ("ms-screenclip:", false),
            (r"shell:AppsFolder\Microsoft.VisualStudioCode", false),
            ("wt.exe", true),
            ("explorer.exe", true),
            ("code", true),
            (r"C:\Program Files\App\app.exe", true),
            ("/usr/bin/firefox", true),
            ("/Applications/Safari.app", true),
            ("~/bin/tool", true),
            // quotes and spaces around a path are not part of it
            ("  \"D:\\Tools\\x.exe\"  ", true),
            ("é:\\x", true),
            ("1:\\x", false),
        ] {
            assert_eq!(looks_like_file_path(cmd), expected, "{cmd:?}");
        }
    }

    #[test]
    fn commands_without_a_file_have_no_icon() {
        for cmd in [
            "builtin:lock",
            "https://github.com",
            "ms-settings:",
            r"shell:AppsFolder\Microsoft.VisualStudioCode",
        ] {
            assert!(icon(&shortcut(cmd)).is_none(), "{cmd}");
        }
    }

    #[test]
    fn a_missing_program_has_no_icon() {
        assert!(icon(&shortcut("this-program-does-not-exist-12345")).is_none());
    }

    #[test]
    fn has_icon_agrees_with_icon() {
        for cmd in [
            "builtin:lock",
            "https://github.com",
            "this-program-does-not-exist-12345",
        ] {
            assert!(!has_icon(&shortcut(cmd)), "{cmd}");
        }
    }

    #[test]
    fn has_icon_cache_survives_a_clear_with_the_same_answer() {
        let sc = shortcut("this-cache-test-command-does-not-exist");
        assert!(!has_icon(&sc));
        clear_has_icon_cache();
        assert!(!has_icon(&sc));
    }

    #[test]
    fn removing_a_resolved_program_stops_advertising_its_icon() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("phonebox-shortcut-icon.exe");
        let sc = shortcut(&file.to_string_lossy().replace('\\', "/"));
        std::fs::write(&file, "fixture").unwrap();
        assert!(has_icon(&sc));
        assert!(revision(&sc).is_some());

        std::fs::remove_file(&file).unwrap();
        assert!(!has_icon(&sc));
        assert_eq!(revision(&sc), None);

        std::fs::write(&file, "replacement").unwrap();
        assert!(has_icon(&sc));
        assert!(revision(&sc).is_some());
    }

    #[test]
    fn revision_agrees_with_has_icon() {
        for cmd in ["builtin:lock", "this-program-does-not-exist-12345"] {
            assert_eq!(revision(&shortcut(cmd)), None, "{cmd}");
        }
    }

    #[test]
    fn a_reinstalled_program_changes_the_revision_and_a_touch_alone_does_not_matter_to_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        std::fs::write(&file, "v1").unwrap();
        let sc = shortcut(&file.to_string_lossy());
        let first = revision(&sc).unwrap();
        assert_eq!(revision(&sc), Some(first)); // stable while nothing changes
        std::fs::write(&file, "version two").unwrap();
        assert_ne!(revision(&sc), Some(first));
        // it fits a 32-bit int, as HashCode.Combine does
        assert!(i32::try_from(revision(&sc).unwrap()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_name_resolves_through_path_and_a_folder_is_not_a_program() {
        assert!(has_icon(&shortcut("sh")));
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_icon(&shortcut(&dir.path().to_string_lossy())));
        // an .app is a folder, and counts only as one
        let app = dir.path().join("Tool.app");
        assert!(!has_icon(&shortcut(&app.to_string_lossy())));
        std::fs::create_dir(&app).unwrap();
        assert!(has_icon(&shortcut(&app.to_string_lossy())));
        assert!(revision(&shortcut(&app.to_string_lossy())).is_some());
    }

    #[test]
    fn a_leading_tilde_is_the_home_folder() {
        let home = crate::app_paths::home();
        let dir = tempfile::tempdir_in(&home).unwrap();
        let file = dir.path().join("phonebox-tilde.exe");
        std::fs::write(&file, "x").unwrap();
        let rel = file
            .strip_prefix(&home)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert!(has_icon(&shortcut(&format!("~/{rel}"))));
    }
}
