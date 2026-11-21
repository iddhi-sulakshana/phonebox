//! Port of `KeySender.cs`: key combinations, text and mouse input for shortcut keys, macros, dictation and the
//! trackpad: "ctrl+shift+m", "win+d", "f13", "playpause"; several combos separated by commas are pressed in turn.
//! Windows: SendInput. Linux: xdotool (X11) or wtype (Wayland). macOS: System Events via osascript (needs the
//! Accessibility permission). Errors are the C# exception messages.

use std::sync::Mutex;

/// What to do when macOS refuses to send keys. Fixed text, so the phone may show it as it is.
pub const ACCESSIBILITY_HINT: &str = "macOS refused: allow PhoneBoxAgent in System Settings › Privacy & Security › Accessibility. If it is already listed, remove it there with − and add it again";

const MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "win"];

fn is_modifier(k: &str) -> bool {
    MODIFIERS.contains(&k)
}

/// name → (Windows virtual key, xdotool name, macOS key code; -1: none)
const SPECIAL: [(&str, u8, &str, i32); 46] = [
    ("enter", 0x0D, "Return", 36),
    ("tab", 0x09, "Tab", 48),
    ("esc", 0x1B, "Escape", 53),
    ("space", 0x20, "space", 49),
    ("backspace", 0x08, "BackSpace", 51),
    ("delete", 0x2E, "Delete", 117),
    ("insert", 0x2D, "Insert", 114),
    ("home", 0x24, "Home", 115),
    ("end", 0x23, "End", 119),
    ("pageup", 0x21, "Prior", 116),
    ("pagedown", 0x22, "Next", 121),
    ("up", 0x26, "Up", 126),
    ("down", 0x28, "Down", 125),
    ("left", 0x25, "Left", 123),
    ("right", 0x27, "Right", 124),
    ("printscreen", 0x2C, "Print", 105),
    ("minus", 0xBD, "minus", 27),
    ("equals", 0xBB, "equal", 24),
    ("comma", 0xBC, "comma", 43),
    ("period", 0xBE, "period", 47),
    ("slash", 0xBF, "slash", 44),
    ("semicolon", 0xBA, "semicolon", 41),
    ("bracketleft", 0xDB, "bracketleft", 33),
    ("bracketright", 0xDD, "bracketright", 30),
    ("playpause", 0xB3, "XF86AudioPlay", -1),
    ("nexttrack", 0xB0, "XF86AudioNext", -1),
    ("prevtrack", 0xB1, "XF86AudioPrev", -1),
    ("mute", 0xAD, "XF86AudioMute", -1),
    ("volumeup", 0xAF, "XF86AudioRaiseVolume", -1),
    ("volumedown", 0xAE, "XF86AudioLowerVolume", -1),
    // the keypad: its own keys, not the number row, so a shortcut bound to Numpad3 (Discord, games) is matched
    ("numpad0", 0x60, "KP_0", 82),
    ("numpad1", 0x61, "KP_1", 83),
    ("numpad2", 0x62, "KP_2", 84),
    ("numpad3", 0x63, "KP_3", 85),
    ("numpad4", 0x64, "KP_4", 86),
    ("numpad5", 0x65, "KP_5", 87),
    ("numpad6", 0x66, "KP_6", 88),
    ("numpad7", 0x67, "KP_7", 89),
    ("numpad8", 0x68, "KP_8", 91),
    ("numpad9", 0x69, "KP_9", 92),
    ("numpadmultiply", 0x6A, "KP_Multiply", 67),
    ("numpadadd", 0x6B, "KP_Add", 69),
    ("numpadsubtract", 0x6D, "KP_Subtract", 78),
    ("numpaddecimal", 0x6E, "KP_Decimal", 65),
    ("numpaddivide", 0x6F, "KP_Divide", 75),
    // Windows has no key code of its own for the keypad's Enter: VK_RETURN with the extended flag (win_extended)
    ("numpadenter", 0x0D, "KP_Enter", 76),
];

fn special(k: &str) -> Option<(u8, &'static str, i32)> {
    SPECIAL.iter().find(|s| s.0 == k).map(|s| (s.1, s.2, s.3))
}

/// `int.TryParse` of an F-key number: white space around and one sign allowed.
fn f_number(rest: &str) -> Option<i32> {
    rest.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
        .parse()
        .ok()
}

/// Splits "Ctrl+Shift+M, Win+D" into combos of normalized key names; unknown keys are an error.
pub fn parse(spec: &str) -> Result<Vec<Vec<String>>, String> {
    let mut combos = vec![];
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let keys: Vec<String> = part
            .split('+')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(normalize)
            .collect();
        if keys.is_empty() {
            continue;
        }
        if keys.iter().filter(|k| !is_modifier(k)).count() > 1 {
            return Err(format!(
                "\"{part}\" has more than one key besides Ctrl / Shift / Alt / Win"
            ));
        }
        if let Some(k) = keys.iter().find(|k| !is_modifier(k) && !is_known(k)) {
            return Err(format!("unknown key \"{k}\""));
        }
        combos.push(keys);
    }
    if combos.is_empty() {
        return Err("no keys given".into());
    }
    Ok(combos)
}

fn normalize(k: &str) -> String {
    let lower = k.to_lowercase();
    if let Some(n) = numpad(&lower) {
        return n;
    }
    match lower.as_str() {
        "control" | "ctl" => "ctrl",
        "cmd" | "command" | "super" | "meta" | "windows" | "⌘" => "win",
        "option" | "opt" | "⌥" => "alt",
        "return" => "enter",
        "escape" => "esc",
        "del" => "delete",
        "bksp" => "backspace",
        "pgup" => "pageup",
        "pgdn" | "pagedn" => "pagedown",
        "prtsc" | "print" => "printscreen",
        "play" | "pause" | "play/pause" => "playpause",
        "next" => "nexttrack",
        "prev" | "previous" => "prevtrack",
        "volup" => "volumeup",
        "[" | "openbracket" => "bracketleft",
        "]" | "closebracket" => "bracketright",
        "voldown" => "volumedown",
        _ => return lower,
    }
    .to_string()
}

/// The keypad under the names other tools use: numpad3, num3, kp3, KP_3 (xdotool), numpad_3; numpadadd, kpplus…
fn numpad(k: &str) -> Option<String> {
    const PREFIXES: [&str; 6] = ["numpad_", "numpad", "num_", "num", "kp_", "kp"];
    let prefix = PREFIXES.iter().find(|p| k.starts_with(**p))?;
    let rest = &k[prefix.len()..];
    let name = match rest {
        d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => d,
        "enter" | "return" => "enter",
        "add" | "plus" => "add",
        "subtract" | "sub" | "minus" => "subtract",
        "multiply" | "mul" | "times" | "*" => "multiply",
        "divide" | "div" | "slash" | "/" => "divide",
        "decimal" | "dec" | "period" | "dot" | "." | "comma" | "separator" => "decimal",
        _ => return None,
    };
    Some(format!("numpad{name}"))
}

fn is_known(k: &str) -> bool {
    let chars: Vec<char> = k.chars().collect();
    (chars.len() == 1 && chars[0].is_ascii_alphanumeric())
        || special(k).is_some()
        || (matches!(chars.len(), 2 | 3)
            && chars[0] == 'f'
            && f_number(&k[1..]).is_some_and(|n| (1..=24).contains(&n)))
}

/// Keys Windows tells apart from another only by the "extended" flag: the keypad's Enter and its /.
pub fn win_extended(k: &str) -> bool {
    matches!(k, "numpadenter" | "numpaddivide")
}

/// The Windows virtual-key code of a normalized key name; modifiers are the left-hand keys (VK_LSHIFT, not VK_SHIFT):
/// raw-input apps (Discord) could not tell which Shift went down.
pub fn win_vk(k: &str) -> u8 {
    match k {
        "ctrl" => 0xA2,
        "shift" => 0xA0,
        "alt" => 0xA4,
        "win" => 0x5B,
        _ => {
            if let Some((vk, _, _)) = special(k) {
                vk
            } else if k.starts_with('f') && k.len() > 1 {
                (0x70 + f_number(&k[1..]).unwrap_or(1) - 1) as u8
            } else {
                k.chars().next().map_or(0, |c| c.to_ascii_uppercase() as u8)
            }
        }
    }
}

/// Presses every combo of the spec in turn (60 ms apart). A combo the tool refused is not an error (as in C#).
pub fn send_keys(spec: &str) -> Result<(), String> {
    let combos = parse(spec)?;
    for combo in &combos {
        send_combo(combo)?;
        if combos.len() > 1 {
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }
    Ok(())
}

pub fn type_text(text: &str) -> Result<(), String> {
    try_type_text(text).map(|_| ())
}

// ---------------------------------------------------------------- mouse

/// A mouse call the tests record instead of moving the real pointer.
#[derive(Debug, Clone, PartialEq)]
pub struct MouseCall {
    pub action: String,
    pub dx: i32,
    pub dy: i32,
    pub delta: i32,
}

type MouseSink = Box<dyn Fn(MouseCall) + Send + Sync>;
static TEST_MOUSE_SINK: Mutex<Option<MouseSink>> = Mutex::new(None);

/// Hands the call to the test sink when one is set (true), so nothing reaches the real pointer.
fn to_sink(action: &str, dx: i32, dy: i32, delta: i32) -> bool {
    let sink = TEST_MOUSE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    match sink.as_ref() {
        Some(f) => {
            f(MouseCall {
                action: action.into(),
                dx,
                dy,
                delta,
            });
            true
        }
        None => false,
    }
}

pub fn mouse_move(dx: i32, dy: i32) -> Result<(), String> {
    if to_sink("move", dx, dy, 0) {
        return Ok(());
    }
    os::mouse_move(dx, dy)
}

pub fn mouse_click() -> Result<(), String> {
    if to_sink("click", 0, 0, 0) {
        return Ok(());
    }
    os::mouse_click()
}

pub fn mouse_right_click() -> Result<(), String> {
    if to_sink("rightClick", 0, 0, 0) {
        return Ok(());
    }
    os::mouse_right_click()
}

pub fn mouse_scroll(delta: i32) -> Result<(), String> {
    if delta == 0 || to_sink("scroll", 0, 0, delta) {
        return Ok(());
    }
    os::mouse_scroll(delta)
}

pub fn mouse_horizontal_scroll(delta: i32) -> Result<(), String> {
    if delta == 0 || to_sink("hscroll", 0, 0, delta) {
        return Ok(());
    }
    os::mouse_horizontal_scroll(delta)
}

pub fn mouse_down() -> Result<(), String> {
    if to_sink("mouseDown", 0, 0, 0) {
        return Ok(());
    }
    os::mouse_down()
}

pub fn mouse_up() -> Result<(), String> {
    if to_sink("mouseUp", 0, 0, 0) {
        return Ok(());
    }
    os::mouse_up()
}

/// Records the mouse calls of a test; one recorder at a time (the sink is process-wide), cleared on drop.
#[cfg(test)]
pub(crate) struct MouseRecorder {
    calls: std::sync::Arc<Mutex<Vec<MouseCall>>>,
    _one_at_a_time: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl MouseRecorder {
    pub(crate) fn start() -> MouseRecorder {
        static ONE: Mutex<()> = Mutex::new(());
        let guard = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let calls = std::sync::Arc::new(Mutex::new(vec![]));
        let sink = calls.clone();
        *TEST_MOUSE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move |c| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(c)
        }));
        MouseRecorder {
            calls,
            _one_at_a_time: guard,
        }
    }

    pub(crate) fn calls(&self) -> Vec<MouseCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
impl Drop for MouseRecorder {
    fn drop(&mut self) {
        *TEST_MOUSE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(windows)]
pub use os::tap;
pub use os::{send_combo, try_type_text};

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod os {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAP_VIRTUAL_KEY_TYPE,
        MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
    };

    use super::{win_extended, win_vk};

    fn send(inputs: &[INPUT]) -> bool {
        // SAFETY: a slice of fully initialized INPUT structs and their size
        unsafe { SendInput(inputs, size_of::<INPUT>() as i32) as usize == inputs.len() }
    }

    fn key(vk: u8, up: bool, force_extended: bool) -> INPUT {
        // the scan code too: raw-input apps tell keys (and left from right) apart by it; MAPVK_VK_TO_VSC_EX marks the
        // media keys with E0, while the arrows and the Home/End block share their codes with the keypad
        // SAFETY: a plain lookup
        let sc = unsafe { MapVirtualKeyW(u32::from(vk), MAP_VIRTUAL_KEY_TYPE(4)) };
        let extended = force_extended
            || (0x21..=0x2E).contains(&vk)
            || vk == 0x5B
            || matches!(sc >> 8, 0xE0 | 0xE1);
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(u16::from(vk)),
                    wScan: (sc & 0xFF) as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn unicode(unit: u16, up: bool) -> INPUT {
        let mut flags = KEYEVENTF_UNICODE;
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Presses one parsed combo: all keys down, then up in reverse; false when Windows took none or only part.
    pub fn send_combo(combo: &[String]) -> Result<bool, String> {
        let keys: Vec<(u8, bool)> = combo.iter().map(|k| (win_vk(k), win_extended(k))).collect();
        let mut inputs: Vec<INPUT> = keys.iter().map(|(vk, ext)| key(*vk, false, *ext)).collect();
        inputs.extend(keys.iter().rev().map(|(vk, ext)| key(*vk, true, *ext)));
        Ok(send(&inputs))
    }

    /// Types the text (new lines as Enter, everything else as Unicode units); false when Windows took only part.
    pub fn try_type_text(text: &str) -> Result<bool, String> {
        let mut inputs = vec![];
        for unit in text.replace("\r\n", "\n").encode_utf16() {
            if unit == u16::from(b'\n') {
                inputs.push(key(0x0D, false, false));
                inputs.push(key(0x0D, true, false));
            } else {
                inputs.push(unicode(unit, false));
                inputs.push(unicode(unit, true));
            }
        }
        Ok(send(&inputs))
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub fn mouse_move(dx: i32, dy: i32) -> Result<(), String> {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]);
        Ok(())
    }
    pub fn mouse_click() -> Result<(), String> {
        send(&[
            mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            mouse(0, 0, 0, MOUSEEVENTF_LEFTUP),
        ]);
        Ok(())
    }
    pub fn mouse_right_click() -> Result<(), String> {
        send(&[
            mouse(0, 0, 0, MOUSEEVENTF_RIGHTDOWN),
            mouse(0, 0, 0, MOUSEEVENTF_RIGHTUP),
        ]);
        Ok(())
    }
    pub fn mouse_scroll(delta: i32) -> Result<(), String> {
        send(&[mouse(0, 0, delta, MOUSEEVENTF_WHEEL)]);
        Ok(())
    }
    pub fn mouse_horizontal_scroll(delta: i32) -> Result<(), String> {
        send(&[mouse(0, 0, delta, MOUSEEVENTF_HWHEEL)]);
        Ok(())
    }
    pub fn mouse_down() -> Result<(), String> {
        send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
        Ok(())
    }
    pub fn mouse_up() -> Result<(), String> {
        send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
        Ok(())
    }

    /// `Keys.Tap`: one key down and up through keybd_event (the mute and play/pause keys).
    pub fn tap(vk: u8) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{KEYBD_EVENT_FLAGS, keybd_event};
        // SAFETY: plain calls
        unsafe {
            keybd_event(vk, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        }
    }

    /// A zero-pixel mouse move: this process becomes the one that received the last input, which Windows requires
    /// before it lets a process bring a window to the front (used by plan 6's app console).
    #[allow(dead_code)]
    pub fn nudge_input() -> bool {
        send(&[mouse(0, 0, 0, MOUSEEVENTF_MOVE)])
    }
}

// ---------------------------------------------------------------- Linux and macOS

#[cfg(not(windows))]
mod os {
    use std::sync::Mutex;

    use super::{ACCESSIBILITY_HINT, is_modifier, special};
    use crate::unix::sh;

    fn x11() -> bool {
        sh::which("xdotool").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none()
    }

    /// Presses one parsed combo; false when xdotool or wtype failed (macOS answers an error when it refuses).
    pub fn send_combo(combo: &[String]) -> Result<bool, String> {
        if cfg!(target_os = "macos") {
            mac_keystroke(combo)?;
            return Ok(true);
        }
        let x = combo
            .iter()
            .map(|k| match k.as_str() {
                "win" => "super".to_string(),
                "ctrl" | "shift" | "alt" => k.clone(),
                _ => match special(k) {
                    Some((_, name, _)) => name.to_string(),
                    None if k.starts_with('f') && k.len() > 1 => k.to_uppercase(),
                    None => k.clone(),
                },
            })
            .collect::<Vec<_>>()
            .join("+");
        if x11() {
            return Ok(sh::run("xdotool", &["key", "--clearmodifiers", &x]).is_some());
        }
        if sh::which("wtype").is_some() {
            let mods: Vec<&str> = combo
                .iter()
                .filter(|k| is_modifier(k))
                .map(|m| if m == "win" { "logo" } else { m.as_str() })
                .collect();
            let mut args: Vec<String> = vec![];
            for m in &mods {
                args.extend(["-M".to_string(), m.to_string()]);
            }
            if let Some(key) = combo.iter().find(|k| !is_modifier(k)) {
                args.push("-k".into());
                args.push(special(key).map_or_else(|| key.clone(), |s| s.1.to_string()));
            }
            for m in mods.iter().rev() {
                args.extend(["-m".to_string(), m.to_string()]);
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            return Ok(sh::run("wtype", &refs).is_some());
        }
        if sh::which("xdotool").is_some() {
            return Ok(sh::run("xdotool", &["key", "--clearmodifiers", &x]).is_some());
        }
        Err("sending keys needs xdotool (X11) or wtype (Wayland)".into())
    }

    /// Types the text; false when the tool failed or timed out (macOS answers an error when it refuses). The tool gets
    /// time for the whole text: xdotool types a character every 8 ms.
    pub fn try_type_text(text: &str) -> Result<bool, String> {
        let timeout = 3000 + 20 * text.encode_utf16().count() as u64;
        let run = |tool: &str, args: &[&str]| {
            sh::which(tool).is_some() && sh::run_with_timeout(timeout, tool, args).is_some()
        };
        if cfg!(target_os = "macos") {
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!("tell application \"System Events\" to keystroke \"{escaped}\"");
            if !run("osascript", &["-e", &script]) {
                return Err(ACCESSIBILITY_HINT.into());
            }
            return Ok(true);
        }
        if x11() {
            return Ok(run("xdotool", &["type", "--delay", "8", "--", text]));
        }
        if sh::which("wtype").is_some() {
            return Ok(run("wtype", &["--", text]));
        }
        if sh::which("xdotool").is_some() {
            return Ok(run("xdotool", &["type", "--delay", "8", "--", text]));
        }
        Err("typing needs xdotool (X11) or wtype (Wayland)".into())
    }

    fn mac_keystroke(combo: &[String]) -> Result<(), String> {
        let mods: Vec<&str> = combo
            .iter()
            .filter(|k| is_modifier(k))
            .map(|m| match m.as_str() {
                "win" => "command down",
                "ctrl" => "control down",
                "alt" => "option down",
                _ => "shift down",
            })
            .collect();
        let key = combo.iter().find(|k| !is_modifier(k));
        if let Some(k) = key
            && let Some((_, _, mac)) = special(k)
            && mac < 0
        {
            // media keys have no key code in System Events; the media remote where we can
            if k == "playpause" {
                sh::run(
                    "osascript",
                    &["-e", "tell application \"Music\" to playpause"],
                );
                return Ok(());
            }
            return Err(format!("{k} cannot be sent on macOS"));
        }
        let using = if mods.is_empty() {
            String::new()
        } else {
            format!(" using {{{}}}", mods.join(", "))
        };
        let Some(key) = key else {
            return Err("only modifier keys given".into());
        };
        let action = if let Some((_, _, mac)) = special(key) {
            format!("key code {mac}")
        } else if key.starts_with('f') && key.len() > 1 {
            format!(
                "key code {}",
                mac_f(super::f_number(&key[1..]).unwrap_or(0))?
            )
        } else {
            format!("keystroke \"{key}\"")
        };
        let script = format!("tell application \"System Events\" to {action}{using}");
        if sh::run_with_timeout(5000, "osascript", &["-e", &script]).is_none() {
            return Err(ACCESSIBILITY_HINT.into());
        }
        Ok(())
    }

    fn mac_f(n: i32) -> Result<i32, String> {
        const CODES: [i32; 20] = [
            122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80,
            90,
        ];
        usize::try_from(n - 1)
            .ok()
            .and_then(|i| CODES.get(i).copied())
            .ok_or_else(|| format!("F{n} cannot be sent on macOS"))
    }

    // the mouse: xdotool only (X11); moves coalesce for 16 ms, scrolls add up to 120-unit notches

    #[derive(Default)]
    struct Pending {
        dx: i32,
        dy: i32,
        scheduled: bool,
        v_scroll: i32,
        h_scroll: i32,
    }
    static PENDING: Mutex<Pending> = Mutex::new(Pending {
        dx: 0,
        dy: 0,
        scheduled: false,
        v_scroll: 0,
        h_scroll: 0,
    });
    static SEND: Mutex<()> = Mutex::new(());

    fn pending() -> std::sync::MutexGuard<'static, Pending> {
        PENDING.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn xdotool(args: &[&str]) -> Result<(), String> {
        if cfg!(target_os = "macos") {
            return Err("The Trackpad widget is not supported on macOS yet.".into());
        }
        if sh::which("xdotool").is_none() {
            return Err("controlling the mouse needs xdotool (X11)".into());
        }
        // a failed run is not an error (ToolProcess only throws on a timeout or a start failure)
        sh::run_any_exit(2000, "xdotool", args);
        Ok(())
    }

    fn flush_move() -> Result<(), String> {
        let (dx, dy) = {
            let mut p = pending();
            let moved = (p.dx, p.dy);
            p.dx = 0;
            p.dy = 0;
            moved
        };
        if dx != 0 || dy != 0 {
            xdotool(&["mousemove_relative", "--", &dx.to_string(), &dy.to_string()])?;
        }
        Ok(())
    }

    fn action(args: &[&str]) -> Result<(), String> {
        let _send = SEND.lock().unwrap_or_else(|e| e.into_inner());
        flush_move()?;
        xdotool(args)
    }

    pub fn mouse_move(dx: i32, dy: i32) -> Result<(), String> {
        {
            let mut p = pending();
            p.dx += dx;
            p.dy += dy;
            if p.scheduled {
                return Ok(());
            }
            p.scheduled = true;
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(16));
            let _send = SEND.lock().unwrap_or_else(|e| e.into_inner());
            pending().scheduled = false;
            let _ = flush_move();
        });
        Ok(())
    }

    pub fn mouse_click() -> Result<(), String> {
        action(&["click", "1"])
    }
    pub fn mouse_right_click() -> Result<(), String> {
        action(&["click", "3"])
    }
    pub fn mouse_down() -> Result<(), String> {
        action(&["mousedown", "1"])
    }
    pub fn mouse_up() -> Result<(), String> {
        action(&["mouseup", "1"])
    }

    /// Whole 120-unit notches of the accumulated delta, and the button for their direction.
    fn notches(
        acc: &mut i32,
        delta: i32,
        positive: &'static str,
        negative: &'static str,
    ) -> Option<(i32, &'static str)> {
        *acc += delta;
        let n = acc.abs() / 120;
        if n == 0 {
            return None;
        }
        let dir = if *acc > 0 { positive } else { negative };
        *acc -= acc.signum() * n * 120;
        Some((n, dir))
    }

    pub fn mouse_scroll(delta: i32) -> Result<(), String> {
        let step = { notches(&mut pending().v_scroll, delta, "4", "5") };
        match step {
            Some((n, dir)) => action(&["click", "--repeat", &n.to_string(), dir]),
            None => Ok(()),
        }
    }

    pub fn mouse_horizontal_scroll(delta: i32) -> Result<(), String> {
        let step = { notches(&mut pending().h_scroll, delta, "7", "6") };
        match step {
            Some((n, dir)) => action(&["click", "--repeat", &n.to_string(), dir]),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combos(spec: &str) -> Vec<Vec<String>> {
        parse(spec).unwrap_or_else(|e| panic!("{spec}: {e}"))
    }
    fn of(list: &[&[&str]]) -> Vec<Vec<String>> {
        list.iter()
            .map(|c| c.iter().map(|k| k.to_string()).collect())
            .collect()
    }

    #[test]
    fn parses_combos_and_sequences() {
        assert_eq!(
            combos("Ctrl+Shift+M, cmd+Space,F13, play"),
            of(&[
                &["ctrl", "shift", "m"],
                &["win", "space"],
                &["f13"],
                &["playpause"]
            ])
        );
    }

    #[test]
    fn rejects_bad_specs() {
        for spec in ["ctrl+a+b", "ctrl+banana", "f25", " , "] {
            assert!(parse(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn brackets_are_keys() {
        assert_eq!(
            combos("[, ctrl+]"),
            of(&[&["bracketleft"], &["ctrl", "bracketright"]])
        );
        assert_eq!(win_vk("bracketleft"), 0xDB);
        assert_eq!(win_vk("bracketright"), 0xDD);
    }

    #[test]
    fn modifiers_alone_are_allowed() {
        assert_eq!(combos("Win"), of(&[&["win"]]));
    }

    #[test]
    fn windows_keys_are_the_left_hand_ones() {
        for (key, vk) in [
            ("shift", 0xA0),
            ("ctrl", 0xA2),
            ("alt", 0xA4),
            ("win", 0x5B),
            ("f24", 0x87),
            ("m", 0x4D),
            ("playpause", 0xB3),
        ] {
            assert_eq!(win_vk(key), vk, "{key}");
        }
    }

    #[test]
    fn numpad_keys_have_their_own_names() {
        for (spec, key) in [
            ("Ctrl+Alt+Numpad3", "numpad3"),
            ("ctrl+alt+num3", "numpad3"),
            ("ctrl+alt+kp3", "numpad3"),
            ("Ctrl+Alt+KP_3", "numpad3"),
            ("ctrl+alt+numpad_3", "numpad3"),
            ("ctrl+alt+NumpadEnter", "numpadenter"),
            ("ctrl+alt+KP_Enter", "numpadenter"),
            ("ctrl+alt+numpadplus", "numpadadd"),
            ("ctrl+alt+KP_Add", "numpadadd"),
            ("ctrl+alt+NumpadSubtract", "numpadsubtract"),
            ("ctrl+alt+kpminus", "numpadsubtract"),
            ("ctrl+alt+numpad*", "numpadmultiply"),
            ("ctrl+alt+NumpadDivide", "numpaddivide"),
            ("ctrl+alt+NumpadDecimal", "numpaddecimal"),
        ] {
            assert_eq!(combos(spec), of(&[&["ctrl", "alt", key]]), "{spec}");
        }
    }

    #[test]
    fn unknown_numpad_names_are_rejected() {
        for spec in ["numpad10", "numpad", "numlock", "kp_banana"] {
            assert!(parse(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn windows_keypad_codes() {
        for (key, vk, extended) in [
            ("numpad0", 0x60, false),
            ("numpad9", 0x69, false),
            ("numpadmultiply", 0x6A, false),
            ("numpadadd", 0x6B, false),
            ("numpadsubtract", 0x6D, false),
            ("numpaddecimal", 0x6E, false),
            ("numpaddivide", 0x6F, true),
            ("numpadenter", 0x0D, true), // VK_RETURN, told apart from the main Enter only by the extended flag
            ("enter", 0x0D, false),
            ("3", 0x33, false), // the number row stays the number row
        ] {
            assert_eq!((win_vk(key), win_extended(key)), (vk, extended), "{key}");
        }
    }

    #[test]
    fn unusual_specs_are_refused_with_the_csharp_messages() {
        assert_eq!(parse(" , ,"), Err("no keys given".into()));
        assert_eq!(
            parse("ctrl+a+b"),
            Err("\"ctrl+a+b\" has more than one key besides Ctrl / Shift / Alt / Win".into())
        );
        assert_eq!(parse("ctrl+Banana"), Err("unknown key \"banana\"".into()));
        assert_eq!(parse("ctrl+😀"), Err("unknown key \"😀\"".into()));
        assert_eq!(parse("+,+"), Err("no keys given".into()));
        assert_eq!(
            combos("⌘+⌥+Escape, Return"),
            of(&[&["win", "alt", "esc"], &["enter"]])
        );
        assert_eq!(
            combos("F1, f12, PgDn, del, PrtSc"),
            of(&[
                &["f1"],
                &["f12"],
                &["pagedown"],
                &["delete"],
                &["printscreen"]
            ])
        );
        assert_eq!(win_vk("f1"), 0x70);
        assert_eq!(win_vk("pagedown"), 0x22);
    }

    #[test]
    fn the_mouse_goes_to_the_test_sink_and_zero_scrolls_are_dropped() {
        let recorder = MouseRecorder::start();
        mouse_move(3, -4).unwrap();
        mouse_click().unwrap();
        mouse_scroll(0).unwrap();
        mouse_scroll(120).unwrap();
        mouse_horizontal_scroll(-240).unwrap();
        mouse_right_click().unwrap();
        mouse_down().unwrap();
        mouse_up().unwrap();
        let calls: Vec<(String, i32, i32, i32)> = recorder
            .calls()
            .into_iter()
            .map(|c| (c.action, c.dx, c.dy, c.delta))
            .collect();
        assert_eq!(
            calls,
            [
                ("move".into(), 3, -4, 0),
                ("click".into(), 0, 0, 0),
                ("scroll".into(), 0, 0, 120),
                ("hscroll".into(), 0, 0, -240),
                ("rightClick".into(), 0, 0, 0),
                ("mouseDown".into(), 0, 0, 0),
                ("mouseUp".into(), 0, 0, 0),
            ]
        );
    }
}
