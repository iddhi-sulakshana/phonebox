//! `CoreAuditTests`, `AppearanceTests` and the page-reference `StoreFollowUpTests` check: plan-1 code (the config
//! store, pushed values) whose C# tests were assigned with plan 6.

use crate::config::store::ConfigStore;
use crate::config::write::to_dotnet_json;
use crate::push_store::PushStore;
use std::time::{Duration, Instant};

fn store(dir: &tempfile::TempDir, json: &str) -> ConfigStore {
    let path = dir.path().join("config.json");
    std::fs::write(&path, json).unwrap();
    ConfigStore::new(path)
}

// ---------------------------------------------------------------- C1: unreadable config.json + X-Config-Version

/// config.json is unreadable when the agent starts: the settings page (GET, then PUT with the version it got) must be
/// able to save.
#[test]
fn c1_a_settings_page_can_save_over_a_config_file_that_was_unreadable_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir, r#"{ "night": { "start": "01:00" }, }, oops"#);
    let loaded = s.version(); // GET /api/settings -> X-Config-Version
    let page = to_dotnet_json(&s.get()); // ... and its body
    assert!(s.save(&page, Some(loaded)).is_ok()); // PUT /api/settings
}

/// A hand edit breaks config.json while the agent runs: same dead end, and reloading the page does not help.
#[test]
fn c1_a_settings_page_can_save_after_a_hand_edit_broke_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir, "{}");
    let _ = s.get();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(s.path(), r#"{ "display": { "theme": "amoled", "#).unwrap(); // unfinished edit, saved
    for _reload in 0..3 {
        let loaded = s.version();
        let page = to_dotnet_json(&s.get());
        assert!(s.save(&page, Some(loaded)).is_ok());
    }
}

// ---------------------------------------------------------------- C2: pushed values while something pushes often

#[test]
fn c2_values_reach_data_json_while_a_script_keeps_pushing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data.json");
    {
        let s = PushStore::new(&data);
        s.set("door", serde_json::json!("open"), None).unwrap(); // set once, must survive a restart
        let until = Instant::now() + Duration::from_secs(7);
        let mut i = 0;
        while Instant::now() < until {
            // e.g. a sensor script, once a second
            s.set("cpu-temp", serde_json::json!(40 + i), None).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(
            data.exists(),
            "data.json was never written in 7 s of one push per second"
        );
    }
    let restarted = PushStore::new(&data);
    assert!(restarted.get("door").is_some());
}

// ---------------------------------------------------------------- C4: null entries in the settings JSON

#[test]
fn c4_save_answers_null_entries_with_an_error_message_not_an_exception() {
    for json in [
        r#"{"shortcuts":[null]}"#,
        r#"{"shortcuts":[{"label":"x","cmd":"notepad","steps":[null]}]}"#,
        r#"{"night":{"start":null}}"#,
        r#"{"pages":[{"id":"p","widgets":[{"type":null}]}]}"#,
        r#"{"controls":{"profiles":[{"id":"p","actions":[null]}]}}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let s = ConfigStore::new(dir.path().join("config.json"));
        // an answer either way, never a panic
        let saved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.save(json, None)));
        assert!(saved.is_ok(), "{json}");
    }
}

// ---------------------------------------------------------------- appearance

#[test]
fn existing_layouts_do_not_acquire_appearance_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let config = store(&dir, "{}").get();
    assert!(config.display.appearance.is_none());
    for p in &config.pages {
        assert!(p.appearance.is_none());
        for w in p.widgets.iter().flatten() {
            assert!(w.appearance.is_none());
        }
    }
}

#[test]
fn imported_appearance_values_are_bounded_and_only_known_styles_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let config = store(
        &dir,
        r#"{"display":{"appearance":{"style":"url(private)","font":"url(evil)","fontScale":999,"radius":999,"gap":-5,"padding":99,"pagePadding":99}},
         "pages":[{"id":"p","title":"P","appearance":{"style":"flat","font":"serif","fontScale":10,"border":false},"widgets":[
          {"type":"note","w":6,"h":6,"appearance":{"font":"mono","fontScale":135,"radius":-5,"gap":99,"pagePadding":99,"shadow":false}}]}]}"#,
    )
    .get();
    let global = config.display.appearance.as_ref().unwrap();
    assert_eq!(global.style, None);
    assert_eq!(global.font, None);
    assert_eq!(global.font_scale, Some(200));
    assert_eq!(global.radius, Some(32));
    assert_eq!(global.gap, Some(0));
    assert_eq!(global.padding, Some(24));
    assert_eq!(global.page_padding, Some(32));
    assert_eq!(config.pages.len(), 1);
    let page = &config.pages[0];
    let pa = page.appearance.as_ref().unwrap();
    assert_eq!(pa.style.as_deref(), Some("flat"));
    assert_eq!(pa.font.as_deref(), Some("serif"));
    assert_eq!(pa.font_scale, Some(50));
    assert_eq!(pa.border, Some(false));
    let widgets = page.widgets.as_ref().unwrap();
    assert_eq!(widgets.len(), 1);
    let wa = widgets[0].appearance.as_ref().unwrap();
    assert_eq!(wa.font.as_deref(), Some("mono"));
    assert_eq!(wa.font_scale, Some(135));
    assert_eq!(wa.radius, Some(0));
    assert_eq!(wa.gap, None);
    assert_eq!(wa.page_padding, None);
    assert_eq!(wa.shadow, Some(false));
    let public = crate::config::public::to_public(&config, None, None);
    let p0 = &public["pages"][0];
    assert_eq!(p0["appearance"]["style"], "flat");
    assert_eq!(p0["widgets"][0]["appearance"]["font"], "mono");
    assert_eq!(p0["widgets"][0]["appearance"]["fontScale"], 135);
    assert_eq!(p0["widgets"][0]["appearance"]["radius"], 0);
}

#[test]
fn duplicated_widgets_have_independent_appearance() {
    let original: crate::config::WidgetConfig = serde_json::from_str(
        r#"{"type":"note","appearance":{"style":"flat","font":"mono","fontScale":125,"radius":24}}"#,
    )
    .unwrap();
    let mut clone = original.clone();
    let a = clone.appearance.as_mut().unwrap();
    a.radius = Some(0);
    a.font = Some("serif".into());
    a.font_scale = Some(90);
    let o = original.appearance.as_ref().unwrap();
    assert_eq!(o.radius, Some(24));
    assert_eq!(o.font.as_deref(), Some("mono"));
    assert_eq!(o.font_scale, Some(125));
}

#[test]
fn invalid_appearance_types_do_not_overwrite_settings() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir, "{}");
    for json in [
        r#"{"display":{"appearance":{"radius":"invalid"}}}"#,
        r#"{"display":{"appearance":{"fontScale":"huge"}}}"#,
    ] {
        let error = s.save(json, None).unwrap_err();
        assert!(error.starts_with("Invalid JSON:"), "{error}");
    }
    assert_eq!(std::fs::read_to_string(s.path()).unwrap(), "{}");
}

// ---------------------------------------------------------------- page references

#[test]
fn a_page_reference_with_stray_spaces_finds_its_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let reference = " Desk + Live ";
    let s = ConfigStore::new(path.clone());
    let body = serde_json::json!({
        "pages": [{ "id": "Desk / Live" }, { "id": "Desk + Live" }, { "id": "kitchen" }],
        "display": { "homePage": reference },
        "phones": [{ "id": "desk", "pages": [reference] }],
        "rules": [{ "when": "time", "then": "goto", "page": reference }],
    });
    assert!(s.save(&body.to_string(), None).is_ok());
    for config in [s.get(), ConfigStore::new(path.clone()).get()] {
        let ids: Vec<&str> = config.pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["desk-live", "desk-live-2", "kitchen"]);
        assert_eq!(config.display.home_page.as_deref(), Some("desk-live-2"));
        let desk: Vec<&str> = config
            .pages_for(Some("desk"))
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(desk.join(","), "desk-live-2");
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].page.as_deref(), Some("desk-live-2"));
    }
}
