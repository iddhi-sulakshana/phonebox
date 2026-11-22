//! Writing `config.json` as the C# agent does (`ConfigStore.WriteOpts`).

use std::io;

use serde::Serializer;
use serde_json::ser::{CharEscape, Formatter};

use super::AgentConfig;

/// The settings as `config.json` text, as `JsonSerializer.Serialize(cfg, ConfigStore.WriteOpts)` writes it: two-space
/// indent, the platform's line ending, camelCase, nulls left out, non-ASCII text as is, doubles in .NET's shortest form.
pub fn to_dotnet_json(cfg: &AgentConfig) -> String {
    to_dotnet_json_value(cfg)
}

/// Any serializable value written as `WriteOpts` writes it (see [`to_dotnet_json`]).
pub fn to_dotnet_json_value<T: serde::Serialize + ?Sized>(value: &T) -> String {
    let mut out = Vec::with_capacity(16 * 1024);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, DotnetFormatter::default());
    value.serialize(&mut ser).expect("settings serialize");
    String::from_utf8(out).expect("UTF-8")
}

/// The same value without indentation, as an ASP.NET JSON answer writes it (doubles in .NET's shortest form too).
pub fn to_dotnet_json_compact<T: serde::Serialize + ?Sized>(value: &T) -> String {
    let mut out = Vec::with_capacity(4 * 1024);
    let formatter = DotnetFormatter {
        compact: true,
        ..Default::default()
    };
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    value.serialize(&mut ser).expect("JSON serialize");
    String::from_utf8(out).expect("UTF-8")
}

/// `Utf8JsonWriter` with `WriteIndented` (unless `compact`) and `JavaScriptEncoder.UnsafeRelaxedJsonEscaping`.
#[derive(Default)]
struct DotnetFormatter {
    indent: usize,
    has_value: bool,
    compact: bool,
}

const NEWLINE: &[u8] = if cfg!(windows) { b"\r\n" } else { b"\n" };

impl DotnetFormatter {
    fn newline_indent<W: ?Sized + io::Write>(&self, w: &mut W) -> io::Result<()> {
        if self.compact {
            return Ok(());
        }
        w.write_all(NEWLINE)?;
        for _ in 0..self.indent {
            w.write_all(b"  ")?;
        }
        Ok(())
    }
}

impl Formatter for DotnetFormatter {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent += 1;
        self.has_value = false;
        w.write_all(b"[")
    }
    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent -= 1;
        if self.has_value {
            self.newline_indent(w)?;
        }
        w.write_all(b"]")
    }
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        self.newline_indent(w)
    }
    fn end_array_value<W: ?Sized + io::Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent += 1;
        self.has_value = false;
        w.write_all(b"{")
    }
    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent -= 1;
        if self.has_value {
            self.newline_indent(w)?;
        }
        w.write_all(b"}")
    }
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        self.newline_indent(w)
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(if self.compact { b":" } else { b": " })
    }
    fn end_object_value<W: ?Sized + io::Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(format_double(value).as_bytes())
    }
    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        w.write_all(format_double(value as f64).as_bytes())
    }
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        // serde_json escapes only C0 controls, quotes and backslashes; .NET's relaxed encoder also escapes DEL, the C1
        // controls and the line / paragraph separators
        let mut start = 0;
        for (i, ch) in fragment.char_indices() {
            if matches!(ch, '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}') {
                w.write_all(&fragment.as_bytes()[start..i])?;
                write!(w, "\\u{:04X}", ch as u32)?;
                start = i + ch.len_utf8();
            }
        }
        w.write_all(&fragment.as_bytes()[start..])
    }
    fn write_char_escape<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        escape: CharEscape,
    ) -> io::Result<()> {
        match escape {
            CharEscape::Quote => w.write_all(b"\\\""),
            CharEscape::ReverseSolidus => w.write_all(b"\\\\"),
            CharEscape::Solidus => w.write_all(b"/"),
            CharEscape::Backspace => w.write_all(b"\\b"),
            CharEscape::FormFeed => w.write_all(b"\\f"),
            CharEscape::LineFeed => w.write_all(b"\\n"),
            CharEscape::CarriageReturn => w.write_all(b"\\r"),
            CharEscape::Tab => w.write_all(b"\\t"),
            CharEscape::AsciiControl(b) => write!(w, "\\u{:04X}", b),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_doubles_like_dotnet() {
        let mut cfg = AgentConfig::default();
        cfg.night.brightness = 1.0;
        cfg.weather.lat = 6.9271;
        let text = to_dotnet_json(&cfg);
        assert!(
            text.contains("\"brightness\": 1,")
                || text.contains("\"brightness\": 1\n")
                || text.contains("\"brightness\": 1\r\n")
        );
        assert!(text.contains("\"lat\": 6.9271"));
    }

    #[test]
    fn writes_like_the_csharp_writer() {
        let mut cfg = AgentConfig::default();
        cfg.shortcuts.push(crate::config::Shortcut {
            id: "t".into(),
            label: "Café\u{7f}".into(),
            icon: "\u{2328}\u{fe0f}".into(),
            cmd: "a\"b\\c\n".into(),
            ..Default::default()
        });
        let text = to_dotnet_json(&cfg);
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        assert!(text.starts_with(&format!(
            "{{{nl}  \"codexBar\": {{{nl}    \"providers\": [{nl}      \"claude\","
        )));
        assert!(
            text.contains("\"label\": \"Café\\u007F\""),
            "non-ASCII as is, DEL escaped"
        );
        assert!(
            text.contains("\"icon\": \"\\u2328\\uFE0F\""),
            "icons escaped"
        );
        assert!(text.contains("\"cmd\": \"a\\\"b\\\\c\\n\""));
        assert!(text.contains("\"pages\": []"));
        assert!(!text.contains("deviceSerial"), "nulls are left out");
        assert!(!text.ends_with('\n'));
    }
}
