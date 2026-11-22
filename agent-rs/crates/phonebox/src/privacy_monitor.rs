//! Port of `PrivacyMonitor.cs` (and `CameraActivity.cs` on Windows): is the microphone or the camera in use, and by
//! which app? For the "On air" widget and rules.
//!  - Windows: recording sessions on the microphones (Core Audio) and the camera streams Media Foundation reports. The
//!    privacy consent store (behind the taskbar's microphone and camera icons) is the fallback where those can't be read.
//!  - Linux: ALSA capture streams that are running, and processes with a /dev/video* device open.
//!  - macOS: not detectable without extra permissions; reported as unknown.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::server::Stop;

const CATEGORY: &str = "PhoneBox.Agent.PrivacyMonitor";

/// `mic`/`camera`: whether an app is using it; None where this cannot be detected (macOS).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacySnapshot {
    pub mic: Option<bool>,
    pub camera: Option<bool>,
    pub mic_apps: Vec<String>,
    pub camera_apps: Vec<String>,
}

/// What the rule engine reads (a fake in its tests).
pub trait PrivacySource: Send + Sync {
    /// None until the first read.
    fn snapshot(&self) -> Option<PrivacySnapshot>;
}

#[derive(Default)]
pub struct PrivacyMonitor {
    snap: Mutex<Option<PrivacySnapshot>>,
    /// The Media Foundation camera monitor, started on the first read (Windows).
    #[cfg(windows)]
    camera: windows_read::CameraActivity,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PrivacySource for PrivacyMonitor {
    fn snapshot(&self) -> Option<PrivacySnapshot> {
        lock(&self.snap).clone()
    }
}

/// The `privacy` part of `/api/state`: null until the first read.
pub struct PrivacyState(pub Arc<PrivacyMonitor>);

impl crate::state::StateSource for PrivacyState {
    fn snapshot(&self) -> Value {
        serde_json::to_value(PrivacySource::snapshot(&*self.0)).unwrap_or(Value::Null)
    }
}

impl PrivacyMonitor {
    pub fn new() -> Arc<PrivacyMonitor> {
        Arc::default()
    }

    /// Reads every 2 s until `stop` (macOS: "unknown" once).
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            if cfg!(target_os = "macos") {
                *lock(&me.snap) = Some(PrivacySnapshot {
                    mic: None,
                    camera: None,
                    mic_apps: vec![],
                    camera_apps: vec![],
                });
                return;
            }
            let mut last_log: Option<String> = None;
            while !stop.is_stopped() {
                let reader = me.clone();
                // the process table, /proc and COM: off the async workers
                match tokio::task::spawn_blocking(move || reader.read()).await {
                    Ok((mic, camera)) => {
                        let line = format!(
                            "mic: {}, camera: {}",
                            apps_or_off(&mic),
                            apps_or_off(&camera)
                        );
                        *lock(&me.snap) = Some(PrivacySnapshot {
                            mic: Some(!mic.is_empty()),
                            camera: Some(!camera.is_empty()),
                            mic_apps: mic,
                            camera_apps: camera,
                        });
                        if last_log.as_deref() != Some(line.as_str()) {
                            if last_log.is_some() {
                                crate::log::info(CATEGORY, &line);
                            }
                            last_log = Some(line);
                        }
                    }
                    Err(e) => crate::log::debug(CATEGORY, &format!("privacy: {e}")),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    _ = stop.stopped() => return,
                }
            }
        })
    }

    /// The apps using a microphone and a camera now.
    fn read(&self) -> (Vec<String>, Vec<String>) {
        #[cfg(windows)]
        {
            windows_read::read(&self.camera)
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            (linux::mic(), linux::camera())
        }
        #[cfg(target_os = "macos")]
        {
            (vec![], vec![])
        }
    }
}

fn apps_or_off(apps: &[String]) -> String {
    if apps.is_empty() {
        "off".to_string()
    } else {
        apps.join(", ")
    }
}

/// Each name once, ordinally (the order kept).
#[cfg(not(target_os = "macos"))]
fn ordinal_distinct(apps: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    apps.into_iter()
        .filter(|a| seen.insert(a.clone()))
        .collect()
}

/// A consent-store entry of a desktop app, as its program's name: "C:#Program Files#Zoom#bin#Zoom.exe" -> "Zoom".
#[cfg(any(windows, test))]
fn friendly_exe(key: &str) -> String {
    let path = key.replace('#', "\\");
    let file = path.rsplit(['\\', '/', ':']).next().unwrap_or("");
    match file.rfind('.') {
        Some(dot) => file[..dot].to_string(),
        None => file.to_string(),
    }
}

/// A consent-store entry of a store app, as its name: "Microsoft.WindowsCamera_8wekyb3d8bbwe" -> "WindowsCamera".
#[cfg(any(windows, test))]
fn friendly_package(key: &str) -> String {
    let name = key.split('_').next().unwrap_or("");
    match name.rfind('.') {
        Some(dot) => name[dot + 1..].to_string(),
        None => name.to_string(),
    }
}

#[cfg(windows)]
#[path = "privacy_monitor_windows.rs"]
mod windows_read;

/// Linux: running ALSA capture streams and processes with a video device open.
#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use std::path::{Path, PathBuf};

    use super::ordinal_distinct;

    pub fn mic() -> Vec<String> {
        mic_in(Path::new("/proc/asound"), Path::new("/proc"))
    }

    pub fn camera() -> Vec<String> {
        camera_in(Path::new("/dev"), Path::new("/proc"))
    }

    fn statuses(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // the card aliases are links to the card folders, which are walked anyway
            if kind.is_dir() {
                if depth < 8 {
                    statuses(&entry.path(), depth + 1, found);
                }
            } else if entry.file_name() == "status" {
                found.push(entry.path());
            }
        }
    }

    /// Running ALSA capture substreams (PipeWire / PulseAudio open them while something records), by their owner.
    pub(super) fn mic_in(asound: &Path, proc_root: &Path) -> Vec<String> {
        let mut apps = Vec::new();
        if !asound.is_dir() {
            return apps;
        }
        let mut found = Vec::new();
        statuses(asound, 0, &mut found);
        for status in found {
            // pcmNc/subM/status: a capture stream
            if !status.to_string_lossy().contains("c/sub") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&status) else {
                continue;
            };
            if !text.contains("state: RUNNING") {
                continue;
            }
            let pid = text
                .split('\n')
                .find(|l| l.starts_with("owner_pid"))
                .and_then(|l| l.split(':').next_back())
                .map(str::trim);
            let comm = pid
                .and_then(|pid| std::fs::read_to_string(proc_root.join(pid).join("comm")).ok())
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            apps.push(comm.unwrap_or_else(|| "microphone".to_string()));
        }
        ordinal_distinct(apps)
    }

    /// Processes (of this user) with a video device open.
    pub(super) fn camera_in(dev: &Path, proc_root: &Path) -> Vec<String> {
        let any_video = std::fs::read_dir(dev).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("video")
                    && e.file_type().is_ok_and(|k| !k.is_dir())
            })
        });
        let mut apps = Vec::new();
        if !any_video {
            return apps;
        }
        let Ok(processes) = std::fs::read_dir(proc_root) else {
            return apps;
        };
        for process in processes.flatten() {
            let pid = process.file_name().to_string_lossy().into_owned();
            if !pid.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            // another user's process: not readable
            let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            let open = fds.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .is_ok_and(|target| target.to_string_lossy().starts_with("/dev/video"))
            });
            if open {
                let comm = std::fs::read_to_string(process.path().join("comm"))
                    .map(|c| c.trim().to_string())
                    .unwrap_or(pid);
                apps.push(comm);
            }
        }
        ordinal_distinct(apps)
    }
}

/// The session's own name, or the process name; "Windows" for the system's own sessions.
pub fn session_name(pid: u32, display_name: Option<&str>) -> String {
    if let Some(name) = display_name
        && !name.trim().is_empty()
        && !name.starts_with('@')
    {
        return name.trim().to_string();
    }
    if pid == 0 {
        return "Windows".to_string();
    }
    process_name(pid).unwrap_or_else(|| "an app".to_string())
}

/// Each app once (the first spelling, ignoring case), without blank names.
pub fn distinct(apps: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    apps.into_iter()
        .filter(|a| !a.trim().is_empty() && seen.insert(a.to_uppercase()))
        .collect()
}

/// The name of a running process (without ".exe" on Windows), or None.
pub fn process_name(pid: u32) -> Option<String> {
    crate::process_monitor::list_processes(Some(&[pid]))
        .into_iter()
        .find(|p| p.pid == pid)
        .map(|p| p.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naming the apps behind microphone recordings (#282).
    #[test]
    fn a_session_is_named_after_its_own_name_then_its_process() {
        assert_eq!(session_name(1234, Some("  Teams call ")), "Teams call");
        let me = std::process::id();
        let name = process_name(me).expect("this test's own process");
        assert!(!name.is_empty());
        // An empty name, or a resource reference ("@%SystemRoot%\..."), falls back to the process.
        assert_eq!(session_name(me, None), name);
        assert_eq!(
            session_name(me, Some(r"@%SystemRoot%\System32\AudioSrv.Dll,-202")),
            name
        );
        assert_eq!(session_name(0, Some("")), "Windows");
    }

    #[test]
    fn a_session_of_a_process_that_has_ended_still_counts_as_an_app() {
        // A process id that can't exist: the recording is still reported, just without a name.
        assert_eq!(session_name(i32::MAX as u32, None), "an app");
        assert_eq!(process_name(i32::MAX as u32), None);
    }

    #[test]
    fn an_app_with_several_sessions_is_listed_once() {
        let apps = ["Zoom", "zoom", " ", "obs64", "ZOOM"].map(String::from);
        assert_eq!(distinct(apps), ["Zoom", "obs64"]);
    }

    #[test]
    fn consent_store_entries_are_named_after_the_program_or_the_package() {
        assert_eq!(friendly_exe("C:#Program Files#Zoom#bin#Zoom.exe"), "Zoom");
        assert_eq!(friendly_exe("C:#Tools#obs64.exe"), "obs64");
        assert_eq!(
            friendly_package("Microsoft.WindowsCamera_8wekyb3d8bbwe"),
            "WindowsCamera"
        );
        assert_eq!(friendly_package("Zoom_abc"), "Zoom");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_names_running_capture_streams_and_processes_with_a_camera_open() {
        let root = tempfile::tempdir().unwrap();
        let (asound, proc_root, dev) = (
            root.path().join("asound"),
            root.path().join("proc"),
            root.path().join("dev"),
        );
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        // a running capture stream owned by pid 42, a stopped one, and a running playback stream
        write(
            asound.join("card0/pcm0c/sub0/status"),
            "state: RUNNING\nowner_pid   : 42\n",
        );
        write(asound.join("card0/pcm1c/sub0/status"), "closed\n");
        write(
            asound.join("card0/pcm0p/sub0/status"),
            "state: RUNNING\nowner_pid   : 43\n",
        );
        // an owner whose name can't be read
        write(
            asound.join("card1/pcm0c/sub0/status"),
            "state: RUNNING\nowner_pid   : 99\n",
        );
        write(proc_root.join("42/comm"), "pipewire\n");
        write(proc_root.join("43/comm"), "player\n");
        // the folders are read in the file system's order
        let mut mic = linux::mic_in(&asound, &proc_root);
        mic.sort();
        assert_eq!(mic, ["microphone", "pipewire"]);
        assert!(linux::mic_in(&root.path().join("none"), &proc_root).is_empty());

        // no video device: nothing is looked at
        std::fs::create_dir_all(&dev).unwrap();
        assert!(linux::camera_in(&dev, &proc_root).is_empty());
        write(dev.join("video0"), "");
        std::fs::create_dir_all(proc_root.join("42/fd")).unwrap();
        std::os::unix::fs::symlink("/dev/video0", proc_root.join("42/fd/7")).unwrap();
        std::fs::create_dir_all(proc_root.join("43/fd")).unwrap();
        std::os::unix::fs::symlink("/dev/null", proc_root.join("43/fd/0")).unwrap();
        std::fs::create_dir_all(proc_root.join("44/fd")).unwrap();
        std::os::unix::fs::symlink("/dev/video1", proc_root.join("44/fd/3")).unwrap();
        std::fs::create_dir_all(proc_root.join("self/fd")).unwrap();
        std::os::unix::fs::symlink("/dev/video0", proc_root.join("self/fd/3")).unwrap();
        // pid 44 has no readable name: its pid stands in
        assert_eq!(
            {
                let mut apps = linux::camera_in(&dev, &proc_root);
                apps.sort();
                apps
            },
            ["44", "pipewire"]
        );
    }

    #[test]
    fn the_state_shows_null_until_the_first_read_then_the_phone_shape() {
        let monitor = PrivacyMonitor::new();
        let state = PrivacyState(monitor.clone());
        assert_eq!(crate::state::StateSource::snapshot(&state), Value::Null);
        *lock(&monitor.snap) = Some(PrivacySnapshot {
            mic: Some(true),
            camera: Some(false),
            mic_apps: vec!["Zoom".into()],
            camera_apps: vec![],
        });
        assert_eq!(
            serde_json::to_string(&crate::state::StateSource::snapshot(&state)).unwrap(),
            r#"{"mic":true,"camera":false,"micApps":["Zoom"],"cameraApps":[]}"#
        );
    }
}
