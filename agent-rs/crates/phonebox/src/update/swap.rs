//! Installing a downloaded agent (`UpdateService.cs:259-287, 370-382`): unpack it beside the downloads, make sure it
//! starts on this machine (a departure: C# swapped blind and had no way back), then move each file over the installed
//! one, which is kept as `<name>.old` because a running program can be renamed but not overwritten.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const EXE_NAME: &str = if cfg!(windows) {
    "PhoneBoxAgent.exe"
} else {
    "PhoneBoxAgent"
};

/// How long the staged agent may take to answer `--print-install-dir`.
const SELF_TEST: Duration = Duration::from_secs(5);
/// Downloads older than this are deleted at start.
const KEEP_DOWNLOADS: Duration = Duration::from_secs(7 * 24 * 3600);

#[cfg(unix)]
fn make_executable(file: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Unpacks `zip` into a fresh `into` (entries that would leave it are refused) and returns the agent in it.
pub fn stage(zip: &Path, into: &Path) -> Result<PathBuf, String> {
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    archive.extract(into).map_err(|e| e.to_string())?;
    let exe = into.join(EXE_NAME);
    if !exe.is_file() {
        return Err(format!("the agent package has no {EXE_NAME}"));
    }
    Ok(exe)
}

/// The staged agent answers `--print-install-dir` within 5 s; a release that does not start here (another
/// architecture, a damaged or wrongly signed build) is never swapped in. Blocks.
pub fn self_test(exe: &Path) -> Result<(), String> {
    let refused = |why: String| {
        format!(
            "the new agent does not start on this computer ({why}); the update was not installed"
        )
    };
    make_executable(exe).map_err(|e| refused(e.to_string()))?;
    let mut child = std::process::Command::new(exe)
        .arg("--print-install-dir")
        .current_dir(crate::app_paths::neutral_cwd())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| refused(e.to_string()))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(refused(format!("it exited with {status}"))),
            Ok(None) if started.elapsed() < SELF_TEST => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(refused("it did not answer within 5 seconds".into()));
            }
            Err(e) => return Err(refused(e.to_string())),
        }
    }
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files_under(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(suffix);
    PathBuf::from(text)
}

/// Moves every staged file into `install_dir`, keeping each one it replaces as `<name>.old`; then the agent is made
/// executable (the release zips carry no Unix modes). Blocks.
pub fn replace(staged: &Path, install_dir: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    files_under(staged, &mut files).map_err(|e| e.to_string())?;
    for src in files {
        let rel = src.strip_prefix(staged).map_err(|e| e.to_string())?;
        let dst = install_dir.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if dst.exists() {
            let old = with_suffix(&dst, ".old");
            if old.exists() {
                std::fs::remove_file(&old).map_err(|e| e.to_string())?;
            }
            std::fs::rename(&dst, &old).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    }
    make_executable(&install_dir.join(EXE_NAME)).map_err(|e| e.to_string())
}

fn old_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            old_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "old") {
            out.push(path);
        }
    }
}

/// `CleanupOldFiles`: what earlier updates left, best effort: every `*.old` under the install folder, and downloads
/// last written more than 7 days ago.
pub fn cleanup(install_dir: &Path, downloads: &Path, now: SystemTime) {
    let mut old = Vec::new();
    old_files(install_dir, &mut old);
    for file in old {
        let _ = std::fs::remove_file(file);
    }
    let Ok(entries) = std::fs::read_dir(downloads) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > KEEP_DOWNLOADS));
        if stale {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cleanup_removes_old_files_and_week_old_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let (app, downloads) = (dir.path().join("app"), dir.path().join("updates"));
        std::fs::create_dir_all(app.join("sub")).unwrap();
        std::fs::create_dir_all(downloads.join("agent-1.0.0-alpha.1")).unwrap();
        for f in ["PhoneBoxAgent.old", "sub/x.old", "PhoneBoxAgent"] {
            std::fs::write(app.join(f), "x").unwrap();
        }
        for f in ["fresh.zip", "stale.zip"] {
            std::fs::write(downloads.join(f), "x").unwrap();
        }
        let week_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        let stale = std::fs::File::options()
            .write(true)
            .open(downloads.join("stale.zip"))
            .unwrap();
        stale.set_modified(week_ago).unwrap();
        cleanup(&app, &downloads, SystemTime::now() + Duration::from_secs(0));
        assert!(!app.join("PhoneBoxAgent.old").exists() && !app.join("sub/x.old").exists());
        assert!(app.join("PhoneBoxAgent").exists());
        assert!(downloads.join("fresh.zip").exists() && !downloads.join("stale.zip").exists());
        // a missing folder is fine
        cleanup(
            &dir.path().join("none"),
            &dir.path().join("none2"),
            SystemTime::now(),
        );
    }
}
