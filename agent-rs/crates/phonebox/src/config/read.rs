//! Reading `config.json` as the C# agent does (`ConfigStore.ReadOpts`): property names in any case, `//` and `/* */`
//! comments, trailing commas.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Map, Value};

use super::{AgentConfig, KNOWN_FIELDS, OPAQUE_FIELDS, VALUE_FIELDS};

static KNOWN: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    KNOWN_FIELDS
        .iter()
        .map(|name| (name.to_ascii_lowercase(), *name))
        .collect()
});

static VALUE: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| VALUE_FIELDS.iter().copied().collect());

/// Parses settings text leniently (the file on disk): a null anywhere under a known property is the model's default.
/// Errors read `Invalid JSON: …`, as `ConfigStore.Save` reports them.
pub fn parse(text: &str) -> Result<AgentConfig, String> {
    from_value(parse_value(text)?)
}

/// Parses settings sent to be saved, as strictly as C# deserializes them: a null for a number or a switch is refused
/// (an emptied number field arrives as null; it must not save the default in its place).
pub fn parse_for_save(text: &str) -> Result<AgentConfig, String> {
    from_value(canonicalize(parse_raw(text)?, true, "$")?)
}

fn from_value(value: Value) -> Result<AgentConfig, String> {
    if !value.is_object() {
        return Err("Invalid JSON: the settings must be a JSON object".to_string());
    }
    serde_json::from_value(value).map_err(|e| format!("Invalid JSON: {e}"))
}

/// Reads JSON text with comments and trailing commas into a value whose known property names are in their canonical
/// camelCase, with nulls dropped where the model has a default (as a missing property).
pub fn parse_value(text: &str) -> Result<Value, String> {
    canonicalize(parse_raw(text)?, false, "$")
}

fn parse_raw(text: &str) -> Result<Value, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // File.ReadAllText drops a UTF-8 byte order mark
    serde_json::from_str(&strip_comments_and_trailing_commas(text))
        .map_err(|e| format!("Invalid JSON: {e}"))
}

/// `JsonCommentHandling.Skip` + `AllowTrailingCommas`: `//` and `/* */` comments become spaces (so error positions keep
/// their lines), and a comma followed only by white space or comments before `}` or `]` is dropped. Text inside strings
/// is left alone.
pub fn strip_comments_and_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut pending_comma: Option<usize> = None; // index in `out` of a comma that may turn out to be trailing
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '"' => {
                pending_comma = None;
                out.push(ch);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if chars[i - 1] == '"' {
                        break;
                    }
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                out.extend([' ', ' ']);
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.extend([' ', ' ']);
                    i += 2;
                }
            }
            ',' => {
                pending_comma = Some(out.len());
                out.push(ch);
                i += 1;
            }
            '}' | ']' => {
                if let Some(at) = pending_comma.take() {
                    out[at] = ' ';
                }
                out.push(ch);
                i += 1;
            }
            c if c.is_whitespace() => {
                out.push(c);
                i += 1;
            }
            c => {
                pending_comma = None;
                out.push(c);
                i += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// Known property names in any case become their camelCase spelling; a null under a known property is dropped (the
/// model's default applies, as C# `Normalize` would fill it), and so are null items of arrays the model lists (C#
/// filters them). Free-form dictionaries (`options`, `defaults`) and properties the model does not know are kept as
/// written. `strict` (a save): a null for a number or a switch is an error, worded as C# reports it; `path` is the
/// JSON path of `value` for that message.
fn canonicalize(value: Value, strict: bool, path: &str) -> Result<Value, String> {
    Ok(match value {
        Value::Object(map) => Value::Object(canonical_object(map, strict, path)?),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .filter(|(_, v)| !v.is_null())
                .map(|(i, v)| canonicalize(v, strict, &format!("{path}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        other => other,
    })
}

fn canonical_object(
    map: Map<String, Value>,
    strict: bool,
    path: &str,
) -> Result<Map<String, Value>, String> {
    let mut out = Map::with_capacity(map.len());
    for (key, value) in map {
        match KNOWN.get(&key.to_ascii_lowercase()) {
            Some(&name) => {
                let here = format!("{path}.{key}");
                if value.is_null() {
                    match VALUE.get(name) {
                        // C# has no null for a bool, int or double: System.Text.Json refuses it (without the line
                        // and byte position .NET adds)
                        Some(net) if strict => {
                            return Err(format!(
                                "Invalid JSON: The JSON value could not be converted to {net}. Path: {here}."
                            ));
                        }
                        _ => continue,
                    }
                }
                let value = if OPAQUE_FIELDS.contains(&name) {
                    value
                } else {
                    canonicalize(value, strict, &here)?
                };
                out.insert(name.to_string(), value);
            }
            None => {
                out.insert(key, value);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_trailing_commas_go_but_strings_stay() {
        let v =
            parse_value("{ /* a, } */ \"a\": [1, 2, ], // x\n \"b\": \"// not, a comment ]\", }")
                .unwrap();
        assert_eq!(v["a"], serde_json::json!([1, 2]));
        assert_eq!(v["b"], "// not, a comment ]");
    }

    #[test]
    fn surrogate_pair_escapes_and_a_bom_are_read() {
        let v = parse_value("\u{feff}{\"icon\": \"\\uD83C\\uDFA7\"}").unwrap();
        assert_eq!(v["icon"], "🎧");
    }
}
