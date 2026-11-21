//! `AppConsoleTests` (18) and `AppConsoleFocusRaceTests` (2): keys go only to the app a profile is for, and only when
//! a widget allows it; an edit or a focus change mid-macro stops the rest.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use super::*;

const PROFILES: &str = r#"[{ "id": "ps", "label": "Photoshop", "apps": "photoshop.exe, gimp",
           "keys": [{ "label": "Undo", "keys": "ctrl+z" }, { "label": "Save both", "keys": "ctrl+s, ctrl+shift+s" }],
           "dialLeft": "minus", "dialRight": "equals" }]"#;

fn app(process: &str, elevated: bool) -> FocusedApp {
    FocusedApp {
        process: process.into(),
        elevated,
        window: 0,
    }
}

fn windowed(process: &str, window: isize) -> FocusedApp {
    FocusedApp {
        window,
        ..app(process, false)
    }
}

/// A settings file, the keys sent and the app in front.
struct Fx {
    dir: tempfile::TempDir,
    sent: Arc<Mutex<Vec<String>>>,
    focused: Arc<Mutex<Option<FocusedApp>>>,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            sent: Default::default(),
            focused: Arc::new(Mutex::new(Some(app("Photoshop", false)))),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("config.json")
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    fn set_focused(&self, focused: Option<FocusedApp>) {
        *self.focused.lock().unwrap() = focused;
    }

    /// A console over a fresh settings file, with the test's focused app and a recording key sender.
    fn make_with(&self, settings: String) -> AppConsole {
        std::fs::write(self.path(), settings).unwrap();
        let mut console = AppConsole::new(Arc::new(ConfigStore::new(self.path())));
        console.supported = true;
        console.agent_elevated = false;
        let focused = self.focused.clone();
        console.focused = Arc::new(move || focused.lock().unwrap().clone());
        console.foreground = Arc::new(|| 0);
        console.send_combo = self.recorder(|_| {});
        console
    }

    fn make(&self, control: bool, page_enabled: bool) -> AppConsole {
        self.make_with(format!(
            r#"{{ "pages": [ {{ "id": "p", "title": "P", "enabled": {page_enabled},
                "widgets": [ {{ "id": "c", "type": "app-console", "x": 0, "y": 0, "w": 6, "h": 4, "options": {{ "control": {control} }} }} ] }},
                {{ "id": "q", "title": "Q", "enabled": true, "widgets": [
                  {{ "id": "c", "type": "app-console", "x": 0, "y": 0, "w": 6, "h": 4, "options": {{ "control": false }} }},
                  {{ "id": "s", "type": "stack", "x": 6, "y": 0, "w": 6, "h": 4, "options": {{ "widgets": [
                    {{ "id": "on", "type": "app-console", "options": {{ "control": true }} }},
                    {{ "id": "off", "type": "app-console", "enabled": false, "options": {{ "control": true }} }} ] }} }} ] }} ],
              "console": {{ "profiles": {PROFILES} }} }}"#
        ))
    }

    fn console(&self) -> AppConsole {
        self.make(true, true)
    }

    /// A key sender that records the combo, then runs `after` with how many went out so far.
    fn recorder(&self, after: impl Fn(usize) + Send + Sync + 'static) -> SendCombo {
        let sent = self.sent.clone();
        Arc::new(move |combo| {
            let count = {
                let mut sent = sent.lock().unwrap();
                sent.push(combo.join("+"));
                sent.len()
            };
            after(count);
            true
        })
    }

    /// Sets the file's write time (the store reads a file again when it changes).
    fn stamp(&self, at: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(self.path())
            .unwrap()
            .set_modified(at)
            .unwrap();
    }
}

fn year_2024() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_704_067_200)
}

async fn act(
    c: &AppConsole,
    widget: Option<&str>,
    profile: &str,
    slot: &str,
    times: i32,
) -> (u16, Option<String>) {
    c.act(None, widget, profile, slot, times, &Stop::default())
        .await
        .expect("not cancelled")
}

async fn status(
    c: &AppConsole,
    widget: Option<&str>,
    profile: &str,
    slot: &str,
    times: i32,
) -> u16 {
    act(c, widget, profile, slot, times).await.0
}

#[test]
fn the_state_names_the_profile_but_never_the_app() {
    let fx = Fx::new();
    let (status, body) = fx.console().state(None, Some("p/c"));
    assert_eq!(status, 200);
    let json = body.to_string();
    // C# asserts its default serializer's "Id"; the phone gets camelCase from the route
    assert!(json.contains(r#""id":"ps""#), "{json}");
    assert!(json.contains("Undo"));
    assert!(!json.to_lowercase().contains("ctrl"));
    assert!(!json.to_lowercase().contains("photoshop.exe"));

    fx.set_focused(Some(app("notepad", false)));
    assert_eq!(
        fx.console().state(None, Some("p/c")).1["profile"],
        Value::Null
    );
}

#[tokio::test]
async fn keys_go_to_the_matching_app() {
    let fx = Fx::new();
    let console = fx.console();
    assert_eq!(act(&console, Some("p/c"), "ps", "k1", 1).await, (200, None));
    assert_eq!(fx.sent(), ["ctrl+s", "ctrl+shift+s"]);
    fx.set_focused(Some(app("GIMP", false))); // names match without case and without .exe
    assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 200);
}

#[tokio::test]
async fn nothing_is_sent_without_a_widget_that_allows_it() {
    let fx = Fx::new();
    assert_eq!(
        status(&fx.make(false, true), Some("p/c"), "ps", "k0", 1).await,
        403
    );
    assert_eq!(
        status(&fx.make(true, false), Some("p/c"), "ps", "k0", 1).await,
        403
    );
    assert_eq!(fx.make(true, false).state(None, Some("p/c")).0, 404);
    assert!(fx.sent().is_empty());
}

#[tokio::test]
async fn each_widgets_own_switch_counts() {
    let fx = Fx::new();
    let console = fx.console();
    // page q has a console with sending off: page p's switch doesn't open it
    assert_eq!(status(&console, Some("q/c"), "ps", "k0", 1).await, 403);
    assert_eq!(console.state(None, Some("q/c")).1["control"], json!(false));
    // a Smart Stack card is named by the stack and the card; a hidden card is not there at all
    assert_eq!(status(&console, Some("q/s:on"), "ps", "k0", 1).await, 200);
    assert_eq!(status(&console, Some("q/s:off"), "ps", "k0", 1).await, 403);
    assert_eq!(console.state(None, Some("q/s:off")).0, 404);
    assert_eq!(status(&console, None, "ps", "k0", 1).await, 403);
    assert_eq!(status(&console, Some("p/nope"), "ps", "k0", 1).await, 403);
    assert_eq!(fx.sent().len(), 1);
}

#[tokio::test]
async fn turning_the_switch_off_mid_macro_stops_the_rest() {
    let fx = Fx::new();
    let mut console = fx.console();
    let path = fx.path();
    // the user turns "Send keys" off right after the first combo went out
    console.send_combo = fx.recorder(move |_| {
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace(r#""control": true"#, r#""control": false"#),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
    });
    assert_eq!(status(&console, Some("p/c"), "ps", "k1", 1).await, 403);
    assert_eq!(fx.sent(), ["ctrl+s"]);
}

#[tokio::test]
async fn an_in_progress_macro_uses_the_current_profile_contract() {
    for (edit, expected, count) in [
        ("rename", 200, 2),
        ("binding", 409, 1),
        ("remove", 409, 1),
        ("app", 409, 1),
    ] {
        let fx = Fx::new();
        let mut console = fx.console();
        fx.stamp(year_2024());
        let path = fx.path();
        console.send_combo = fx.recorder(move |count| {
            if count != 1 {
                return;
            }
            let before = std::fs::read_to_string(&path).unwrap();
            let after = match edit {
                "rename" => before.replace(r#""label": "Photoshop""#, r#""label": "Editor""#),
                "binding" => before.replace("ctrl+s, ctrl+shift+s", "F5"),
                "remove" => before.replace(PROFILES, "[]"),
                _ => before.replace("photoshop.exe, gimp", "notepad"),
            };
            ConfigStore::new(path.clone()).save(&after, None).unwrap();
        });
        let actual = status(&console, Some("p/c"), "ps", "k1", 1).await;
        assert!(
            actual == expected && fx.sent().len() == count,
            "{edit}: EXPECTED: status={expected}, sent={count} | ACTUAL: status={actual}, sent={}",
            fx.sent().len()
        );
    }
}

#[tokio::test]
async fn cancellation_during_focus_checks_does_not_send_a_combo() {
    for cancel_at in ["none", "focused", "foreground"] {
        let fx = Fx::new();
        let mut console = fx.console();
        let stop = Stop::default();
        console.focused = {
            let stop = stop.clone();
            Arc::new(move || {
                if cancel_at == "focused" {
                    stop.stop();
                }
                Some(windowed("photoshop", 42))
            })
        };
        console.foreground = {
            let stop = stop.clone();
            Arc::new(move || {
                if cancel_at == "foreground" {
                    stop.stop();
                }
                42
            })
        };
        let result = console.act(None, Some("p/c"), "ps", "k0", 1, &stop).await;
        if cancel_at == "none" {
            assert_eq!(result, Ok((200, None)));
            assert_eq!(fx.sent(), ["ctrl+z"]);
            continue;
        }
        assert!(
            result == Err(Cancelled) && fx.sent().is_empty(),
            "{cancel_at}: EXPECTED: cancelled, sent=0 | ACTUAL: {result:?}, sent={}",
            fx.sent().len()
        );
        let focused = fx.focused.clone();
        console.focused = Arc::new(move || focused.lock().unwrap().clone());
        assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 200);
        assert_eq!(fx.sent(), ["ctrl+z"]);
    }
}

#[tokio::test]
async fn a_window_change_right_before_sending_stops_it() {
    let fx = Fx::new();
    let mut console = fx.console();
    console.focused = Arc::new(|| Some(windowed("photoshop", 42)));
    console.foreground = Arc::new(|| 43); // another window came to the front after the checks
    assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 409);
    console.foreground = Arc::new(|| 42);
    assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 200);
    assert_eq!(fx.sent().len(), 1);
}

#[tokio::test]
async fn another_focused_app_gets_nothing() {
    let fx = Fx::new();
    fx.set_focused(Some(app("notepad", false)));
    assert_eq!(status(&fx.console(), Some("p/c"), "ps", "k0", 1).await, 409);
    fx.set_focused(None);
    assert_eq!(status(&fx.console(), Some("p/c"), "ps", "k0", 1).await, 409);
    assert!(fx.sent().is_empty());
}

#[tokio::test]
async fn a_focus_change_mid_macro_stops_the_rest() {
    let fx = Fx::new();
    let mut console = fx.console();
    let calls = Arc::new(Mutex::new(0));
    console.focused = Arc::new(move || {
        let mut calls = calls.lock().unwrap();
        *calls += 1;
        Some(app(
            if *calls == 1 { "photoshop" } else { "explorer" },
            false,
        ))
    });
    assert_eq!(status(&console, Some("p/c"), "ps", "k1", 1).await, 409);
    assert_eq!(fx.sent(), ["ctrl+s"]);
}

#[tokio::test]
async fn an_elevated_app_is_refused_with_a_reason() {
    let fx = Fx::new();
    fx.set_focused(Some(app("Photoshop", true)));
    let (status, error) = act(&fx.console(), Some("p/c"), "ps", "k0", 1).await;
    assert_eq!(status, 409);
    assert!(error.unwrap().contains("administrator"));
    assert!(fx.sent().is_empty());
}

#[tokio::test]
async fn unknown_profiles_and_slots_never_fall_back() {
    let fx = Fx::new();
    let console = fx.console();
    assert_eq!(status(&console, Some("p/c"), "other", "k0", 1).await, 404);
    assert_eq!(status(&console, Some("p/c"), "ps", "k7", 1).await, 404);
    assert_eq!(status(&console, Some("p/c"), "ps", "k01", 1).await, 404);
    assert_eq!(
        status(&console, Some("p/c"), "ps", "dial-press", 1).await,
        404
    ); // not set
    assert_eq!(status(&console, Some("p/c"), "ps", "ctrl+z", 1).await, 404);
    assert!(fx.sent().is_empty());
}

#[tokio::test]
async fn dial_turns_are_clamped() {
    let fx = Fx::new();
    let console = fx.console();
    act(&console, Some("p/c"), "ps", "dial-right", 50).await;
    assert_eq!(fx.sent().len(), 5);
    fx.sent.lock().unwrap().clear();
    act(&console, Some("p/c"), "ps", "k0", 9).await; // a key is pressed once whatever the phone asks
    assert_eq!(fx.sent().len(), 1);
}

#[tokio::test]
async fn an_overlong_binding_from_an_older_file_is_not_sent() {
    let fx = Fx::new();
    let console = fx.console();
    // written by hand (or before the limit): normalize keeps it, act refuses it before any key goes out
    let many = vec!["a"; 21].join(",");
    std::fs::write(
        fx.path(),
        fx.read().replace(
            r#""dialLeft": "minus""#,
            &format!(r#""dialLeft": "{many}""#),
        ),
    )
    .unwrap();
    fx.stamp(SystemTime::now() + Duration::from_secs(60));
    assert_eq!(
        status(&console, Some("p/c"), "ps", "dial-left", 1).await,
        400
    );
    assert!(fx.sent().is_empty());
}

#[tokio::test]
async fn a_failed_key_press_is_reported() {
    let fx = Fx::new();
    let mut console = fx.console();
    console.send_combo = Arc::new(|_| false);
    assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 500);
}

#[tokio::test]
async fn unsupported_systems_say_so() {
    let fx = Fx::new();
    let mut console = fx.console();
    console.supported = false;
    assert_eq!(
        console.state(None, Some("p/c")).1["supported"],
        json!(false)
    );
    assert_eq!(status(&console, Some("p/c"), "ps", "k0", 1).await, 409);
}

#[test]
fn saving_checks_the_keys_and_needs_an_app() {
    let fx = Fx::new();
    std::fs::write(fx.path(), "{}").unwrap();
    let store = ConfigStore::new(fx.path());
    let refused = |json: &str| store.save(json, None).unwrap_err();
    assert!(
        refused(r#"{ "console": { "profiles": [{ "label": "X", "apps": "x", "keys": [{ "keys": "ctrl+nope" }] }] } }"#)
            .contains("unknown key")
    );
    assert!(
        refused(r#"{ "console": { "profiles": [{ "label": "X", "apps": " , ", "keys": [{ "keys": "f5" }] }] } }"#)
            .contains("no app")
    );
    // too long is refused, never cut short into another valid key or app name
    let long_keys = vec!["ctrl+shift+f12"; 20].join(", ");
    assert!(
        refused(&format!(
            r#"{{ "console": {{ "profiles": [{{ "label": "X", "apps": "x", "keys": [{{ "keys": "{long_keys}" }}] }}] }} }}"#
        ))
        .contains("longer than")
    );
    let long_apps = "a".repeat(301);
    assert!(
        refused(&format!(
            r#"{{ "console": {{ "profiles": [{{ "label": "X", "apps": "{long_apps}", "keys": [{{ "keys": "f5" }}] }}] }} }}"#
        ))
        .contains("longer than")
    );
    let thirteen = vec![r#"{ "keys": "f5" }"#; 13].join(",");
    assert!(
        refused(&format!(
            r#"{{ "console": {{ "profiles": [{{ "label": "X", "apps": "x", "keys": [{thirteen}] }}] }} }}"#
        ))
        .contains("more than 12 keys")
    );
    let presses = vec!["a"; 21].join(",");
    assert!(
        refused(&format!(
            r#"{{ "console": {{ "profiles": [{{ "label": "X", "apps": "x", "dialLeft": "{presses}" }}] }} }}"#
        ))
        .contains("more than 20 key presses")
    );
    let apps: Vec<String> = (0..31)
        .map(|i| format!(r#"{{ "label": "A{i}", "apps": "a{i}" }}"#))
        .collect();
    assert!(
        refused(&format!(
            r#"{{ "console": {{ "profiles": [{}] }} }}"#,
            apps.join(",")
        ))
        .contains("more than 30 apps")
    );
    store
        .save(
            r#"{ "console": { "profiles": [{ "label": "Edit Pro", "apps": "edit", "keys": [{ "keys": "f5" }], "dialLeft": " " }, { "label": "Edit Pro", "apps": "e2" }] } }"#,
            None,
        )
        .unwrap();
    let cfg = store.get();
    let saved = &cfg.console.profiles;
    assert_eq!(
        saved.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["edit-pro", "edit-pro-2"]
    );
    assert_eq!(saved[0].keys[0].label, "f5"); // an empty label shows the keys
    assert_eq!(saved[0].dial_left, None);
}

#[test]
fn the_keys_never_reach_the_phone_config() {
    let fx = Fx::new();
    fx.console();
    let json = serde_json::to_string(&crate::config::public::to_public(
        &ConfigStore::new(fx.path()).get(),
        None,
        None,
    ))
    .unwrap();
    assert!(!json.contains("ctrl+z"));
    assert!(!json.to_lowercase().contains("photoshop"));
}

// ---- AppConsoleFocusRaceTests: the focused app and the saved binding are checked together

const RACE_PROFILES: &str = r#"[{ "id": "ps", "label": "Photoshop", "apps": "photoshop.exe",
           "keys": [{ "label": "Undo", "keys": "ctrl+z" }, { "label": "Save both", "keys": "ctrl+s, ctrl+shift+s" }] }]"#;

fn race_console(fx: &Fx) -> AppConsole {
    let console = fx.make_with(format!(
        r#"{{ "pages": [ {{ "id": "p", "title": "P", "enabled": true,
                "widgets": [ {{ "id": "c", "type": "app-console", "x": 0, "y": 0, "w": 6, "h": 4, "options": {{ "control": true }} }} ] }} ],
              "console": {{ "profiles": {RACE_PROFILES} }} }}"#
    ));
    fx.stamp(year_2024());
    console
}

fn save_edit(path: &std::path::Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    ConfigStore::new(path.to_path_buf())
        .save(&text.replace(from, to), None)
        .unwrap();
}

#[tokio::test]
async fn an_edit_saved_while_the_focus_is_looked_up_is_honoured() {
    for (edit, expected, count) in [("binding", 409, 0), ("rename", 200, 1)] {
        let fx = Fx::new();
        let mut console = race_console(&fx);
        let (path, saved) = (fx.path(), Mutex::new(false));
        console.focused = Arc::new(move || {
            let mut saved = saved.lock().unwrap();
            if !*saved {
                *saved = true;
                if edit == "binding" {
                    save_edit(&path, "ctrl+z", "F5");
                } else {
                    save_edit(&path, r#""label": "Photoshop""#, r#""label": "Editor""#);
                }
            }
            Some(app("photoshop", false))
        });
        let actual = tokio::time::timeout(
            Duration::from_secs(10),
            act(&console, Some("p/c"), "ps", "k0", 1),
        )
        .await
        .unwrap()
        .0;
        assert!(
            actual == expected && fx.sent().len() == count,
            "{edit}: EXPECTED: status={expected}, sent={count} | ACTUAL: status={actual}, sent={:?}",
            fx.sent()
        );
    }
}

#[tokio::test]
async fn a_macro_is_not_redirected_to_an_app_the_profile_was_moved_to() {
    let fx = Fx::new();
    let mut console = race_console(&fx);
    fx.set_focused(Some(app("photoshop", false)));
    let (path, focused) = (fx.path(), fx.focused.clone());
    console.send_combo = fx.recorder(move |count| {
        if count == 1 {
            // the profile now names Notepad, and the user clicks into Notepad
            save_edit(&path, "photoshop.exe", "notepad");
            *focused.lock().unwrap() = Some(app("notepad", false));
        }
    });
    let actual = tokio::time::timeout(
        Duration::from_secs(10),
        act(&console, Some("p/c"), "ps", "k1", 1),
    )
    .await
    .unwrap();
    assert_eq!(actual.0, 409);
    assert_eq!(fx.sent(), ["ctrl+s"]);
}
