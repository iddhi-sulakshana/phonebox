//! Port of `StackWidgets.cs`: the cards of a Smart Stack widget, and the options a widget may show the phone.

use serde_json::{Map, Value};

use crate::config::WidgetConfig;

/// One card of a Smart Stack.
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub id: String,
    pub r#type: String,
    pub enabled: bool,
    pub options: Option<Map<String, Value>>,
}

/// The cards of a `stack` widget: at most 12, never a nested stack or a command widget, only known widget types.
pub fn children(parent: &WidgetConfig) -> Vec<Child> {
    if parent.r#type != "stack" {
        return vec![];
    }
    let Some(raw) = parent.opt("widgets") else {
        return vec![];
    };
    let parsed;
    let raw = match raw {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => return vec![],
        },
        other => other,
    };
    let Value::Array(items) = raw else {
        return vec![];
    };
    items
        .iter()
        .take(12)
        .filter_map(|x| {
            let obj = x.as_object()?;
            let ty = obj.get("type")?.as_str()?;
            if ty == "stack" || ty == "command" || !crate::catalog::is_widget(ty) {
                return None;
            }
            Some((obj, ty))
        })
        .enumerate()
        .map(|(index, (obj, ty))| Child {
            id: obj
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("card-{index}")),
            r#type: ty.to_string(),
            enabled: obj.get("enabled") != Some(&Value::Bool(false))
                && obj.get("active") != Some(&Value::Bool(false)),
            options: obj.get("options").and_then(Value::as_object).cloned(),
        })
        .collect()
}

/// The options the phone may see: a command widget's scripts stay on this computer, also inside a Smart Stack.
pub fn public_options(w: &WidgetConfig) -> Option<Map<String, Value>> {
    if w.r#type == "command" {
        return w.options.as_ref().map(|o| {
            o.iter()
                .filter(|(k, _)| *k == "title" || *k == "interval")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        });
    }
    let options = w.options.as_ref()?;
    if w.r#type != "stack" {
        return Some(options.clone());
    }
    let mut options = options.clone();
    if options.contains_key("widgets") {
        let cards: Vec<Value> = children(w)
            .into_iter()
            .map(|c| {
                serde_json::json!({ "id": c.id, "type": c.r#type, "enabled": c.enabled, "options": c.options.map(Value::Object).unwrap_or(Value::Null) })
            })
            .collect();
        options.insert("widgets".into(), Value::Array(cards));
    }
    Some(options)
}
