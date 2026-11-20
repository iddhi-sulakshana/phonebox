//! Writing `config.json` as the C# agent does (`ConfigStore.WriteOpts`).

use serde::Serializer;

/// `IconJsonConverter.Write`: icons are written with `JavaScriptEncoder.Default`, which escapes every character outside
/// Basic Latin and the HTML-sensitive ones as `\uXXXX` (upper-case hex, UTF-16 units).
pub fn ser_icon<S: Serializer>(value: &str, s: S) -> Result<S::Ok, S::Error> {
    let raw = serde_json::value::RawValue::from_string(escape_js_default(value))
        .map_err(serde::ser::Error::custom)?;
    serde::Serialize::serialize(&raw, s)
}

/// A JSON string literal (with quotes) escaped like .NET's `JavaScriptEncoder.Default`.
pub fn escape_js_default(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // allowed as is: printable Basic Latin except " & ' + < > ` (escaped by the default encoder)
            ' '..='~' if !matches!(ch, '"' | '&' | '\'' | '+' | '<' | '>' | '`') => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04X}", unit));
                }
            }
        }
    }
    out.push('"');
    out
}

/// A double as .NET writes it (`double.ToString(InvariantCulture)`, also `Utf8JsonWriter`): the shortest digits that
/// round-trip, without a trailing `.0`, in scientific form (`1E-05`, `1E+15`) when the exponent is below -4 or 15 and up.
pub fn format_double(v: f64) -> String {
    let sci = format!("{v:e}"); // shortest round-trip digits: "-1.2345e-7"
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if digits == "0" {
        return format!("{sign}0");
    }
    if !(-4..15).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{dot}E{esign}{:02}", exp.abs());
    }
    if exp >= 0 {
        let int_len = exp as usize + 1;
        let padded = if digits.len() < int_len {
            format!("{digits:0<int_len$}")
        } else {
            digits
        };
        let (int, frac) = padded.split_at(int_len);
        if frac.is_empty() {
            format!("{sign}{int}")
        } else {
            format!("{sign}{int}.{frac}")
        }
    } else {
        format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn doubles_use_the_dotnet_shortest_form() {
        for (v, s) in [
            (1.0, "1"),
            (0.02, "0.02"),
            (59.437, "59.437"),
            (-24.745, "-24.745"),
            (1440.0, "1440"),
            (0.0001, "0.0001"),
            (0.00001, "1E-05"),
            (1.5e-7, "1.5E-07"),
            (1e14, "100000000000000"),
            (1e15, "1E+15"),
            (1.2345678901234568e17, "1.2345678901234568E+17"),
            (0.0, "0"),
            (-0.0, "-0"),
            (6.9271, "6.9271"),
        ] {
            assert_eq!(format_double(v), s, "{v}");
        }
    }

    #[test]
    fn icons_are_escaped_like_the_default_javascript_encoder() {
        assert_eq!(escape_js_default("\u{2328}\u{fe0f}"), "\"\\u2328\\uFE0F\"");
        assert_eq!(escape_js_default("🎧"), "\"\\uD83C\\uDFA7\"");
        assert_eq!(
            escape_js_default("a<b>&'+`\"\\"),
            "\"a\\u003Cb\\u003E\\u0026\\u0027\\u002B\\u0060\\u0022\\\\\""
        );
        assert_eq!(escape_js_default("x"), "\"x\"");
    }
}
