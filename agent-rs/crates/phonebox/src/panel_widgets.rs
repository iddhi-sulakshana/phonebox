//! `PanelWidgets.Find` (the rest of `PanelWidgets.cs` comes with plan 6): the widget a phone's request names, by
//! `page/widget` (or `page/widget:child` for a card of a Smart Stack), among the enabled pages that phone shows.

use crate::config::{AgentConfig, WidgetConfig};

pub fn find(
    cfg: &AgentConfig,
    device_id: Option<&str>,
    key: Option<&str>,
    kind: &str,
) -> Option<WidgetConfig> {
    let key = key.filter(|k| !k.is_empty())?;
    for page in cfg.pages_for(device_id).into_iter().filter(|p| p.enabled) {
        for w in page.widgets.iter().flatten() {
            let own = format!("{}/{}", page.id, w.id);
            if w.r#type == kind && own == key {
                return Some(w.clone());
            }
            for child in crate::stack_widgets::children(w)
                .into_iter()
                .filter(|c| c.enabled)
            {
                if child.r#type == kind && format!("{own}:{}", child.id) == key {
                    return Some(WidgetConfig {
                        id: child.id,
                        r#type: child.r#type,
                        options: child.options,
                        ..WidgetConfig::default()
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> (tempfile::TempDir, AgentConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json.to_string()).unwrap();
        let cfg = crate::config::store::ConfigStore::new(dir.path().join("config.json")).get();
        (dir, cfg.as_ref().clone())
    }

    #[test]
    fn find_matches_page_widget_and_stack_children_of_enabled_pages() {
        let (_d, cfg) = config(serde_json::json!({
            "pages": [
                {"id": "desk", "title": "Desk", "icon": "x", "enabled": true, "widgets": [
                    {"id": "d1", "type": "dictate", "w": 4, "h": 2, "options": {"control": true}},
                    {"id": "s1", "type": "stack", "w": 4, "h": 2, "options": {"widgets": [
                        {"id": "c1", "type": "dictate", "enabled": true, "options": {"enter": true}},
                        {"id": "c2", "type": "dictate", "enabled": false}
                    ]}}
                ]},
                {"id": "off", "title": "Off", "icon": "x", "enabled": false, "widgets": [
                    {"id": "d2", "type": "dictate", "w": 4, "h": 2}
                ]},
                {"id": "other", "title": "Other", "icon": "x", "enabled": true, "widgets": [
                    {"id": "d3", "type": "dictate", "w": 4, "h": 2}
                ]}
            ],
            "phones": [{"id": "tablet", "name": "Tablet", "pages": ["other"]}]
        }));
        let hit = find(&cfg, None, Some("desk/d1"), "dictate").expect("the widget");
        assert_eq!((hit.id.as_str(), hit.bool("control")), ("d1", Some(true)));
        let child = find(&cfg, None, Some("desk/s1:c1"), "dictate").expect("the stack card");
        assert_eq!(
            (
                child.id.as_str(),
                child.r#type.as_str(),
                child.bool("enter")
            ),
            ("c1", "dictate", Some(true))
        );
        assert!(
            find(&cfg, None, Some("desk/s1:c2"), "dictate").is_none(),
            "a disabled card"
        );
        assert!(
            find(&cfg, None, Some("off/d2"), "dictate").is_none(),
            "a disabled page"
        );
        assert!(
            find(&cfg, None, Some("desk/d1"), "trackpad").is_none(),
            "another type"
        );
        assert!(find(&cfg, None, Some(""), "dictate").is_none());
        assert!(find(&cfg, None, None, "dictate").is_none());
        // a phone with its own pages sees only those
        assert!(find(&cfg, Some("tablet"), Some("desk/d1"), "dictate").is_none());
        assert!(find(&cfg, Some("tablet"), Some("other/d3"), "dictate").is_some());
        assert!(find(&cfg, Some("unknown-phone"), Some("desk/d1"), "dictate").is_some());
    }
}
