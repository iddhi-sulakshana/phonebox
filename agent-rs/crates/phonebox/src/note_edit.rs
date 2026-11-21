//! `NoteEdit`: the phone's only write path into the settings — the text of a Note widget.

use crate::config::store::{CONFLICT, ConfigStore};

/// Longest note text, in UTF-16 units.
pub const MAX_LENGTH: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteResult {
    pub status: u16,
    pub error: Option<String>,
    /// The saved config's version (ticks, as text).
    pub version: Option<String>,
}

/// Sets `pageId`'s top-level widget `widgetId` (a note) to `text`, saving against `expected` (else the current
/// version). The settings save's side effects (deck, photos, cameras, icons) do not run: only a note changed.
pub fn apply(
    cfg: &ConfigStore,
    page_id: &str,
    widget_id: &str,
    text: Option<&str>,
    expected: Option<i64>,
) -> NoteResult {
    let fail = |status: u16, error: &str| NoteResult {
        status,
        error: Some(error.to_string()),
        version: None,
    };
    let Some(text) = text else {
        return fail(400, "text expected");
    };
    if text.encode_utf16().count() > MAX_LENGTH {
        return fail(400, "text too long");
    }
    let version = expected.unwrap_or_else(|| cfg.version());
    // a copy of the config with the one note changed (the top-level widget: Smart Stack cards are not reachable)
    let mut copy = (*cfg.get()).clone();
    let Some(widget) = copy
        .pages
        .iter_mut()
        .find(|p| p.id == page_id)
        .and_then(|p| p.widgets.as_mut())
        .and_then(|ws| ws.iter_mut().find(|w| w.id == widget_id))
    else {
        return fail(404, "widget not found");
    };
    if widget.r#type != "note" {
        return fail(400, "option not allowed");
    }
    widget.set("text", serde_json::Value::String(text.to_string()));
    match cfg.save(&crate::config::write::to_dotnet_json(&copy), Some(version)) {
        Ok(saved) => NoteResult {
            status: 200,
            error: None,
            version: Some(saved.to_string()),
        },
        Err(e) if e == CONFLICT => fail(409, CONFLICT),
        Err(e) => fail(400, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"pages":[{"id":"p","title":"P","widgets":[{"id":"n","type":"note","options":{"text":"old"}},{"id":"c","type":"clock"}]}]}"#,
        )
        .unwrap();
        (dir, ConfigStore::new(path))
    }

    fn note_text(cfg: &ConfigStore) -> Option<String> {
        let c = cfg.get();
        let page = c.pages.iter().find(|p| p.id == "p").unwrap();
        let w = page
            .widgets
            .as_ref()
            .unwrap()
            .iter()
            .find(|w| w.id == "n")
            .unwrap();
        w.str("text").map(String::from)
    }

    #[test]
    fn sets_the_text_of_a_note() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "n", Some("hello"), None);
        assert_eq!(r.status, 200);
        assert_eq!(note_text(&cfg).as_deref(), Some("hello"));
        assert_eq!(r.version, Some(cfg.version().to_string()));
    }

    #[test]
    fn an_empty_text_clears_the_note() {
        let (_d, cfg) = make();
        assert_eq!(apply(&cfg, "p", "n", Some(""), None).status, 200);
        assert_eq!(note_text(&cfg).as_deref(), Some(""));
    }

    #[test]
    fn a_missing_text_is_refused() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "n", None, None);
        assert_eq!((r.status, r.error.as_deref()), (400, Some("text expected")));
    }

    #[test]
    fn refuses_text_over_the_limit() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "n", Some(&"x".repeat(MAX_LENGTH + 1)), None);
        assert_eq!((r.status, r.error.as_deref()), (400, Some("text too long")));
        // UTF-16 units: 250 astral characters are 500 units
        assert_eq!(
            apply(&cfg, "p", "n", Some(&"😀".repeat(250)), None).status,
            200
        );
        assert_eq!(
            apply(&cfg, "p", "n", Some(&"😀".repeat(251)), None).status,
            400
        );
    }

    #[test]
    fn unknown_widget_is_not_found() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "missing", Some("hello"), None);
        assert_eq!(
            (r.status, r.error.as_deref()),
            (404, Some("widget not found"))
        );
        assert_eq!(apply(&cfg, "nope", "n", Some("hello"), None).status, 404);
    }

    #[test]
    fn other_widget_types_are_refused() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "c", Some("hello"), None);
        assert_eq!(
            (r.status, r.error.as_deref()),
            (400, Some("option not allowed"))
        );
    }

    #[test]
    fn a_stale_version_is_a_conflict() {
        let (_d, cfg) = make();
        let r = apply(&cfg, "p", "n", Some("hello"), Some(-1));
        assert_eq!((r.status, r.error.as_deref()), (409, Some(CONFLICT)));
        assert_eq!(note_text(&cfg).as_deref(), Some("old"));
    }
}
