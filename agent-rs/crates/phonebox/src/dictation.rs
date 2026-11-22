//! `Dictation`: the phone turns speech into text with its own recognizer and sends the text here, to be typed into the
//! app in front. Off unless the widget's own "Type the text on the computer" option is on, on an enabled page of this
//! phone. Control characters are removed here (a line break would be Enter, which sends a chat message or runs a
//! command), so Enter is pressed only with the widget's "Press Enter after the text" option. The text is never logged.

use std::sync::Arc;

use crate::config::store::ConfigStore;

pub const WIDGET_TYPE: &str = "dictate";

/// The longest dictation. Its time budget: typing gets at most 3 s + 20 ms a character (23 s, key_sender) plus up to
/// 2 s waiting for the previous text, inside the phone's 30 s request deadline.
pub const MAX_CHARS: usize = 1000;

/// The app in front (plan 6's AppConsole finds it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedApp {
    pub process: String,
    pub elevated: bool,
    pub window: isize,
}

pub type TypeFn = Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync>;
pub type ComboFn = Arc<dyn Fn(&[String]) -> Result<bool, String> + Send + Sync>;
pub type FocusedFn = Arc<dyn Fn() -> Option<FocusedApp> + Send + Sync>;

pub struct Dictation {
    cfg: Arc<ConfigStore>,
    typing: Arc<tokio::sync::Semaphore>,
    pub(crate) type_text: TypeFn,
    pub(crate) send_combo: ComboFn,
    pub(crate) focused: FocusedFn,
    pub(crate) check_elevation: bool,
    pub(crate) agent_elevated: bool,
}

/// One line of plain text: control characters become spaces, runs of spaces one, at most [`MAX_CHARS`] UTF-16 units.
pub fn clean(text: Option<&str>) -> String {
    let spaced: String = text
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let line = spaced
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // at most MAX_CHARS UTF-16 units; a character that would be cut in half is left out
    let mut units = 0;
    let mut end = line.len();
    for (i, c) in line.char_indices() {
        if units + c.len_utf16() > MAX_CHARS {
            end = i;
            break;
        }
        units += c.len_utf16();
    }
    line[..end].to_string()
}

/// `Environment.IsPrivilegedProcess`: an elevated process on Windows, root elsewhere.
pub(crate) fn is_privileged_process() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = HANDLE::default();
        // SAFETY: the token is closed below; GetTokenInformation writes at most size_of::<TOKEN_ELEVATION>() bytes
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok && elevation.TokenIsElevated != 0
        }
    }
    #[cfg(not(windows))]
    {
        crate::unix::sh::uid() == 0
    }
}

impl Dictation {
    pub fn new(cfg: Arc<ConfigStore>) -> Arc<Dictation> {
        Arc::new(Dictation::plain(cfg))
    }

    pub(crate) fn plain(cfg: Arc<ConfigStore>) -> Dictation {
        Dictation {
            cfg,
            typing: Arc::new(tokio::sync::Semaphore::new(1)),
            type_text: Arc::new(crate::key_sender::try_type_text),
            send_combo: Arc::new(crate::key_sender::send_combo),
            focused: Arc::new(crate::app_console::focused_now),
            check_elevation: cfg!(windows),
            agent_elevated: is_privileged_process(),
        }
    }

    fn widget(
        &self,
        device_id: Option<&str>,
        widget: Option<&str>,
    ) -> Option<crate::config::WidgetConfig> {
        crate::panel_widgets::find(&self.cfg.get(), device_id, widget, WIDGET_TYPE)
    }

    /// The status and the error for the phone (None on success).
    pub async fn type_async(
        self: &Arc<Self>,
        device_id: Option<&str>,
        widget: Option<&str>,
        text: Option<&str>,
    ) -> (u16, Option<String>) {
        let allowed = |w: &Option<crate::config::WidgetConfig>| {
            w.as_ref().and_then(|w| w.bool("control")) == Some(true)
        };
        if !allowed(&self.widget(device_id, widget)) {
            return (
                403,
                Some("typing is off: turn on \"Type the text on the computer\" in the dictation widget's settings".into()),
            );
        }
        // the phone keeps to MAX_CHARS
        if text.unwrap_or("").encode_utf16().count() > MAX_CHARS * 2 {
            return (413, Some("the text is too long".into()));
        }
        let line = clean(text);
        if line.is_empty() {
            return (400, Some("nothing to type".into()));
        }
        // Windows drops input to an app running as administrator without a word (UIPI): say so instead
        if self.check_elevation
            && (self.focused)().is_some_and(|f| f.elevated)
            && !self.agent_elevated
        {
            return (
                409,
                Some("the app in front runs as administrator, so Windows doesn't let the agent type into it".into()),
            );
        }

        let Ok(Ok(permit)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.typing.clone().acquire_owned(),
        )
        .await
        else {
            return (429, Some("still typing the previous text".into()));
        };
        // the switch can be turned off while this waited for the previous text: decide with the settings of now
        let w = self.widget(device_id, widget);
        if !allowed(&w) {
            return (403, Some("typing was turned off".into()));
        }
        let enter = w.and_then(|w| w.bool("enter")) == Some(true);
        // a trailing space keeps the next dictation from running into this one
        let typed = if enter { line } else { format!("{line} ") };
        // The typing runs on a blocking thread that holds the gate until it is done, even if the phone gives up (the
        // request future is dropped): the next text never types into the middle of this one. A dropped request never
        // reaches the Enter below: that would send or run what the caller gave up on.
        let type_text = self.type_text.clone();
        let Ok((result, permit)) =
            tokio::task::spawn_blocking(move || (type_text(&typed), permit)).await
        else {
            return failed();
        };
        match result {
            Err(_) => return failed(),
            Ok(false) => return (500, Some("the computer refused the typing".into())),
            Ok(true) => {}
        }
        if enter {
            let send_combo = self.send_combo.clone();
            let Ok((result, _permit)) =
                tokio::task::spawn_blocking(move || (send_combo(&["enter".to_string()]), permit))
                    .await
            else {
                return failed();
            };
            match result {
                Err(_) => return failed(),
                Ok(false) => return (500, Some("the computer refused the Enter key".into())),
                Ok(true) => {}
            }
        }
        (200, None)
    }
}

/// A typing error: logged without its message (the key sender's errors can quote the script that held the text),
/// answered generically.
fn failed() -> (u16, Option<String>) {
    crate::log::warn(
        "PhoneBox.Agent.Dictation",
        "dictation: typing failed (error)",
    );
    (500, Some("the text could not be typed".into()))
}

/// A dictation that records instead of typing (for the route tests).
#[cfg(test)]
pub(crate) fn test_dictation(cfg: Arc<ConfigStore>, type_text: TypeFn) -> Arc<Dictation> {
    Arc::new(Dictation {
        type_text,
        send_combo: Arc::new(|_| Ok(true)),
        focused: Arc::new(|| None),
        ..Dictation::plain(cfg)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fixture {
        dir: tempfile::TempDir,
        typed: Arc<Mutex<Vec<String>>>,
        keys: Arc<Mutex<Vec<String>>>,
        focused: Arc<Mutex<Option<FocusedApp>>>,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                dir: tempfile::tempdir().unwrap(),
                typed: Default::default(),
                keys: Default::default(),
                focused: Arc::new(Mutex::new(Some(FocusedApp {
                    process: "notepad".into(),
                    elevated: false,
                    window: 0,
                }))),
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("config.json")
        }

        fn make(&self, control: bool, enter: bool) -> Dictation {
            let json = serde_json::json!({"pages": [{"id": "p", "title": "P", "widgets": [
                {"id": "d", "type": "dictate", "x": 0, "y": 0, "w": 4, "h": 3,
                 "options": {"control": control, "enter": enter}},
                {"id": "off", "type": "dictate", "x": 4, "y": 0, "w": 4, "h": 3,
                 "options": {"control": false}}
            ]}]});
            std::fs::write(self.path(), serde_json::to_string_pretty(&json).unwrap()).unwrap();
            let (typed, keys, focused) =
                (self.typed.clone(), self.keys.clone(), self.focused.clone());
            Dictation {
                type_text: Arc::new(move |t| {
                    typed.lock().unwrap().push(t.to_string());
                    Ok(true)
                }),
                send_combo: Arc::new(move |c| {
                    keys.lock().unwrap().push(c.join("+"));
                    Ok(true)
                }),
                focused: Arc::new(move || focused.lock().unwrap().clone()),
                check_elevation: true,
                agent_elevated: false,
                ..Dictation::plain(Arc::new(ConfigStore::new(self.path())))
            }
        }

        fn typed(&self) -> Vec<String> {
            self.typed.lock().unwrap().clone()
        }
        fn keys(&self) -> Vec<String> {
            self.keys.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn the_text_is_typed_with_a_space_after_it() {
        let f = Fixture::new();
        let d = Arc::new(f.make(true, false));
        assert_eq!(
            d.type_async(None, Some("p/d"), Some("hello world")).await,
            (200, None)
        );
        assert_eq!(f.typed(), ["hello world "]);
        assert!(f.keys().is_empty());
    }

    #[tokio::test]
    async fn line_breaks_never_become_enter() {
        // a newline would be Enter: it sends a chat message or runs a command
        let f = Fixture::new();
        let d = Arc::new(f.make(true, false));
        d.type_async(
            None,
            Some("p/d"),
            Some("rm -rf x\nreally\r\n\tnow\u{2028}done"),
        )
        .await;
        assert_eq!(f.typed(), ["rm -rf x really now done "]);
        assert!(f.keys().is_empty());
    }

    #[tokio::test]
    async fn enter_only_with_the_widgets_option() {
        let f = Fixture::new();
        let d = Arc::new(f.make(true, true));
        d.type_async(None, Some("p/d"), Some("ship it")).await;
        assert_eq!(f.typed(), ["ship it"]);
        assert_eq!(f.keys(), ["enter"]);
    }

    #[tokio::test]
    async fn nothing_is_typed_without_the_widgets_own_switch() {
        let f = Fixture::new();
        let d = Arc::new(f.make(true, false));
        assert_eq!(d.type_async(None, Some("p/off"), Some("x")).await.0, 403);
        assert_eq!(d.type_async(None, None, Some("x")).await.0, 403);
        assert_eq!(
            d.type_async(None, Some("p/missing"), Some("x")).await.0,
            403
        );
        let off = Arc::new(f.make(false, false));
        assert_eq!(off.type_async(None, Some("p/d"), Some("x")).await.0, 403);
        assert!(f.typed().is_empty());
    }

    #[tokio::test]
    async fn empty_text_and_elevated_apps_are_refused() {
        let f = Fixture::new();
        let d = Arc::new(f.make(true, false));
        assert_eq!(d.type_async(None, Some("p/d"), Some(" \n\t ")).await.0, 400);
        *f.focused.lock().unwrap() = Some(FocusedApp {
            process: "regedit".into(),
            elevated: true,
            window: 0,
        });
        let (status, error) = d.type_async(None, Some("p/d"), Some("x")).await;
        assert_eq!(status, 409);
        assert!(error.unwrap().contains("administrator"));
        assert!(f.typed().is_empty());
    }

    /// The phone gave up while the text was typed (the request future is dropped): Enter is never pressed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_request_never_presses_enter() {
        let f = Fixture::new();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let typed = f.typed.clone();
        let d = Arc::new(Dictation {
            type_text: Arc::new(move |t| {
                typed.lock().unwrap().push(t.to_string());
                let _ = started_tx.send(());
                let _ = release_rx.lock().unwrap().recv();
                Ok(true)
            }),
            ..f.make(true, true)
        });
        let running = {
            let d = d.clone();
            tokio::spawn(async move { d.type_async(None, Some("p/d"), Some("rm -rf build")).await })
        };
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        // the typing finishes and gives the gate back; the next text gets it, and Enter for it alone
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(f.typed(), ["rm -rf build"]);
        assert!(f.keys().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_switch_is_read_again_after_waiting_for_the_text_before() {
        let f = Fixture::new();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let typed = f.typed.clone();
        let d = Arc::new(Dictation {
            type_text: Arc::new(move |t| {
                typed.lock().unwrap().push(t.to_string());
                let _ = started_tx.send(());
                let _ = release_rx.lock().unwrap().recv();
                Ok(true)
            }),
            ..f.make(true, false)
        });
        let running = {
            let d = d.clone();
            tokio::spawn(async move { d.type_async(None, Some("p/d"), Some("one")).await })
        };
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        let waiting = {
            let d = d.clone();
            tokio::spawn(async move { d.type_async(None, Some("p/d"), Some("two")).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // turned off while the second text waits for the first
        let text = std::fs::read_to_string(f.path()).unwrap();
        assert!(text.contains("\"control\": true"));
        std::fs::write(
            f.path(),
            text.replace("\"control\": true", "\"control\": false"),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(f.path())
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(running.await.unwrap(), (200, None));
        assert_eq!(waiting.await.unwrap().0, 403);
        assert_eq!(f.typed(), ["one "]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_text_that_waits_more_than_two_seconds_is_refused() {
        let f = Fixture::new();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let d = Arc::new(Dictation {
            type_text: Arc::new(move |_| {
                let _ = release_rx.lock().unwrap().recv();
                Ok(true)
            }),
            ..f.make(true, false)
        });
        let running = {
            let d = d.clone();
            tokio::spawn(async move { d.type_async(None, Some("p/d"), Some("one")).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (status, error) = d.type_async(None, Some("p/d"), Some("two")).await;
        assert_eq!(
            (status, error.as_deref()),
            (429, Some("still typing the previous text"))
        );
        release_tx.send(()).unwrap();
        assert_eq!(running.await.unwrap().0, 200);
    }

    #[tokio::test]
    async fn long_text_is_cut() {
        let f = Fixture::new();
        let d = Arc::new(f.make(true, false));
        d.type_async(None, Some("p/d"), Some(&"a".repeat(MAX_CHARS + 500)))
            .await;
        assert_eq!(f.typed()[0].len(), MAX_CHARS + 1); // plus the trailing space
        assert_eq!(
            d.type_async(None, Some("p/d"), Some(&"a".repeat(MAX_CHARS * 3)))
                .await
                .0,
            413
        );
    }

    #[tokio::test]
    async fn a_refused_typing_is_reported() {
        let f = Fixture::new();
        let d = Arc::new(Dictation {
            type_text: Arc::new(|_| Ok(false)),
            ..f.make(true, false)
        });
        assert_eq!(
            d.type_async(None, Some("p/d"), Some("x")).await,
            (500, Some("the computer refused the typing".into()))
        );
        let failing = Arc::new(Dictation {
            type_text: Arc::new(|_| Err("osascript said no".into())),
            ..f.make(true, false)
        });
        assert_eq!(
            failing.type_async(None, Some("p/d"), Some("x")).await,
            (500, Some("the text could not be typed".into()))
        );
        let no_enter = Arc::new(Dictation {
            send_combo: Arc::new(|_| Ok(false)),
            ..f.make(true, true)
        });
        assert_eq!(
            no_enter.type_async(None, Some("p/d"), Some("x")).await,
            (500, Some("the computer refused the Enter key".into()))
        );
    }

    #[test]
    fn cleaning_keeps_one_line_of_at_most_a_thousand_units() {
        assert_eq!(clean(None), "");
        assert_eq!(clean(Some("  a \u{7f} b\u{85}c\u{2029}  ")), "a b c");
        // only spaces collapse; other white space stays as it is
        assert_eq!(clean(Some("a\u{a0}\u{a0}b")), "a\u{a0}\u{a0}b");
        let long = format!("{}😀", "a".repeat(MAX_CHARS - 1));
        assert_eq!(clean(Some(&long)).encode_utf16().count(), MAX_CHARS - 1); // half an emoji is not kept
        assert_eq!(
            clean(Some(&"ab ".repeat(600))).encode_utf16().count(),
            MAX_CHARS
        );
    }
}
