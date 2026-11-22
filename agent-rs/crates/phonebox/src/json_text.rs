//! JSON as `System.Text.Json`'s `JsonNode` / `JsonElement` keep it: member order, duplicate members, and the exact text
//! of every number (`1.0` stays `1.0`), with the comments and trailing commas `JsonDocumentOptions` can allow. Written
//! back the way `Utf8JsonWriter` indents, with the relaxed or the default escaping.

use crate::config::write::escape_js_default;

/// A parsed JSON value; numbers keep their text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escaping {
    /// `JavaScriptEncoder.UnsafeRelaxedJsonEscaping`: non-ASCII as is.
    Relaxed,
    /// `JavaScriptEncoder.Default`: everything outside printable ASCII and the HTML-sensitive characters as `\uXXXX`.
    Default,
}

/// `JsonDocumentOptions.MaxDepth` left at 0 means 64.
const MAX_DEPTH: usize = 64;

impl Node {
    /// A member (the last of repeated ones, as `JsonElement.TryGetProperty` finds it).
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Object(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Node> {
        match self {
            Node::Object(members) => members
                .iter_mut()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Node::Object(_))
    }

    /// `Str`: a member's text when it is a string.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Node::as_str)
    }

    /// `Num`: a member that is a finite number.
    pub fn num(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Node::Number(text) => text.parse::<f64>().ok().filter(|n| n.is_finite()),
            _ => None,
        }
    }

    /// `Long` (`TryGetInt64`): a member that is a whole number in range, written without a fraction or an exponent.
    pub fn long(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Node::Number(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Parses `text`; `jsonc` allows `//` and `/* */` comments and trailing commas.
pub fn parse(text: &str, jsonc: bool) -> Result<Node, String> {
    let mut p = Parser {
        s: text,
        b: text.as_bytes(),
        i: 0,
        jsonc,
        depth: 0,
    };
    p.skip_ws()?;
    if p.i == p.b.len() {
        return Err("The input does not contain any JSON tokens.".into());
    }
    let node = p.value()?;
    p.skip_ws()?;
    if p.i < p.b.len() {
        return Err(p.error("is invalid after a single JSON value"));
    }
    Ok(node)
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    jsonc: bool,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        let line = self.b[..self.i].iter().filter(|&&c| c == b'\n').count();
        let col = self.i
            - self.b[..self.i]
                .iter()
                .rposition(|&c| c == b'\n')
                .map_or(0, |p| p + 1);
        match self.s[self.i..].chars().next() {
            Some(c) => format!("'{c}' {what}. LineNumber: {line} | BytePositionInLine: {col}."),
            None => {
                format!("The JSON ends too early. LineNumber: {line} | BytePositionInLine: {col}.")
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) -> Result<(), String> {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.i += 1;
            }
            if !self.jsonc || self.peek() != Some(b'/') {
                return Ok(());
            }
            match self.b.get(self.i + 1) {
                Some(b'/') => {
                    self.i = self.b[self.i..]
                        .iter()
                        .position(|&c| c == b'\n' || c == b'\r')
                        .map_or(self.b.len(), |p| self.i + p);
                }
                Some(b'*') => match self.s[self.i + 2..].find("*/") {
                    Some(p) => self.i += 2 + p + 2,
                    None => {
                        self.i = self.b.len();
                        return Err(self.error("is an unterminated comment"));
                    }
                },
                _ => return Err(self.error("is not the start of a comment")),
            }
        }
    }

    fn value(&mut self) -> Result<Node, String> {
        match self.peek() {
            Some(b'{') => self
                .container(b'}', |p| {
                    if p.peek() != Some(b'"') {
                        return Err(
                            p.error("is an invalid start of a property name. Expected a '\"'")
                        );
                    }
                    let key = p.string()?;
                    p.skip_ws()?;
                    if p.peek() != Some(b':') {
                        return Err(p.error("is invalid after a property name. Expected a ':'"));
                    }
                    p.i += 1;
                    p.skip_ws()?;
                    Ok((Some(key), p.value()?))
                })
                .map(|members| {
                    Node::Object(
                        members
                            .into_iter()
                            .map(|(k, v)| (k.unwrap_or_default(), v))
                            .collect(),
                    )
                }),
            Some(b'[') => self
                .container(b']', |p| Ok((None, p.value()?)))
                .map(|items| Node::Array(items.into_iter().map(|(_, v)| v).collect())),
            Some(b'"') => self.string().map(Node::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal("true", Node::Bool(true)),
            Some(b'f') => self.literal("false", Node::Bool(false)),
            Some(b'n') => self.literal("null", Node::Null),
            _ => Err(self.error("is an invalid start of a value")),
        }
    }

    /// `{ ... }` or `[ ... ]` from the opening bracket: items read by `item`, comma-separated.
    fn container(
        &mut self,
        close: u8,
        mut item: impl FnMut(&mut Self) -> Result<(Option<String>, Node), String>,
    ) -> Result<Vec<(Option<String>, Node)>, String> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error(&format!("goes deeper than the maximum of {MAX_DEPTH}")));
        }
        self.depth += 1;
        self.i += 1;
        let mut items = Vec::new();
        self.skip_ws()?;
        if self.peek() == Some(close) {
            self.i += 1;
            self.depth -= 1;
            return Ok(items);
        }
        loop {
            items.push(item(self)?);
            self.skip_ws()?;
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.skip_ws()?;
                    if self.jsonc && self.peek() == Some(close) {
                        self.i += 1;
                        break;
                    }
                }
                Some(c) if c == close => {
                    self.i += 1;
                    break;
                }
                _ => {
                    return Err(
                        self.error("is invalid here. Expected a ',' or the end of the list")
                    );
                }
            }
        }
        self.depth -= 1;
        Ok(items)
    }

    fn literal(&mut self, word: &str, node: Node) -> Result<Node, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(node)
        } else {
            Err(self.error(&format!(
                "is an invalid JSON literal. Expected the literal '{word}'"
            )))
        }
    }

    fn number(&mut self) -> Result<Node, String> {
        let start = self.i;
        let digits = |p: &mut Self| {
            let from = p.i;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i > from
        };
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.error("is not a digit. Expected a digit after the sign")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.error("is not a digit. Expected a digit after the decimal point"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.error("is not a digit. Expected a digit in the exponent"));
            }
        }
        Ok(Node::Number(self.s[start..self.i].to_string()))
    }

    /// A string from its opening quote; `\u` escapes of lone surrogates read as U+FFFD (Rust text cannot hold them).
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let from = self.i;
            while matches!(self.peek(), Some(c) if c != b'"' && c != b'\\' && c >= 0x20) {
                self.i += 1;
            }
            out.push_str(&self.s[from..self.i]);
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let escaped = match self.peek() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.i += 1;
                            let unit = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&unit)
                                && self.b[self.i..].starts_with(b"\\u")
                                && let Some(low) = self.peek_hex4(self.i + 2)
                                && (0xDC00..0xE000).contains(&low)
                            {
                                self.i += 6;
                                char::from_u32(
                                    0x10000
                                        + ((unit as u32 - 0xD800) << 10)
                                        + (low as u32 - 0xDC00),
                                )
                            } else {
                                char::from_u32(unit as u32)
                            };
                            out.push(c.unwrap_or('\u{fffd}'));
                            continue;
                        }
                        _ => {
                            return Err(
                                self.error("is not valid within a JSON string after a '\\'")
                            );
                        }
                    };
                    out.push(escaped);
                    self.i += 1;
                }
                Some(_) => {
                    return Err(self.error(
                        "is invalid within a JSON string. The string should be correctly escaped",
                    ));
                }
                None => return Err(self.error("ends a string that was never closed")),
            }
        }
    }

    fn peek_hex4(&self, at: usize) -> Option<u16> {
        let digits = self.b.get(at..at + 4)?;
        let text = std::str::from_utf8(digits).ok()?;
        if !text.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(text, 16).ok()
    }

    fn hex4(&mut self) -> Result<u16, String> {
        let unit = self
            .peek_hex4(self.i)
            .ok_or_else(|| self.error("is not part of four hex digits after '\\u'"))?;
        self.i += 4;
        Ok(unit)
    }
}

/// `Utf8JsonWriter` with `Indented = true`: two spaces, the platform's line ending, `"name": value`, `{}` and `[]`
/// for empty ones, no line ending at the end.
pub fn to_indented(node: &Node, escaping: Escaping) -> String {
    let mut out = String::new();
    write(node, escaping, 0, &mut out);
    out
}

const NEWLINE: &str = if cfg!(windows) { "\r\n" } else { "\n" };

fn write(node: &Node, escaping: Escaping, depth: usize, out: &mut String) {
    let indent = |out: &mut String, depth: usize| {
        out.push_str(NEWLINE);
        out.extend(std::iter::repeat_n("  ", depth));
    };
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Node::Number(text) => out.push_str(text),
        Node::String(s) => escape(s, escaping, out),
        Node::Array(items) if items.is_empty() => out.push_str("[]"),
        Node::Object(members) if members.is_empty() => out.push_str("{}"),
        Node::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                write(item, escaping, depth + 1, out);
            }
            indent(out, depth);
            out.push(']');
        }
        Node::Object(members) => {
            out.push('{');
            for (n, (key, value)) in members.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                escape(key, escaping, out);
                out.push_str(": ");
                write(value, escaping, depth + 1, out);
            }
            indent(out, depth);
            out.push('}');
        }
    }
}

fn escape(s: &str, escaping: Escaping, out: &mut String) {
    if escaping == Escaping::Default {
        out.push_str(&escape_js_default(s));
        return;
    }
    // the relaxed encoder: as the config writer (quotes, backslashes, controls, DEL, C1, line / paragraph separators)
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04X}", ch as u32))
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    const NL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

    #[test]
    fn numbers_members_and_order_survive_a_round_trip() {
        let text = r#"{"z": 1.0, "a": {"big": 1e3, "neg": -0.50, "list": [true, false, null, "x"]}, "e": {}, "f": []}"#;
        let node = parse(text, false).unwrap();
        let out = to_indented(&node, Escaping::Relaxed);
        let expected = [
            "{",
            "  \"z\": 1.0,",
            "  \"a\": {",
            "    \"big\": 1e3,",
            "    \"neg\": -0.50,",
            "    \"list\": [",
            "      true,",
            "      false,",
            "      null,",
            "      \"x\"",
            "    ]",
            "  },",
            "  \"e\": {},",
            "  \"f\": []",
            "}",
        ]
        .join(NL);
        assert_eq!(out, expected);
        assert_eq!(parse(&out, false).unwrap(), node);
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed_only_when_asked() {
        let text = "// head\n{ /* a */ \"a\": [1, 2,], \"b\": \"//not a comment\", }\n";
        let node = parse(text, true).unwrap();
        assert_eq!(
            node.get("b").and_then(Node::as_str),
            Some("//not a comment")
        );
        assert_eq!(
            node.get("a"),
            Some(&Node::Array(vec![
                Node::Number("1".into()),
                Node::Number("2".into())
            ]))
        );
        assert!(parse(text, false).is_err());
        for bad in [
            "",
            "{",
            "{\"a\":}",
            "[1,,2]",
            "01",
            "1.",
            "-",
            "\"\\x\"",
            "{\"a\" 1}",
            "[1] 2",
            "nul",
            "\"a",
        ] {
            assert!(parse(bad, true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn strings_decode_and_encode_with_either_escaping() {
        let node = parse(
            r#"{"k\u00e9": "a\"b\\c\n\u00e9<&'+`>\ud83d\ude00\u0001\u2028"}"#,
            false,
        )
        .unwrap();
        assert_eq!(
            node.get("ké").and_then(Node::as_str),
            Some("a\"b\\c\né<&'+`>😀\u{1}\u{2028}")
        );
        let relaxed = to_indented(&node, Escaping::Relaxed);
        assert!(
            relaxed.contains(r#""ké": "a\"b\\c\né<&'+`>😀\u0001\u2028""#),
            "{relaxed}"
        );
        let default = to_indented(&node, Escaping::Default);
        assert!(
            default.contains(r#""k\u00E9": "a\u0022b\\c\n\u00E9\u003C\u0026\u0027\u002B\u0060\u003E\uD83D\uDE00\u0001\u2028""#),
            "{default}"
        );
    }

    #[test]
    fn the_last_of_repeated_members_is_the_one_read_and_all_are_kept() {
        let mut node = parse(r#"{"a": 1, "a": 2}"#, false).unwrap();
        assert_eq!(node.get("a"), Some(&Node::Number("2".into())));
        *node.get_mut("a").unwrap() = Node::Bool(true);
        assert_eq!(
            to_indented(&node, Escaping::Relaxed),
            ["{", "  \"a\": 1,", "  \"a\": true", "}"].join(NL)
        );
    }

    #[test]
    fn deep_nesting_is_refused() {
        assert!(parse(&format!("{}{}", "[".repeat(64), "]".repeat(64)), false).is_ok());
        assert!(parse(&format!("{}{}", "[".repeat(65), "]".repeat(65)), false).is_err());
    }
}
