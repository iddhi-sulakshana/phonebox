//! Whether this copy runs from the install folder, and the files an install updated from a framework-dependent C#
//! release (up to 0.9.6) still carries (`UpdateService.cs:41-56, 386-409`).

use std::path::{Path, PathBuf};

/// The framework-dependent agent's files: the updater only adds and replaces files, so an install updated to a
/// single-file release keeps them, and nothing loads them any more.
pub const FRAMEWORK_DEPENDENT_FILES: [&str; 11] = [
    "PhoneBoxAgent.dll",
    "PhoneBoxAgent.deps.json",
    "PhoneBoxAgent.runtimeconfig.json",
    "NAudio.dll",
    "NAudio.Core.dll",
    "NAudio.Midi.dll",
    "NAudio.Wasapi.dll",
    "NAudio.WinMM.dll",
    "WinRT.Runtime.dll",
    "Microsoft.Windows.SDK.NET.dll",
    "System.Numerics.Tensors.dll",
];

/// Deletes [`FRAMEWORK_DEPENDENT_FILES`] from a single-file install (the Rust agent always is one); nothing for a
/// framework-dependent one. Missing files and a missing folder are fine.
pub fn remove_framework_dependent_leftovers(dir: &Path, single_file: bool) {
    if !single_file || !dir.is_dir() {
        return;
    }
    for name in FRAMEWORK_DEPENDENT_FILES {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// The folder as compared: absolute, without trailing separators, and (off Windows) with symbolic links resolved
/// when it exists: the home folder may be a link to another disk.
fn resolved(folder: &Path) -> PathBuf {
    let full = std::path::absolute(folder).unwrap_or_else(|_| folder.to_path_buf());
    #[cfg(not(windows))]
    let full = std::fs::canonicalize(&full).unwrap_or(full);
    let text = full.to_string_lossy();
    let trimmed = text.trim_end_matches(['\\', '/']);
    PathBuf::from(if trimmed.is_empty() {
        text.as_ref()
    } else {
        trimmed
    })
}

/// `SameFolder`: the same folder once resolved (in any case on Windows).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    let (a, b) = (resolved(a), resolved(b));
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// `RunningInstalled`: the agent at `exe` is the copy in the install folder (only that one updates itself).
pub fn running_installed(exe: &Path) -> bool {
    running_from(exe, &crate::app_paths::install_dir())
}

/// The agent at `exe` runs from `install_dir`.
pub fn running_from(exe: &Path, install_dir: &Path) -> bool {
    exe.parent()
        .is_some_and(|folder| same_folder(folder, install_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_files(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("phonebox-migrate-")
            .tempdir()
            .unwrap();
        for n in names {
            std::fs::write(dir.path().join(n), "x").unwrap();
        }
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_single_file_agent_removes_the_framework_dependent_files_and_keeps_the_rest() {
        let keep = [
            "PhoneBoxAgent.exe",
            "THIRD-PARTY-NOTICES.txt",
            "config.default.json",
            "widgets.json",
            "aspnetcorev2_inprocess.dll",
            "agent.log",
        ];
        let all: Vec<&str> = FRAMEWORK_DEPENDENT_FILES
            .iter()
            .chain(keep.iter())
            .copied()
            .collect();
        let dir = with_files(&all);
        remove_framework_dependent_leftovers(dir.path(), true);
        let mut expected: Vec<String> = keep.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(names(dir.path()), expected);
    }

    #[test]
    fn a_framework_dependent_agent_keeps_its_files() {
        let dir = with_files(&FRAMEWORK_DEPENDENT_FILES);
        remove_framework_dependent_leftovers(dir.path(), false);
        assert_eq!(names(dir.path()).len(), FRAMEWORK_DEPENDENT_FILES.len());
    }

    #[test]
    fn a_missing_directory_or_partial_set_is_fine() {
        let missing = std::env::temp_dir().join(format!("phonebox-missing-{}", std::process::id()));
        remove_framework_dependent_leftovers(&missing, true);
        let dir = with_files(&["PhoneBoxAgent.dll"]);
        remove_framework_dependent_leftovers(dir.path(), true);
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn the_same_folder_is_found_through_links_and_trailing_separators() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("app");
        std::fs::create_dir(&real).unwrap();
        assert!(same_folder(&real, &dir.path().join("app/")));
        assert!(!same_folder(&real, dir.path()));
        #[cfg(unix)]
        {
            let link = dir.path().join("linked");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(same_folder(&link, &real));
        }
        // a folder that does not exist is compared as it is
        assert!(same_folder(
            &dir.path().join("nope"),
            &dir.path().join("nope")
        ));
    }
}
