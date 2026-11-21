//! Port of `Unix/UnixProc.cs`: process details on Linux (/proc) and macOS (ps): executable, arguments, owner, a
//! friendly name and an icon.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::sh;

/// Processes whose end would break the session, and the agent's own tools (compared case-sensitively, as on Unix).
pub const PROTECTED: [&str; 43] = [
    // Linux
    "systemd",
    "init",
    "kthreadd",
    "Xorg",
    "Xwayland",
    "gnome-shell",
    "gnome-session-binary",
    "plasmashell",
    "kwin_x11",
    "kwin_wayland",
    "mutter",
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "pulseaudio",
    "dbus-daemon",
    "dbus-broker",
    "sshd",
    "login",
    "gdm",
    "sddm",
    "lightdm",
    "NetworkManager",
    "polkitd",
    "udisksd",
    "upowerd",
    // macOS
    "kernel_task",
    "launchd",
    "WindowServer",
    "loginwindow",
    "Dock",
    "Finder",
    "SystemUIServer",
    "coreaudiod",
    "mds",
    "mds_stores",
    "cfprefsd",
    "distnoted",
    "ControlCenter",
    "NotificationCenter",
    // ours
    "PhoneBoxAgent",
    "adb",
    "dotnet",
];

struct PsRow {
    uid: u32,
    path: String,
    args: String,
}

/// macOS: owner, executable and arguments of every process from two `ps` calls, cached for two seconds (one `ps` per
/// process takes seconds with a few hundred processes).
fn with_ps<R>(f: impl FnOnce(&HashMap<u32, PsRow>) -> R) -> R {
    static PS: Mutex<Option<(Instant, HashMap<u32, PsRow>)>> = Mutex::new(None);
    let mut ps = PS.lock().unwrap_or_else(|e| e.into_inner());
    if !matches!(&*ps, Some((at, _)) if at.elapsed() < Duration::from_secs(2)) {
        let args =
            parse_ps_args(&sh::run("ps", &["-axww", "-o", "pid=,args="]).unwrap_or_default());
        let rows = parse_ps_owners(
            &sh::run("ps", &["-axww", "-o", "pid=,uid=,comm="]).unwrap_or_default(),
            &args,
        );
        *ps = Some((Instant::now(), rows));
    }
    match &*ps {
        Some((_, rows)) => f(rows),
        None => f(&HashMap::new()),
    }
}

/// `ps -o pid=,args=` lines: the pid, a space, the command line.
fn parse_ps_args(text: &str) -> HashMap<u32, String> {
    text.split('\n')
        .filter_map(|line| {
            let t = line.trim();
            let (pid, rest) = t.split_once(' ')?;
            Some((pid.parse().ok()?, rest.trim().to_string()))
        })
        .collect()
}

/// `ps -o pid=,uid=,comm=` lines (the executable may contain spaces), joined with the arguments by pid.
fn parse_ps_owners(text: &str, args: &HashMap<u32, String>) -> HashMap<u32, PsRow> {
    text.split('\n')
        .filter_map(|line| {
            let t = line.trim();
            let (pid, rest) = t.split_once(' ')?;
            let (uid, path) = rest.trim_start().split_once(' ')?;
            let pid: u32 = pid.parse().ok()?;
            let path = path.trim();
            if path.is_empty() {
                return None;
            }
            let row = PsRow {
                uid: uid.parse().ok()?,
                path: path.to_string(),
                args: args.get(&pid).cloned().unwrap_or_default(),
            };
            Some((pid, row))
        })
        .collect()
}

/// True if the process belongs to the user running the agent (only those may be ended).
pub fn is_own(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        return sh::read_file(Path::new(&format!("/proc/{pid}/status")))
            .and_then(|s| status_uid(&s))
            .is_some_and(|uid| uid == sh::uid());
    }
    with_ps(|ps| ps.get(&pid).is_some_and(|r| r.uid == sh::uid()))
}

/// The real uid from /proc/<pid>/status ("Uid:\treal\teffective…").
fn status_uid(status: &str) -> Option<u32> {
    let line = status.split('\n').find(|l| l.starts_with("Uid:"))?;
    line.split(['\t', ' '])
        .filter(|f| !f.is_empty())
        .nth(1)?
        .parse()
        .ok()
}

/// The executable: /proc/<pid>/exe on Linux, ps on macOS.
pub fn image_path(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
    }
    with_ps(|ps| {
        ps.get(&pid)
            .filter(|r| !r.path.is_empty())
            .map(|r| r.path.clone())
    })
}

/// The arguments after the executable, shortened to 140 characters.
pub fn args(pid: u32) -> Option<String> {
    let cmd = if cfg!(target_os = "linux") {
        sh::read_file(Path::new(&format!("/proc/{pid}/cmdline")))
            .map(|c| c.replace('\0', " ").trim().to_string())
    } else {
        with_ps(|ps| ps.get(&pid).map(|r| r.args.clone()))
    }?;
    let rest = match cmd.find(' ') {
        Some(i) if i > 0 => cmd[i + 1..].trim(),
        _ => "",
    };
    shorten(rest)
}

/// None when empty, the first 137 characters and "…" when longer than 140.
pub fn shorten(rest: &str) -> Option<String> {
    if rest.is_empty() {
        None
    } else if rest.chars().count() > 140 {
        Some(rest.chars().take(137).chain(['…']).collect())
    } else {
        Some(rest.to_string())
    }
}

/// "Visual Studio Code" for /Applications/Visual Studio Code.app/…, the Linux launcher's name, or the process name.
pub fn title(name: &str, path: Option<&str>) -> String {
    if let Some(path) = path
        && let Some(i) = path.find(".app/")
    {
        return file_name(&path[..i]).to_string();
    }
    if cfg!(target_os = "linux")
        && let Some((n, _)) = desktop().get(name)
    {
        return n.clone();
    }
    name.to_string()
}

/// `Path.GetFileName`: what follows the last separator.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

type DesktopMap = HashMap<String, (String, Option<String>)>;

/// Linux: executables mapped to the Name and Icon of their .desktop launcher, read once.
fn desktop() -> &'static DesktopMap {
    static DESKTOP: OnceLock<DesktopMap> = OnceLock::new();
    DESKTOP.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        let dirs = [
            "/usr/share/applications".to_string(),
            "/usr/local/share/applications".to_string(),
            "/var/lib/snapd/desktop/applications".to_string(),
            "/var/lib/flatpak/exports/share/applications".to_string(),
            format!("{home}/.local/share/applications"),
        ];
        let mut map = DesktopMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                if let Some(text) = std::fs::read(&path)
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    && let Some((bin, name, icon)) = parse_desktop(&text)
                {
                    map.entry(bin).or_insert((name, icon));
                }
            }
        }
        map
    })
}

/// A launcher's (executable, Name, Icon): the first Name=, Exec= and Icon= of the file, stopping at the next section
/// once a name was seen. The executable is the file name of Exec's first word, quotes trimmed as the C# code does.
fn parse_desktop(text: &str) -> Option<(String, String, Option<String>)> {
    let (mut name, mut exec, mut icon) = (None, None, None);
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.starts_with('[') && line != "[Desktop Entry]" && name.is_some() {
            break;
        }
        if name.is_none()
            && let Some(v) = line.strip_prefix("Name=")
        {
            name = Some(v.to_string());
        } else if exec.is_none()
            && let Some(v) = line.strip_prefix("Exec=")
        {
            exec = Some(v.to_string());
        } else if icon.is_none()
            && let Some(v) = line.strip_prefix("Icon=")
        {
            icon = Some(v.to_string());
        }
    }
    let exec = exec?;
    let first = exec
        .trim_matches('"')
        .split(' ')
        .next()
        .unwrap_or_default()
        .trim_matches('"');
    Some((file_name(first).to_string(), name?, icon))
}

/// A PNG icon for the process, if one can be found (Linux icon themes, macOS app bundles).
pub fn icon(name: &str, path: Option<&str>) -> Option<Vec<u8>> {
    if cfg!(target_os = "linux") {
        let icon = desktop()
            .get(name)
            .and_then(|(_, i)| i.clone())
            .unwrap_or_else(|| name.to_string());
        if icon.starts_with('/') && icon.ends_with(".png") && Path::new(&icon).is_file() {
            return std::fs::read(&icon).ok();
        }
        for size in ["64x64", "48x48", "128x128", "256x256", "32x32"] {
            for root in ["/usr/share/icons/hicolor", "/usr/share/icons/Adwaita"] {
                let f = format!("{root}/{size}/apps/{icon}.png");
                if Path::new(&f).is_file() {
                    return std::fs::read(f).ok();
                }
            }
        }
        let px = format!("/usr/share/pixmaps/{icon}.png");
        return std::fs::read(px).ok();
    }
    if cfg!(target_os = "macos")
        && let Some(path) = path
        && let Some(i) = path.find(".app/")
    {
        let resources = Path::new(&path[..i + 4]).join("Contents").join("Resources");
        let mut icns: Vec<String> = std::fs::read_dir(&resources)
            .ok()?
            .flatten()
            .map(|e| e.path().to_string_lossy().into_owned())
            .filter(|p| p.ends_with(".icns"))
            .collect();
        icns.sort_by_key(|p| p.chars().count()); // stable: the shortest path, ties in folder order
        let icns = icns.into_iter().next()?;
        return sips_png(&icns);
    }
    None
}

/// `sips` converts the bundle's .icns to a 64-pixel PNG in a temporary file, which is read and removed.
fn sips_png(icns: &str) -> Option<Vec<u8>> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let png = std::env::temp_dir().join(format!(
        "phonebox-icon-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let out = png.to_string_lossy().into_owned();
    sh::run(
        "sips",
        &[
            "-s",
            "format",
            "png",
            "--resampleWidth",
            "64",
            icns,
            "--out",
            &out,
        ],
    );
    let bytes = std::fs::read(&png).ok();
    let _ = std::fs::remove_file(&png);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_output_is_read_as_the_csharp_code_reads_it() {
        let args = parse_ps_args(
            "  1 /sbin/launchd\n 501 /Applications/Visual Studio Code.app/Contents/MacOS/Electron --x\nbad\n",
        );
        assert_eq!(
            args[&501],
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron --x"
        );
        let rows = parse_ps_owners(
            "    1     0 /sbin/launchd\n  501   501 /Applications/Visual Studio Code.app/Contents/MacOS/Electron\n 7 x\n",
            &args,
        );
        assert_eq!(rows[&1].uid, 0);
        assert_eq!(
            rows[&501].path,
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"
        );
        assert_eq!(rows[&501].args, args[&501]);
        assert!(!rows.contains_key(&7), "a uid that is not a number");
    }

    #[test]
    fn status_uid_is_the_real_uid() {
        assert_eq!(
            status_uid("Name:\tbash\nUid:\t1000\t0\t0\t0\nGid:\t1\n"),
            Some(1000)
        );
        assert_eq!(status_uid("Name:\tbash\n"), None);
    }

    #[test]
    fn titles_come_from_bundles() {
        assert_eq!(
            title(
                "Electron",
                Some("/Applications/Visual Studio Code.app/Contents/MacOS/Electron")
            ),
            "Visual Studio Code"
        );
        assert_eq!(title("missing-proc-f390", None), "missing-proc-f390");
        assert_eq!(
            shorten(&"y".repeat(141)),
            Some(format!("{}…", "y".repeat(137)))
        );
        assert_eq!(shorten(""), None);
    }

    #[test]
    fn desktop_files_give_name_icon_and_executable() {
        let text = "[Desktop Entry]\r\nName=Visual Studio Code\r\nExec=/usr/share/code/code --unity-launch %F\r\nIcon=vscode\r\n\
                    [Desktop Action new-empty-window]\r\nName=New Empty Window\r\nExec=/usr/share/code/code --new-window %F\r\n";
        assert_eq!(
            parse_desktop(text),
            Some((
                "code".into(),
                "Visual Studio Code".into(),
                Some("vscode".into())
            ))
        );
        // the C# code trims the quotes before splitting, so a quoted path with a space keeps only its first part
        assert_eq!(
            parse_desktop("[Desktop Entry]\nName=App\nExec=\"/opt/My App/app\" %U\n").map(|d| d.0),
            Some("My".into())
        );
        assert_eq!(
            parse_desktop("[Desktop Entry]\nExec=foo\n"),
            None,
            "no name"
        );
    }
}
