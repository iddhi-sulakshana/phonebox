//! Port of `Catalog.cs`: the widget catalogue (`widgets.json`) and the page templates (`templates.json`), embedded from
//! the same files the C# agent embeds.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::config::WidgetConfig;

pub const COLS: i32 = 12;
pub const ROWS: i32 = 6;

pub const WIDGETS_JSON: &str = include_str!("../../../../agent/PhoneBox.Agent/widgets.json");
pub const TEMPLATES_JSON: &str = include_str!("../../../../agent/PhoneBox.Agent/templates.json");

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplateDef {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub desc: String,
    pub widgets: Vec<WidgetConfig>,
}

static TEMPLATES: LazyLock<Vec<TemplateDef>> = LazyLock::new(|| {
    let value = crate::config::read::parse_value(TEMPLATES_JSON).expect("templates.json");
    serde_json::from_value(value).unwrap_or_else(|e| panic!("templates.json: {e}"))
});

static SIZES: LazyLock<HashMap<String, (i32, i32)>> = LazyLock::new(|| {
    let list: Vec<serde_json::Value> = serde_json::from_str(WIDGETS_JSON).expect("widgets.json");
    list.iter()
        .map(|w| {
            let n = |k: &str| w[k].as_i64().expect("widgets.json size") as i32;
            (
                w["type"].as_str().expect("widgets.json type").to_string(),
                (n("minW"), n("minH")),
            )
        })
        .collect()
});

pub fn templates() -> &'static [TemplateDef] {
    &TEMPLATES
}

pub fn is_widget(r#type: &str) -> bool {
    SIZES.contains_key(r#type)
}

/// The smallest size of a widget type, (1, 1) for one the catalogue does not list.
pub fn min_size(r#type: &str) -> (i32, i32) {
    SIZES.get(r#type).copied().unwrap_or((1, 1))
}

pub fn template(id: Option<&str>) -> Option<&'static TemplateDef> {
    let id = id?;
    TEMPLATES.iter().find(|t| t.id == id)
}
