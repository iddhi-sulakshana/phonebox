//! Port of `Shortcuts.cs`: runs the shortcuts defined in config.json (the phone only sends an id, never a command). A
//! shortcut starts a program, presses keys, types text or runs a macro; toggles remember or read their on/off state.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::audio_control::AudioControl;
use crate::config::{AgentConfig, Shortcut};
use crate::key_sender;
use crate::push_store::PushStore;

/// What the phone sees of a failed shortcut (an error can carry configured command text).
pub const GENERIC_FAILURE: &str =
    "The shortcut failed. The agent log on the computer has the details.";

/// The remembered state of toggles without a state key.
static REMEMBERED: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

fn remembered(id: &str) -> bool {
    let r = REMEMBERED.lock().unwrap_or_else(|e| e.into_inner());
    r.as_ref().and_then(|m| m.get(id).copied()).unwrap_or(false)
}

fn remember(id: &str, on: bool) {
    let mut r = REMEMBERED.lock().unwrap_or_else(|e| e.into_inner());
    r.get_or_insert_with(HashMap::new)
        .insert(id.to_string(), on);
}

/// Runs the shortcut with this id; an error message, or None on success.
pub fn run(
    cfg: &AgentConfig,
    id: &str,
    push: Option<&PushStore>,
    audio: Option<&AudioControl>,
) -> Option<String> {
    match cfg.shortcuts.iter().find(|s| s.id == id) {
        None => Some(format!("no such shortcut: {id}")),
        Some(sc) => run_core(Some(cfg), sc, push, audio, 0),
    }
}

fn run_core(
    cfg: Option<&AgentConfig>,
    sc: &Shortcut,
    push: Option<&PushStore>,
    audio: Option<&AudioControl>,
    depth: u32,
) -> Option<String> {
    if !sc.toggle {
        return execute(sc, cfg, depth, push, audio);
    }
    let on = state(sc, push, audio).unwrap_or(false);
    let off;
    let target = match &sc.off_cmd {
        Some(cmd) if on => {
            off = Shortcut {
                label: sc.label.clone(),
                cmd: cmd.clone(),
                args: sc.off_args.clone(),
                cwd: sc.cwd.clone(),
                ..Shortcut::default()
            };
            &off
        }
        _ => sc,
    };
    let err = execute(target, cfg, depth, push, audio);
    if err.is_none() && sc.state_key.is_none() && sc.cmd != "builtin:mute" {
        remember(&sc.id, !on);
    }
    err
}

/// The on/off state of the toggle keys, for the phone (ids as written).
pub fn states(
    cfg: &AgentConfig,
    push: &PushStore,
    audio: &AudioControl,
) -> Option<Map<String, Value>> {
    let mut map: Option<Map<String, Value>> = None;
    for sc in cfg
        .shortcuts
        .iter()
        .filter(|s| s.toggle || s.cmd == "builtin:mute")
    {
        if let Some(on) = state(sc, Some(push), Some(audio)) {
            map.get_or_insert_with(Map::new)
                .insert(sc.id.clone(), Value::Bool(on));
        }
    }
    map
}

pub fn state(
    sc: &Shortcut,
    push: Option<&PushStore>,
    audio: Option<&AudioControl>,
) -> Option<bool> {
    if sc.cmd == "builtin:mute" && sc.state_key.is_none() {
        return audio?.snapshot().map(|a| a.muted);
    }
    if let Some(key) = &sc.state_key {
        let Some(item) = push.and_then(|p| p.snapshot().get(key).cloned()) else {
            return Some(false);
        };
        let d = match &item.data {
            Value::Object(o) if o.contains_key("value") => o["value"].clone(),
            other => other.clone(),
        };
        return Some(match d {
            Value::Bool(b) => b,
            Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
            Value::String(t) => {
                !t.is_empty() && !matches!(t.to_lowercase().as_str(), "0" | "false" | "off" | "no")
            }
            _ => false,
        });
    }
    Some(remembered(&sc.id))
}

/// Runs one shortcut; an error message, or None on success. Logs the label (never the command).
pub fn execute(
    sc: &Shortcut,
    cfg: Option<&AgentConfig>,
    depth: u32,
    push: Option<&PushStore>,
    audio: Option<&AudioControl>,
) -> Option<String> {
    match execute_inner(sc, cfg, depth, push, audio) {
        Ok(()) => {
            crate::log::info("Program", &format!("shortcut: {}", sc.label));
            None
        }
        Err(e) => {
            crate::log::warn("Program", &format!("shortcut {} failed: {e}", sc.label));
            Some(e)
        }
    }
}

fn execute_inner(
    sc: &Shortcut,
    cfg: Option<&AgentConfig>,
    depth: u32,
    push: Option<&PushStore>,
    audio: Option<&AudioControl>,
) -> Result<(), String> {
    let args = sc.args.as_deref().unwrap_or("");
    match sc.cmd.as_str() {
        "builtin:keys" => key_sender::send_keys(args),
        "builtin:text" => key_sender::type_text(args),
        "builtin:macro" => {
            if depth > 3 {
                return Err("macros call each other too deeply".into());
            }
            for step in sc.steps.iter().flatten() {
                let (kind, value) = match step.find(':') {
                    Some(i) if i > 0 => (
                        step[..i].trim().to_lowercase(),
                        step[i + 1..].trim().to_string(),
                    ),
                    _ => ("keys".to_string(), step.trim().to_string()),
                };
                match kind.as_str() {
                    "keys" => key_sender::send_keys(&value)?,
                    "text" => key_sender::type_text(&value)?,
                    "wait" => {
                        let ms =
                            crate::server::query::int32(&[("v".to_string(), value.clone())], "v")
                                .ok()
                                .flatten()
                                .unwrap_or(0)
                                .clamp(0, 10_000);
                        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    }
                    "shortcut" => {
                        let other = cfg
                            .and_then(|c| c.shortcuts.iter().find(|s| s.id == value))
                            .ok_or_else(|| format!("no such shortcut: {value}"))?;
                        if let Some(e) = run_core(cfg, other, push, audio, depth + 1) {
                            return Err(e);
                        }
                    }
                    _ => {
                        return Err(format!(
                            "unknown macro step \"{step}\" (use keys:, text:, wait: or shortcut:)"
                        ));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok(())
        }
        "builtin:lock" => builtin::lock(),
        "builtin:monitor-off" => builtin::monitor_off(),
        "builtin:mute" => builtin::mute(audio),
        _ => start(sc),
    }
}

/// `RunShortcutForPhone`: Ok, or the detail the phone may see (`hint_or_generic`).
pub fn run_for_phone(
    cfg: &AgentConfig,
    id: &str,
    push: &PushStore,
    audio: &AudioControl,
) -> Result<(), String> {
    match run(cfg, id, Some(push), Some(audio)) {
        Some(e) => Err(hint_or_generic(&e)),
        None => Ok(()),
    }
}

/// What the phone may see of a failed shortcut: only the agent's own fixed hint goes through as it is.
pub fn hint_or_generic(error: &str) -> String {
    if error.contains(key_sender::ACCESSIBILITY_HINT) {
        key_sender::ACCESSIBILITY_HINT.to_string()
    } else {
        GENERIC_FAILURE.to_string()
    }
}

/// What to start instead when a default shortcut's program is missing (Windows Terminal on Windows 10,
/// x-terminal-emulator outside Debian, GNOME's apps on other desktops). Empty for other commands.
pub fn terminal_stand_ins(cmd: &str, terminal_env: Option<&str>) -> Vec<String> {
    let list: &[&str] = match cmd.trim().to_lowercase().as_str() {
        "wt.exe" | "wt" => &["powershell.exe", "cmd.exe"],
        "x-terminal-emulator" => {
            let mut v: Vec<String> = terminal_env
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| vec![t.to_string()])
                .unwrap_or_default();
            v.extend(
                [
                    "kgx",
                    "gnome-terminal",
                    "konsole",
                    "xfce4-terminal",
                    "mate-terminal",
                    "lxterminal",
                    "tilix",
                    "alacritty",
                    "kitty",
                    "foot",
                    "wezterm",
                    "xterm",
                ]
                .map(String::from),
            );
            return v;
        }
        "gnome-calculator" => &[
            "kcalc",
            "mate-calc",
            "galculator",
            "qalculate-gtk",
            "xfce4-calculator",
        ],
        "gnome-control-center" => &[
            "systemsettings",
            "systemsettings5",
            "xfce4-settings-manager",
            "mate-control-center",
            "cinnamon-settings",
            "budgie-control-center",
            "lxqt-config",
        ],
        "gnome-system-monitor" => &[
            "plasma-systemmonitor",
            "ksysguard",
            "xfce4-taskmanager",
            "mate-system-monitor",
            "lxtask",
        ],
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// "~" at the start of a path means the home folder (Linux / macOS style), and " ~/" inside the arguments too.
pub fn expand_home(s: &str) -> String {
    let home = crate::app_paths::home().to_string_lossy().into_owned();
    if s == "~" || s.starts_with("~/") {
        format!("{home}{}", &s[1..])
    } else {
        s.replace(" ~/", &format!(" {home}/"))
    }
}

/// .NET's `ParseArgumentsIntoList` (how `Process.Start` splits an argument string on Linux and macOS).
pub fn split_arguments(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut results = vec![];
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        if i == chars.len() {
            break;
        }
        let mut current = String::new();
        let mut in_quotes = false;
        while i < chars.len() {
            let mut backslashes = 0;
            while i < chars.len() && chars[i] == '\\' {
                i += 1;
                backslashes += 1;
            }
            if backslashes > 0 {
                if i >= chars.len() || chars[i] != '"' {
                    current.extend(std::iter::repeat_n('\\', backslashes));
                } else {
                    current.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 != 0 {
                        current.push('"');
                        i += 1;
                    }
                }
                continue;
            }
            let c = chars[i];
            if c == '"' {
                if in_quotes && i + 1 < chars.len() && chars[i + 1] == '"' {
                    current.push('"');
                    i += 1;
                } else {
                    in_quotes = !in_quotes;
                }
                i += 1;
                continue;
            }
            if (c == ' ' || c == '\t') && !in_quotes {
                break;
            }
            current.push(c);
            i += 1;
        }
        results.push(current);
    }
    results
}

/// A stand-in is a fixed program at a full path: started directly, without the shell and without arguments.
fn start_stand_in(path: &std::path::Path, sc: &Shortcut) -> Result<(), String> {
    let mut cmd = std::process::Command::new(path);
    if let Some(cwd) = sc.cwd.as_deref().filter(|c| !c.is_empty()) {
        cmd.current_dir(expand_home(cwd));
    }
    let child = cmd
        .spawn()
        .map_err(|e| start_error(&path.to_string_lossy(), sc, &e))?;
    reap(child);
    Ok(())
}

/// Waits for a started program on a thread of its own, so it does not linger as a zombie when it ends.
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn start_error(program: &str, sc: &Shortcut, e: &std::io::Error) -> String {
    let cwd = sc
        .cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(expand_home)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let text = e.to_string();
    let text = text
        .find(" (os error ")
        .map_or(text.as_str(), |i| &text[..i])
        .to_string();
    format!(
        "An error occurred trying to start process '{program}' with working directory '{cwd}'. {text}"
    )
}

/// Linux and macOS: .NET's shell-execute: an executable file runs with its arguments; anything else is opened with
/// `xdg-open` / `open`, without the arguments. A missing default program is replaced by a stand-in first.
#[cfg(unix)]
fn start(sc: &Shortcut) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let stand_ins = terminal_stand_ins(&sc.cmd, std::env::var("TERMINAL").ok().as_deref());
    if !stand_ins.is_empty()
        && crate::unix::sh::which(&sc.cmd).is_none()
        && let Some(alt) = stand_ins.iter().find_map(|t| crate::unix::sh::which(t))
    {
        return start_stand_in(&alt, sc);
    }
    let file = expand_home(&sc.cmd);
    let args = expand_home(sc.args.as_deref().unwrap_or(""));
    let cwd = sc.cwd.as_deref().filter(|c| !c.is_empty()).map(expand_home);
    let base = cwd
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    // a URI (mailto:, https://) is never executed
    let is_uri = regex::Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:")
        .expect("regex")
        .is_match(&file)
        && !file.starts_with("file:");
    let executable = (!is_uri)
        .then(|| {
            let path = std::path::Path::new(&file);
            if path.is_absolute() {
                Some(path.to_path_buf())
            } else if file.contains('/') {
                Some(base.join(path))
            } else {
                crate::unix::sh::which(&file)
            }
        })
        .flatten()
        .filter(|p| {
            p.is_file() && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        });
    let mut command = match &executable {
        Some(path) => {
            let mut c = std::process::Command::new(path);
            c.args(split_arguments(&args));
            c
        }
        None => opener(&file)?,
    };
    if let Some(dir) = &cwd {
        command.current_dir(dir);
    }
    match command.spawn() {
        Ok(child) => {
            reap(child);
            Ok(())
        }
        // a file with the execute bit that is not a program (a document on a FAT disk): open it instead
        Err(e) if executable.is_some() && e.raw_os_error() == Some(libc::ENOEXEC) => {
            let mut c = opener(&file)?;
            if let Some(dir) = &cwd {
                c.current_dir(dir);
            }
            reap(c.spawn().map_err(|e| start_error(&file, sc, &e))?);
            Ok(())
        }
        Err(e) => Err(start_error(&file, sc, &e)),
    }
}

/// The default handler: `open` on macOS, `xdg-open` on Linux, with the file or address as its only argument.
#[cfg(unix)]
fn opener(file: &str) -> Result<std::process::Command, String> {
    let tool = if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/usr/bin/open")
    } else {
        crate::unix::sh::which("xdg-open").ok_or_else(|| {
            format!("An error occurred trying to start process '{file}'. No such file or directory")
        })?
    };
    let mut c = std::process::Command::new(tool);
    c.arg(file);
    Ok(c)
}

/// Windows: ShellExecuteEx, as `UseShellExecute = true`; a missing Windows Terminal falls back to PowerShell / cmd.
#[cfg(windows)]
fn start(sc: &Shortcut) -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, GetLastError};
    use windows::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    let file = wide(&expand_home(&sc.cmd));
    let args = wide(&expand_home(sc.args.as_deref().unwrap_or("")));
    let cwd = sc
        .cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| wide(&expand_home(c)));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        lpDirectory: cwd.as_ref().map_or(PCWSTR::null(), |c| PCWSTR(c.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: every string is NUL-terminated and outlives the call; the process handle is closed once
    unsafe {
        if ShellExecuteExW(&mut info).is_ok() {
            if !info.hProcess.is_invalid() {
                let _ = CloseHandle(info.hProcess);
            }
            return Ok(());
        }
        let error = GetLastError();
        if error.0 == 2 {
            let system = std::env::var("SystemRoot")
                .map(|r| format!("{r}\\System32"))
                .unwrap_or_default();
            let alt = terminal_stand_ins(&sc.cmd, None)
                .into_iter()
                .map(|t| {
                    let rel = if t == "powershell.exe" {
                        "WindowsPowerShell\\v1.0\\powershell.exe".to_string()
                    } else {
                        t
                    };
                    std::path::Path::new(&system).join(rel)
                })
                .find(|p| p.is_file());
            if let Some(alt) = alt {
                return start_stand_in(&alt, sc);
            }
        }
        Err(start_error(
            &sc.cmd,
            sc,
            &std::io::Error::from_raw_os_error(error.0 as i32),
        ))
    }
}

#[cfg(windows)]
mod builtin {
    use crate::audio_control::AudioControl;

    pub fn lock() -> Result<(), String> {
        // SAFETY: a plain call
        let _ = unsafe { windows::Win32::System::Shutdown::LockWorkStation() };
        Ok(())
    }

    /// HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2 = off.
    pub fn monitor_off() -> Result<(), String> {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        // SAFETY: a posted broadcast message with plain values
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(0xFFFF as *mut core::ffi::c_void)),
                0x0112,
                WPARAM(0xF170),
                LPARAM(2),
            )
        };
        Ok(())
    }

    /// The keyboard's mute key (VK_VOLUME_MUTE).
    pub fn mute(_audio: Option<&AudioControl>) -> Result<(), String> {
        crate::key_sender::tap(0xAD);
        Ok(())
    }
}

#[cfg(not(windows))]
mod builtin {
    use crate::audio_control::AudioControl;
    use crate::unix::sh;

    /// Runs a tool; a failure is "not installed" only when the tool is missing (as the C# code checks).
    fn unix(tool: &str, args: &[&str]) -> Result<(), String> {
        if sh::run(tool, args).is_none() && sh::which(tool).is_none() {
            return Err(format!("{tool} is not installed"));
        }
        Ok(())
    }

    pub fn lock() -> Result<(), String> {
        if cfg!(target_os = "macos") {
            unix("pmset", &["displaysleepnow"])
        } else if sh::run("loginctl", &["lock-session"]).is_none() {
            unix("xdg-screensaver", &["lock"])
        } else {
            Ok(())
        }
    }

    pub fn monitor_off() -> Result<(), String> {
        if cfg!(target_os = "macos") {
            unix("pmset", &["displaysleepnow"])
        } else {
            unix("xset", &["dpms", "force", "off"])
        }
    }

    /// False: the output cannot be muted (macOS) or wpctl / pactl failed; a caller must not count it as done.
    pub fn mute(audio: Option<&AudioControl>) -> Result<(), String> {
        let own;
        let audio = match audio {
            Some(a) => a,
            None => {
                own = AudioControl::new();
                &own
            }
        };
        if audio.toggle_mute() {
            Ok(())
        } else {
            Err("could not toggle mute on the current output".into())
        }
    }
}

/// `toggles` in the state.
pub struct Toggles {
    pub cfg: std::sync::Arc<crate::config::store::ConfigStore>,
    pub push: std::sync::Arc<PushStore>,
    pub audio: std::sync::Arc<AudioControl>,
}

impl crate::state::StateSource for Toggles {
    fn snapshot(&self) -> Value {
        states(&self.cfg.get(), &self.push, &self.audio).map_or(Value::Null, Value::Object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn shortcut(id: &str, cmd: &str) -> Shortcut {
        Shortcut {
            id: id.into(),
            label: id.into(),
            cmd: cmd.into(),
            ..Shortcut::default()
        }
    }
    fn unique(prefix: &str) -> String {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }
    fn push() -> (tempfile::TempDir, Arc<PushStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = PushStore::new(&dir.path().join("data.json"));
        (dir, store)
    }

    #[test]
    fn windows_terminal_falls_back_to_power_shell() {
        for cmd in ["wt.exe", "WT"] {
            assert_eq!(terminal_stand_ins(cmd, None), ["powershell.exe", "cmd.exe"]);
        }
    }

    #[test]
    fn linux_tries_the_terminal_variable_first() {
        let alts = terminal_stand_ins("x-terminal-emulator", Some(" alacritty "));
        assert_eq!(alts[0], "alacritty");
        assert!(alts.iter().any(|a| a == "konsole"));
        assert_eq!(alts.last().unwrap(), "xterm");
    }

    #[test]
    fn linux_without_the_variable_starts_with_the_desktop_terminals() {
        assert_eq!(terminal_stand_ins("x-terminal-emulator", None)[0], "kgx");
        assert_eq!(
            terminal_stand_ins("x-terminal-emulator", Some("  "))[0],
            "kgx"
        );
    }

    #[test]
    fn other_commands_are_left_alone() {
        for cmd in ["notepad.exe", "code", "C:\\Tools\\wt.exe"] {
            assert!(terminal_stand_ins(cmd, None).is_empty(), "{cmd}");
        }
    }

    #[test]
    fn the_gnome_defaults_fall_back_to_kde_first() {
        for (cmd, first) in [
            ("gnome-calculator", "kcalc"),
            ("gnome-control-center", "systemsettings"),
            ("gnome-system-monitor", "plasma-systemmonitor"),
        ] {
            assert_eq!(terminal_stand_ins(cmd, None)[0], first);
        }
    }

    #[test]
    fn every_stand_in_is_a_plain_program_name() {
        let plain = regex::Regex::new("^[a-z0-9][a-z0-9-]*$").unwrap();
        for cmd in [
            "gnome-calculator",
            "gnome-control-center",
            "gnome-system-monitor",
        ] {
            let alts = terminal_stand_ins(cmd, None);
            assert!(!alts.is_empty());
            for alt in alts {
                assert!(plain.is_match(&alt), "{alt}");
            }
        }
    }

    #[test]
    fn other_gnome_names_are_left_alone() {
        for cmd in ["/usr/bin/gnome-calculator", "gnome-terminal", "calc"] {
            assert!(terminal_stand_ins(cmd, None).is_empty(), "{cmd}");
        }
    }

    #[test]
    fn macro_shortcut_steps_update_toggle_state_and_run_off_cmd() {
        let lamp_id = unique("macro-lamp");
        let mut lamp = shortcut(&lamp_id, "builtin:macro");
        lamp.toggle = true;
        lamp.off_cmd = Some("builtin:keys".into());
        lamp.off_args = Some("bad_off_key".into());
        let mut scene = shortcut("scene", "builtin:macro");
        scene.steps = Some(vec![format!("shortcut:{lamp_id}")]);
        let cfg = AgentConfig {
            shortcuts: vec![lamp.clone(), scene],
            ..AgentConfig::default()
        };
        assert_eq!(run(&cfg, "scene", None, None), None);
        assert_eq!(state(&lamp, None, None), Some(true));
        let second = run(&cfg, "scene", None, None).expect("the off command fails");
        assert!(second.contains("bad_off_key"), "{second}");
    }

    // Review Focus 1: an error can carry configured command text; only the agent's own hint reaches the phone
    #[test]
    fn the_phone_does_not_get_the_configured_text_of_a_failed_shortcut() {
        let lamp_id = unique("phone-error-lamp");
        let mut lamp = shortcut(&lamp_id, "builtin:macro");
        lamp.toggle = true;
        lamp.off_cmd = Some("builtin:keys".into());
        lamp.off_args = Some("secret_off_key".into());
        let mut scene = shortcut(&format!("scene-{lamp_id}"), "builtin:macro");
        scene.steps = Some(vec![format!("shortcut:{lamp_id}")]);
        let cfg = AgentConfig {
            shortcuts: vec![lamp, scene.clone()],
            ..AgentConfig::default()
        };
        let audio = AudioControl::with(Arc::new(|| None), None, None);
        let (_d, push) = push();
        assert_eq!(run_for_phone(&cfg, &scene.id, &push, &audio), Ok(())); // switches the lamp on
        let failed = run_for_phone(&cfg, &scene.id, &push, &audio).unwrap_err();
        assert!(!failed.contains("secret_off_key"), "{failed}");
        assert_eq!(failed, GENERIC_FAILURE);
        assert_eq!(
            run_for_phone(&cfg, "missing", &push, &audio).unwrap_err(),
            GENERIC_FAILURE
        );
    }

    #[test]
    fn the_phone_still_gets_the_agents_own_accessibility_hint() {
        assert_eq!(
            hint_or_generic(crate::key_sender::ACCESSIBILITY_HINT),
            crate::key_sender::ACCESSIBILITY_HINT
        );
        let generic = hint_or_generic("keys failed: secret_off_key");
        assert!(!generic.contains("secret_off_key"));
        assert_eq!(generic, GENERIC_FAILURE);
    }

    #[test]
    fn toggle_states_come_from_pushed_values_mute_and_memory() {
        let (_d, push) = push();
        let audio = AudioControl::with(
            Arc::new(|| Some(crate::audio_control::AudioSnapshot::new(30, true, true))),
            None,
            None,
        );
        audio.current();
        let mut pushed = shortcut("Lamp_A", "builtin:keys");
        pushed.toggle = true;
        pushed.state_key = Some("mic".into());
        let mute = shortcut("mute-key", "builtin:mute");
        let plain = shortcut("plain", "builtin:keys");
        let cfg = AgentConfig {
            shortcuts: vec![pushed.clone(), mute, plain],
            ..AgentConfig::default()
        };
        assert_eq!(
            state(&pushed, Some(&push), None),
            Some(false),
            "nothing pushed yet"
        );
        for (value, on) in [
            (serde_json::json!({"value": "on"}), true),
            (serde_json::json!("OFF"), false),
            (serde_json::json!("No"), false),
            (serde_json::json!(""), false),
            (serde_json::json!(2), true),
            (serde_json::json!(0), false),
            (serde_json::json!(true), true),
            (serde_json::json!({"other": 1}), false),
            (serde_json::json!({"value": 0.5}), true),
        ] {
            push.set("mic", value.clone(), None).unwrap();
            assert_eq!(state(&pushed, Some(&push), None), Some(on), "{value}");
        }
        push.set("mic", serde_json::json!("yes"), None).unwrap();
        let map = states(&cfg, &push, &audio).unwrap();
        assert_eq!(
            serde_json::Value::Object(map),
            serde_json::json!({"Lamp_A": true, "mute-key": true})
        );
        let quiet = AgentConfig {
            shortcuts: vec![shortcut("plain", "builtin:keys")],
            ..AgentConfig::default()
        };
        assert_eq!(states(&quiet, &push, &audio), None);
    }

    #[test]
    fn macro_steps_follow_the_csharp_rules() {
        let mut m = shortcut("m", "builtin:macro");
        m.steps = Some(vec!["wait: 5".into(), "Shortcut: missing".into()]);
        let cfg = AgentConfig {
            shortcuts: vec![m.clone()],
            ..AgentConfig::default()
        };
        assert_eq!(
            execute(&m, Some(&cfg), 0, None, None).as_deref(),
            Some("no such shortcut: missing")
        );
        m.steps = Some(vec!["play: x".into()]);
        assert_eq!(
            execute(&m, Some(&cfg), 0, None, None).as_deref(),
            Some("unknown macro step \"play: x\" (use keys:, text:, wait: or shortcut:)")
        );
        m.steps = Some(vec!["ctrl+banana".into()]);
        assert_eq!(
            execute(&m, Some(&cfg), 0, None, None).as_deref(),
            Some("unknown key \"banana\"")
        );
        m.steps = None;
        assert_eq!(
            execute(&m, Some(&cfg), 4, None, None).as_deref(),
            Some("macros call each other too deeply")
        );
        assert_eq!(execute(&m, Some(&cfg), 3, None, None), None);
        assert_eq!(
            run(&cfg, "nope", None, None).as_deref(),
            Some("no such shortcut: nope")
        );
    }

    #[test]
    fn home_expands_a_leading_tilde_and_one_inside_arguments() {
        let home = crate::app_paths::home().to_string_lossy().into_owned();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/bin/x"), format!("{home}/bin/x"));
        assert_eq!(expand_home("--dir ~/work"), format!("--dir {home}/work"));
        assert_eq!(expand_home("a~/b"), "a~/b");
    }

    #[test]
    fn arguments_split_as_dotnet_splits_them() {
        assert_eq!(
            split_arguments(r#"a "b c" d\"e "f""g" h\\"#),
            ["a", "b c", "d\"e", "f\"g", "h\\\\"]
        );
        assert_eq!(split_arguments("  x\t y  "), ["x", "y"]);
        assert_eq!(split_arguments(r#"\\"a b""#), [r"\a b"]);
        assert!(split_arguments("").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_program_starts_with_its_arguments_and_working_folder() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        let mut sc = shortcut("touch", "sh");
        sc.args = Some(format!("-c \"pwd > '{}'\"", out.display()));
        sc.cwd = Some(dir.path().to_string_lossy().into_owned());
        assert_eq!(execute(&sc, None, 0, None, None), None);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !out.exists() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            std::fs::canonicalize(written.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }
}
