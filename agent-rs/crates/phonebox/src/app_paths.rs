//! Port of `AppPaths.cs`: file locations. The install folder is replaced on update; settings and logs are kept.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "PhoneBox";

/// Windows: `%APPDATA%\PhoneBox`. Linux: `$XDG_CONFIG_HOME/PhoneBox` (`~/.config`). macOS: `~/Library/Application Support/PhoneBox`.
pub fn config_dir() -> PathBuf {
    if cfg!(windows) {
        env_dir("APPDATA", "AppData/Roaming").join(APP_NAME)
    } else if cfg!(target_os = "macos") {
        home()
            .join("Library")
            .join("Application Support")
            .join(APP_NAME)
    } else {
        xdg("XDG_CONFIG_HOME", ".config").join(APP_NAME)
    }
}

/// Windows: `%LOCALAPPDATA%\PhoneBox`. Linux: `$XDG_DATA_HOME/PhoneBox` (`~/.local/share`). macOS: same as the settings.
pub fn local_dir() -> PathBuf {
    if cfg!(windows) {
        env_dir("LOCALAPPDATA", "AppData/Local").join(APP_NAME)
    } else if cfg!(target_os = "macos") {
        home()
            .join("Library")
            .join("Application Support")
            .join(APP_NAME)
    } else {
        xdg("XDG_DATA_HOME", ".local/share").join(APP_NAME)
    }
}

/// The user's profile folder (`Environment.SpecialFolder.UserProfile`): `HOME` on macOS/Linux, `USERPROFILE` on Windows.
/// `File.ReadAllText`: the encoding a byte order mark names (UTF-8, UTF-16 LE/BE, UTF-32 LE/BE), else UTF-8; bytes that
/// do not decode become U+FFFD instead of failing the read, and the mark itself is dropped.
pub fn read_all_text(path: &Path) -> std::io::Result<String> {
    Ok(decode_text(&std::fs::read(path)?))
}

pub fn decode_text(b: &[u8]) -> String {
    fn utf16(bytes: &[u8], le: bool) -> String {
        let (units, rest) = bytes.as_chunks::<2>();
        let decode = |c: &[u8; 2]| {
            if le {
                u16::from_le_bytes(*c)
            } else {
                u16::from_be_bytes(*c)
            }
        };
        let mut text = String::from_utf16_lossy(&units.iter().map(decode).collect::<Vec<_>>());
        if !rest.is_empty() {
            text.push('\u{fffd}');
        }
        text
    }
    fn utf32(bytes: &[u8], le: bool) -> String {
        let (units, rest) = bytes.as_chunks::<4>();
        let decode = |c: &[u8; 4]| {
            if le {
                u32::from_le_bytes(*c)
            } else {
                u32::from_be_bytes(*c)
            }
        };
        let mut text: String = units
            .iter()
            .map(|c| char::from_u32(decode(c)).unwrap_or('\u{fffd}'))
            .collect();
        if !rest.is_empty() {
            text.push('\u{fffd}');
        }
        text
    }
    match b {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, 0, 0, rest @ ..] => utf32(rest, true),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0, 0, 0xFE, 0xFF, rest @ ..] => utf32(rest, false),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

pub fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn xdg(variable: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(variable) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

fn env_dir(variable: &str, fallback_under_home: &str) -> PathBuf {
    xdg(variable, fallback_under_home)
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn log_path() -> PathBuf {
    local_dir().join("agent.log")
}

/// Where install scripts put the agent; self-update only works for this copy.
pub fn install_dir() -> PathBuf {
    local_dir().join("app")
}

/// "windows", "linux" or "macos".
pub fn os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Release asset suffix: win-x64, linux-x64, linux-arm64, osx-arm64 or osx-x64.
pub fn rid() -> &'static str {
    match (os(), cfg!(target_arch = "aarch64")) {
        ("windows", _) => "win-x64",
        ("macos", true) => "osx-arm64",
        ("macos", false) => "osx-x64",
        (_, true) => "linux-arm64",
        (_, false) => "linux-x64",
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Writes the default settings file on first run.
pub fn ensure_config() -> io::Result<()> {
    let path = config_path();
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(config_dir())?;
    let defaults: &[u8] = if cfg!(target_os = "macos") {
        include_bytes!("../../../../agent/PhoneBox.Agent/config.default.macos.json")
    } else if cfg!(windows) {
        include_bytes!("../../../../agent/PhoneBox.Agent/config.default.json")
    } else {
        include_bytes!("../../../../agent/PhoneBox.Agent/config.default.linux.json")
    };
    let mut file = create_private(&path)?;
    io::Write::write_all(&mut file, defaults)
}

/// Tightens the settings file in use and its backup to owner only on macOS/Linux.
pub fn restrict_config(config_path: &Path) {
    restrict_to_owner(config_path);
    restrict_to_owner(&with_suffix(config_path, ".bak"));
}

/// `path` with `suffix` appended to its file name (`config.json` → `config.json.bak`).
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Creates a file only its owner can read on macOS/Linux (0600), replacing any file at `path`.
pub fn create_private(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // an existing file is removed first and the new one is created exclusively, so the content never lands in a
        // file with another owner or mode (a leftover or planted .tmp)
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::create(path)
    }
}

/// Copies a file into a new owner-only file.
pub fn copy_private(source: &Path, destination: &Path) -> io::Result<()> {
    let mut src = File::open(source)?;
    let mut dst = create_private(destination)?;
    io::copy(&mut src, &mut dst).map(|_| ())
}

/// Writes `text` (UTF-8, no BOM) into a file only its owner can read.
pub fn write_all_text_private(path: &Path, text: &str) -> io::Result<()> {
    let mut file = create_private(path)?;
    io::Write::write_all(&mut file, text.as_bytes())
}

/// Sets an existing file to 0600 on macOS/Linux; no-op on Windows or when the file is missing.
pub fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path)
            && meta.is_file()
            && meta.permissions().mode() & 0o777 != 0o600
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, m: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
    }

    #[test]
    fn restrict_to_owner_tightens_an_existing_file_and_ignores_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        restrict_to_owner(&path); // missing: no panic
        write_all_text_private(&path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            set_mode(&path, 0o604);
            restrict_to_owner(&path);
            assert_eq!(mode(&path), 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_private_replaces_a_leftover_file_instead_of_writing_into_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json.tmp");
        std::fs::write(&path, "old leftover content that is longer").unwrap();
        set_mode(&path, 0o644);
        write_all_text_private(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn restrict_config_tightens_a_backup_even_without_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.json");
        let bak = dir.path().join("custom.json.bak");
        std::fs::write(&bak, "{}").unwrap();
        set_mode(&bak, 0o604);
        restrict_config(&path);
        assert_eq!(mode(&bak), 0o600);
    }

    #[test]
    fn copy_private_copies_into_an_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a.json"), dir.path().join("a.json.bak"));
        std::fs::write(&src, "{\"x\":2}").unwrap();
        copy_private(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "{\"x\":2}");
        #[cfg(unix)]
        assert_eq!(mode(&dst), 0o600);
    }

    #[test]
    fn folders_follow_the_platform_rules() {
        let cfg = config_dir();
        assert!(cfg.ends_with(APP_NAME));
        assert_eq!(config_path(), cfg.join("config.json"));
        assert_eq!(install_dir(), local_dir().join("app"));
        assert_eq!(log_path(), local_dir().join("agent.log"));
        #[cfg(target_os = "macos")]
        assert!(cfg.ends_with("Library/Application Support/PhoneBox") && local_dir() == cfg);
        #[cfg(target_os = "linux")]
        assert!(local_dir().ends_with("PhoneBox") && local_dir() != cfg);
        assert!(["windows", "linux", "macos"].contains(&os()));
        assert!(
            [
                "win-x64",
                "linux-x64",
                "linux-arm64",
                "osx-arm64",
                "osx-x64"
            ]
            .contains(&rid())
        );
    }
}
