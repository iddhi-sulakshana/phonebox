//! Port of `Tray.cs` (Windows) and `Unix/Tray.cs` (macOS and Linux). Windows shows a notification-area icon with C#'s
//! menu; elsewhere there is none, and the log names the settings page instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::adb::AdbReverse;
use crate::agent_endpoint::AgentEndpoint;
use crate::server::Stop;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{startup_folder, toggle_autostart};

/// What a menu item does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Settings,
    OpenPanel,
    EditConfig,
    OpenLog,
    Autostart,
    Quit,
}

/// One line of the tray's context menu (`Tray.cs`'s `ContextMenuStrip`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    Separator,
    Item {
        text: String,
        action: Action,
        enabled: bool,
        /// the default item, opened by a double-click on the icon
        bold: bool,
        /// None: not a check item
        checked: Option<bool>,
    },
}

/// The menu as C# builds it when it opens: the version, the phones, then the actions.
pub fn menu(version: &str, connected: bool, devices: &str, autostart: bool) -> Vec<MenuItem> {
    let item = |text: &str, action: Action| MenuItem::Item {
        text: text.to_string(),
        action,
        enabled: true,
        bold: false,
        checked: None,
    };
    let label = |text: String| MenuItem::Item {
        text,
        action: Action::None,
        enabled: false,
        bold: false,
        checked: None,
    };
    vec![
        label(format!("PhoneBox {version}")),
        label(status_text(connected, devices)),
        MenuItem::Separator,
        MenuItem::Item {
            text: "Settings\u{2026}".to_string(),
            action: Action::Settings,
            enabled: true,
            bold: true,
            checked: None,
        },
        item("Open panel on phone", Action::OpenPanel),
        MenuItem::Separator,
        item("Edit config.json", Action::EditConfig),
        item("Open log", Action::OpenLog),
        MenuItem::Separator,
        MenuItem::Item {
            text: "Start with Windows".to_string(),
            action: Action::Autostart,
            enabled: true,
            bold: false,
            checked: Some(autostart),
        },
        MenuItem::Separator,
        item("Quit", Action::Quit),
    ]
}

/// The menu's second line; `devices` is the phones in use, joined with ", ".
pub fn status_text(connected: bool, devices: &str) -> String {
    if connected {
        format!("Phone connected ({devices})")
    } else {
        "No phone connected".to_string()
    }
}

/// The icon's tooltip once the phones change (until then it reads "PhoneBox", as in C#).
pub fn tooltip(connected: bool) -> &'static str {
    if connected {
        "PhoneBox \u{2014} phone connected"
    } else {
        "PhoneBox \u{2014} waiting for phone"
    }
}

/// macOS and Linux have no tray icon: the log names the settings page instead (two spaces before the parenthesis).
pub fn unix_line(endpoint: &AgentEndpoint) -> String {
    format!(
        "settings: {}  (no tray icon on this system; run PhoneBoxAgent again to open the settings)",
        endpoint.settings_url()
    )
}

/// `AppPaths.StartupShortcut` inside the Startup folder.
pub fn startup_shortcut(folder: &Path) -> PathBuf {
    folder.join("PhoneBox.lnk")
}

/// An image of an `.ico` file: its width in pixels and where its data lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcoEntry {
    pub width: u32,
    pub offset: usize,
    pub size: usize,
}

/// The images of an `.ico` file whose data lies inside it; none when it is not an icon.
pub fn ico_entries(ico: &[u8]) -> Vec<IcoEntry> {
    let u16_at = |i: usize| ico.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| {
        ico.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if u16_at(0) != Some(0) || u16_at(2) != Some(1) {
        return Vec::new();
    }
    let count = u16_at(4).unwrap_or(0) as usize;
    (0..count)
        .filter_map(|n| {
            let at = 6 + n * 16;
            let width = match *ico.get(at)? {
                0 => 256,
                w => u32::from(w),
            };
            let size = u32_at(at + 8)?;
            let offset = u32_at(at + 12)?;
            (size > 0 && offset.checked_add(size)? <= ico.len()).then_some(IcoEntry {
                width,
                offset,
                size,
            })
        })
        .collect()
}

/// The image to draw at `size` pixels: the smallest at least that wide, else the largest. `(offset, length)`.
pub fn icon_entry(ico: &[u8], size: u32) -> Option<(usize, usize)> {
    let entries = ico_entries(ico);
    let fits = entries
        .iter()
        .filter(|e| e.width >= size)
        .min_by_key(|e| e.width);
    fits.or_else(|| entries.iter().max_by_key(|e| e.width))
        .map(|e| (e.offset, e.size))
}

/// The running tray. [`Tray::wait`] lets its thread finish removing the icon before the process ends: an icon a
/// process leaves behind stays in the notification area until the mouse passes over it.
pub struct Tray {
    done: Option<std::sync::mpsc::Receiver<()>>,
}

impl Tray {
    /// Waits at most `timeout` for the tray's thread to end (at once where there is none).
    pub fn wait(self, timeout: Duration) {
        if let Some(done) = self.done {
            let _ = done.recv_timeout(timeout);
        }
    }
}

/// Starts the tray; call it from the runtime, after everything else (C# registers it last, so it also stops first: the
/// icon goes at the start of shutdown, when `stop` fires). It never fails: a tray that cannot start only logs.
pub fn start(endpoint: AgentEndpoint, adb: Arc<AdbReverse>, stop: Stop) -> Tray {
    #[cfg(windows)]
    {
        self::windows::start(endpoint, adb, stop)
    }
    #[cfg(not(windows))]
    {
        let _ = (adb, stop);
        crate::log::info("Tray", &unix_line(&endpoint));
        Tray { done: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, action: Action) -> MenuItem {
        MenuItem::Item {
            text: text.to_string(),
            action,
            enabled: true,
            bold: false,
            checked: None,
        }
    }

    fn disabled(text: &str) -> MenuItem {
        MenuItem::Item {
            text: text.to_string(),
            action: Action::None,
            enabled: false,
            bold: false,
            checked: None,
        }
    }

    #[test]
    fn the_menu_lists_c_sharps_items_in_order() {
        let expected = vec![
            disabled("PhoneBox 1.0.0-alpha.1"),
            disabled("Phone connected (a, b)"),
            MenuItem::Separator,
            MenuItem::Item {
                text: "Settings…".to_string(),
                action: Action::Settings,
                enabled: true,
                bold: true,
                checked: None,
            },
            item("Open panel on phone", Action::OpenPanel),
            MenuItem::Separator,
            item("Edit config.json", Action::EditConfig),
            item("Open log", Action::OpenLog),
            MenuItem::Separator,
            MenuItem::Item {
                text: "Start with Windows".to_string(),
                action: Action::Autostart,
                enabled: true,
                bold: false,
                checked: Some(true),
            },
            MenuItem::Separator,
            item("Quit", Action::Quit),
        ];
        assert_eq!(menu("1.0.0-alpha.1", true, "a, b", true), expected);
        let off = menu("1.0.0-alpha.1", false, "", false);
        assert_eq!(off[1], disabled("No phone connected"));
        assert!(matches!(
            &off[9],
            MenuItem::Item {
                checked: Some(false),
                ..
            }
        ));
    }

    #[test]
    fn status_and_tooltip_texts() {
        assert_eq!(status_text(true, "a, b"), "Phone connected (a, b)");
        assert_eq!(status_text(false, "a"), "No phone connected");
        assert_eq!(tooltip(true), "PhoneBox \u{2014} phone connected");
        assert_eq!(tooltip(false), "PhoneBox \u{2014} waiting for phone");
    }

    #[test]
    fn the_unix_line_names_the_settings_url() {
        let endpoint = crate::agent_endpoint::AgentEndpoint { port: 8765 };
        assert_eq!(
            unix_line(&endpoint),
            "settings: http://127.0.0.1:8765/  (no tray icon on this system; run PhoneBoxAgent again to open the \
             settings)"
        );
    }

    #[test]
    fn the_tray_icon_takes_the_smallest_entry_at_least_the_size() {
        // the agent's icon: 16, 24, 32, 48, 64, 128 and 256 px
        let ico = crate::server::static_files::FAVICON;
        let (offset, size) = icon_entry(ico, 20).expect("an entry");
        let entries = ico_entries(ico);
        assert_eq!(entries.len(), 7);
        let chosen = entries.iter().find(|e| e.offset == offset).unwrap();
        assert_eq!((chosen.width, chosen.size), (24, size));
        assert_eq!(&ico[offset + 1..offset + 4], b"PNG");
        // larger than every entry: the largest
        let (offset, _) = icon_entry(ico, 512).unwrap();
        assert_eq!(
            entries.iter().find(|e| e.offset == offset).unwrap().width,
            256
        );
        // not an icon, or an entry outside the data
        assert_eq!(icon_entry(b"nope", 16), None);
        let mut cut = ico.to_vec();
        cut.truncate(200);
        assert_eq!(icon_entry(&cut, 16), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_startup_shortcut_points_at_the_exe_and_toggles() {
        let folder = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let link = startup_shortcut(folder.path());
        assert_eq!(link, folder.path().join("PhoneBox.lnk"));
        assert_eq!(toggle_autostart(folder.path(), &exe), Ok(true));
        assert!(link.exists());
        let (target, working, description) = self::windows::read_shortcut(&link).unwrap();
        assert!(
            target.eq_ignore_ascii_case(&exe.display().to_string()),
            "{target}"
        );
        let exe_folder = exe.parent().unwrap().display().to_string();
        assert!(working.eq_ignore_ascii_case(&exe_folder), "{working}");
        assert_eq!(description, "PhoneBox agent");
        assert_eq!(toggle_autostart(folder.path(), &exe), Ok(false));
        assert!(!link.exists());
    }
}
