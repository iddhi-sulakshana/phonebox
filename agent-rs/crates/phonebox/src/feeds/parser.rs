//! `FeedParser`: RSS 2.0, RSS 1.0 (RDF) and Atom. No DTDs and no entities beyond XML's own; titles become plain text.
//!
//! The bytes are decoded as .NET's `XmlReader` does without code pages: a BOM or the first bytes pick UTF-8/16/32,
//! and an 8-bit document's declaration may switch to Latin-1 or ASCII; any other declared encoding fails the feed.
//! A DOCTYPE is cut out before parsing (old RSS 0.91 feeds carry one; it is skipped, never loaded), so an entity it
//! declares is an error, as with `DtdProcessing.Ignore`.

use std::borrow::Cow;

use reqwest::Url;

use super::FeedItem;

const MAX_TITLE: usize = 300;
/// A title is short: the raw text is cut first so nothing below works on megabytes.
const MAX_RAW: usize = 8 * MAX_TITLE;

/// The headlines of a feed (at most `max`); Err when it is not XML or not RSS or Atom.
pub fn parse(xml: &[u8], base: &Url, feed_key: &str, max: usize) -> Result<Vec<FeedItem>, String> {
    let text = decode(xml)?;
    let text = strip_doctype(&text);
    let doc = roxmltree::Document::parse(&text).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let local = root.tag_name().name();
    if !matches!(local, "rss" | "RDF" | "feed") {
        return Err("not a feed".into());
    }
    let atom = local == "feed";
    let wanted = if atom { "entry" } else { "item" };
    let mut list = Vec::new();
    // every descendant with that local name, at any depth and in any namespace
    for e in root
        .descendants()
        .skip(1)
        .filter(|n| n.is_element() && n.tag_name().name() == wanted)
    {
        let mut title = if atom {
            atom_text(child(e, "title"))
        } else {
            clean(&child(e, "title").map(value).unwrap_or_default(), true)
        };
        let link = if atom {
            atom_link(e, base)
        } else {
            rss_link(e, base)
        };
        let key = child(e, if atom { "id" } else { "guid" })
            .map(|k| value(k).trim().to_string())
            .filter(|k| !k.is_empty())
            .or_else(|| link.clone())
            .unwrap_or_else(|| title.clone());
        if title.is_empty() {
            title = link
                .as_deref()
                .and_then(|l| Url::parse(l).ok())
                .and_then(|u| u.host_str().map(String::from))
                .unwrap_or_default();
        }
        if title.is_empty() {
            continue;
        }
        let date = if atom {
            child(e, "published").or_else(|| child(e, "updated"))
        } else {
            child(e, "pubDate").or_else(|| child(e, "date")) // dc:date in RSS 1.0
        };
        list.push(FeedItem {
            id: id_of(feed_key, &key),
            title,
            source: String::new(),
            published: parse_date(date.map(value).as_deref()),
            link,
        });
        if list.len() >= max {
            break;
        }
    }
    Ok(list)
}

type Node<'a, 'input> = roxmltree::Node<'a, 'input>;

/// The first direct child element with this local name, in any namespace.
fn child<'a, 'input>(e: Node<'a, 'input>, local: &str) -> Option<Node<'a, 'input>> {
    e.children()
        .find(|c| c.is_element() && c.tag_name().name() == local)
}

/// `XElement.Value`: every descendant text node (CDATA included), joined.
fn value(e: Node) -> String {
    e.descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect()
}

/// `type="html"` holds escaped markup; xhtml holds elements whose text is the title.
fn atom_text(t: Option<Node>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    clean(&value(t), t.attribute("type") == Some("html"))
}

/// The first link with no `rel` or `rel="alternate"`, else the first link, against the xml:base it sits under.
fn atom_link(e: Node, base: &Url) -> Option<String> {
    let links: Vec<Node> = e
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "link")
        .collect();
    let link = links
        .iter()
        .find(|l| matches!(l.attribute("rel"), None | Some("alternate")))
        .or(links.first())?;
    absolute(link.attribute("href"), &base_of(*link, base))
}

/// The feed's address with every xml:base from the root down to `el` applied (RFC 4287 section 2).
fn base_of(el: Node, feed: &Url) -> Url {
    let mut uri = feed.clone();
    let chain: Vec<Node> = el.ancestors().filter(|a| a.is_element()).collect();
    for a in chain.into_iter().rev() {
        if let Some(b) = a
            .attribute((roxmltree::NS_XML_URI, "base"))
            .filter(|b| (1..=2048).contains(&b.encode_utf16().count()))
            && let Ok(u) = uri.join(b.trim())
        {
            uri = u;
        }
    }
    uri
}

/// RSS links ignore xml:base: `<link>`, else a permalink `<guid>`.
fn rss_link(e: Node, base: &Url) -> Option<String> {
    if let Some(l) = absolute(child(e, "link").map(value).as_deref(), base) {
        return Some(l);
    }
    let guid = child(e, "guid")?;
    let permalink = !guid
        .attribute("isPermaLink")
        .is_some_and(|p| p.eq_ignore_ascii_case("false"));
    if permalink {
        absolute(Some(&value(guid)), base)
    } else {
        None
    }
}

fn absolute(href: Option<&str>, base: &Url) -> Option<String> {
    let href = href.map(str::trim).filter(|h| !h.is_empty())?;
    let u = base.join(href).ok()?;
    safe_link(Some(u.as_str())).map(|u| u.to_string())
}

/// The text of a feed's bytes.
fn decode(xml: &[u8]) -> Result<Cow<'_, str>, String> {
    #[derive(Clone, Copy)]
    enum Wide {
        Le16,
        Be16,
        Le32,
        Be32,
    }
    let (wide, body) = match xml {
        [0xFF, 0xFE, 0, 0, ..] => (Some(Wide::Le32), &xml[4..]),
        [0, 0, 0xFE, 0xFF, ..] => (Some(Wide::Be32), &xml[4..]),
        [0xFF, 0xFE, ..] => (Some(Wide::Le16), &xml[2..]),
        [0xFE, 0xFF, ..] => (Some(Wide::Be16), &xml[2..]),
        [0xEF, 0xBB, 0xBF, ..] => (None, &xml[3..]),
        [b'<', 0, 0, 0, ..] => (Some(Wide::Le32), xml),
        [0, 0, 0, b'<', ..] => (Some(Wide::Be32), xml),
        [b'<', 0, ..] => (Some(Wide::Le16), xml),
        [0, b'<', ..] => (Some(Wide::Be16), xml),
        _ => (None, xml),
    };
    const INVALID: &str = "Invalid character in the given encoding";
    if let Some(wide) = wide {
        // a 16- or 32-bit document's own declaration is not consulted
        let text: Option<String> = match wide {
            Wide::Le16 | Wide::Be16 => {
                if body.len() % 2 != 0 {
                    return Err(INVALID.into());
                }
                let units = body.as_chunks::<2>().0.iter().map(|&c| match wide {
                    Wide::Le16 => u16::from_le_bytes(c),
                    _ => u16::from_be_bytes(c),
                });
                char::decode_utf16(units)
                    .collect::<Result<String, _>>()
                    .ok()
            }
            Wide::Le32 | Wide::Be32 => {
                if body.len() % 4 != 0 {
                    return Err(INVALID.into());
                }
                body.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&c| {
                        char::from_u32(match wide {
                            Wide::Le32 => u32::from_le_bytes(c),
                            _ => u32::from_be_bytes(c),
                        })
                    })
                    .collect()
            }
        };
        return text.map(Cow::Owned).ok_or_else(|| INVALID.into());
    }
    let utf8 = || {
        std::str::from_utf8(body)
            .map(Cow::Borrowed)
            .map_err(|_| INVALID.to_string())
    };
    let Some(name) = declared_encoding(body) else {
        return utf8();
    };
    let name = name.to_ascii_lowercase();
    const UTF8: [&str; 6] = [
        "utf-8",
        "unicode-1-1-utf-8",
        "unicode-2-0-utf-8",
        "x-unicode-1-1-utf-8",
        "x-unicode-2-0-utf-8",
        "x-unicode20utf8",
    ];
    const LATIN1: [&str; 10] = [
        "iso-8859-1",
        "latin1",
        "l1",
        "iso_8859-1",
        "iso_8859-1:1987",
        "iso-ir-100",
        "ibm819",
        "cp819",
        "csisolatin1",
        "iso8859-1",
    ];
    const ASCII: [&str; 11] = [
        "us-ascii",
        "ascii",
        "ansi_x3.4-1968",
        "ansi_x3.4-1986",
        "cp367",
        "csascii",
        "ibm367",
        "iso-ir-6",
        "iso646-us",
        "iso_646.irv:1991",
        "us",
    ];
    if UTF8.contains(&name.as_str()) {
        utf8()
    } else if LATIN1.contains(&name.as_str()) {
        Ok(Cow::Owned(body.iter().map(|&b| char::from(b)).collect()))
    } else if ASCII.contains(&name.as_str()) {
        Ok(Cow::Owned(
            body.iter()
                .map(|&b| if b < 0x80 { char::from(b) } else { '?' })
                .collect(),
        ))
    } else {
        // .NET without code pages: windows-1252, koi8-r, iso-8859-15… (and utf-16 in an 8-bit document)
        Err(format!("System does not support '{name}' encoding"))
    }
}

/// The `encoding` of an XML declaration at the very start of an 8-bit document.
fn declared_encoding(body: &[u8]) -> Option<String> {
    let rest = body.strip_prefix(b"<?xml")?;
    if !rest.first().is_some_and(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let end = rest.windows(2).position(|w| w == b"?>")?;
    let decl = &rest[..end];
    let at = decl.windows(8).position(|w| w == b"encoding")? + 8;
    let mut i = at;
    let skip = |i: &mut usize| {
        while decl.get(*i).is_some_and(|b| b.is_ascii_whitespace()) {
            *i += 1;
        }
    };
    skip(&mut i);
    if decl.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    skip(&mut i);
    let quote = *decl.get(i).filter(|q| matches!(q, b'"' | b'\''))?;
    let value = &decl[i + 1..];
    let close = value.iter().position(|&b| b == quote)?;
    std::str::from_utf8(&value[..close]).ok().map(String::from)
}

/// The text without its DOCTYPE (internal subset included), which may only stand before the root element.
fn strip_doctype(text: &str) -> Cow<'_, str> {
    let b = text.as_bytes();
    let find = |from: usize, pat: &[u8]| {
        b.get(from..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|p| p + from)
    };
    let mut i = 0;
    loop {
        while b
            .get(i)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        {
            i += 1;
        }
        let rest = &b[i.min(b.len())..];
        if rest.starts_with(b"<!--") {
            match find(i + 4, b"-->") {
                Some(end) => i = end + 3,
                None => return Cow::Borrowed(text),
            }
        } else if rest.starts_with(b"<!DOCTYPE") {
            return match doctype_end(b, i + 9) {
                Some(end) => Cow::Owned(format!("{}{}", &text[..i], &text[end..])),
                None => Cow::Borrowed(text),
            };
        } else if rest.starts_with(b"<?") {
            match find(i + 2, b"?>") {
                Some(end) => i = end + 2,
                None => return Cow::Borrowed(text),
            }
        } else {
            return Cow::Borrowed(text);
        }
    }
}

/// Where a DOCTYPE ends: its `>` outside quotes, comments and the internal subset.
fn doctype_end(b: &[u8], mut i: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut depth = 0usize;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if depth > 0 && b[i..].starts_with(b"<!--") {
            let close = b[i + 4..].windows(3).position(|w| w == b"-->")?;
            i += 4 + close + 3;
            continue;
        } else {
            match c {
                b'"' | b'\'' => quote = Some(c),
                b'[' => depth += 1,
                b']' => depth = depth.saturating_sub(1),
                b'>' if depth == 0 => return Some(i + 1),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// The first 16 hex digits of SHA-256(feed key, newline, item key).
pub fn id_of(feed_key: &str, item_key: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(format!("{feed_key}\n{item_key}").as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Only http and https links are ever opened (no file:, javascript:, custom app schemes).
pub fn safe_link(link: Option<&str>) -> Option<Url> {
    let u = Url::parse(link?).ok()?;
    (matches!(u.scheme(), "http" | "https") && u.as_str().len() <= 4096).then_some(u)
}

/// Plain text: tags and entities resolved (when `decode_entities`), control and direction characters dropped,
/// white space collapsed, capped at 300 UTF-16 units and an ellipsis.
pub fn clean(s: &str, decode_entities: bool) -> String {
    if s.is_empty() {
        return String::new();
    }
    let cut = cut_utf16(s, MAX_RAW);
    let text = if decode_entities {
        Cow::Owned(html_decode(&strip_tags(&cut)).into_owned())
    } else {
        cut
    };
    let mut out = String::with_capacity(text.len().min(4 * (MAX_TITLE + 1)));
    let mut units = 0;
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            space = units > 0;
            continue;
        }
        // direction overrides, isolates and marks would let a title rearrange the text around it
        if matches!(c as u32, 0x202A..=0x202E | 0x2066..=0x2069 | 0x200E | 0x200F | 0x061C | 0xFEFF)
        {
            continue;
        }
        if space {
            out.push(' ');
            units += 1;
            space = false;
        }
        out.push(c);
        units += c.len_utf16();
        if units >= MAX_TITLE {
            out.push('…');
            break;
        }
    }
    out
}

/// The first `max` UTF-16 units; a pair cut in half leaves U+FFFD, as .NET's rune walk reads the lone half.
fn cut_utf16(s: &str, max: usize) -> Cow<'_, str> {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let n = c.len_utf16();
        if units + n > max {
            if units < max {
                return Cow::Owned(format!("{}\u{FFFD}", &s[..i]));
            }
            return Cow::Borrowed(&s[..i]);
        }
        units += n;
    }
    Cow::Borrowed(s)
}

/// Removes `<…>` tags in one pass (each becomes a space); a `<` with no `>` after it is kept as text.
fn strip_tags(s: &str) -> Cow<'_, str> {
    let Some(first) = s.find('<') else {
        return Cow::Borrowed(s);
    };
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut next = Some(first);
    while let Some(lt) = next {
        let Some(gt) = s[lt + 1..].find('>').map(|g| g + lt + 1) else {
            break;
        };
        out.push_str(&s[i..lt]);
        out.push(' ');
        i = gt + 1;
        next = s[i..].find('<').map(|n| i + n);
    }
    out.push_str(&s[i..]);
    Cow::Owned(out)
}

/// `WebUtility.HtmlDecode`.
pub(crate) fn html_decode(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // another '&' before the ';' means this one starts no entity (the next one might)
        match after.find([';', '&']) {
            Some(n) if after.as_bytes()[n] == b';' => {
                let entity = &after[..n];
                if n > 1 && entity.starts_with('#') {
                    if let Some(c) = numeric_entity(&entity[1..]) {
                        out.push(c);
                        rest = &after[n + 1..];
                    } else {
                        out.push('&');
                        rest = after;
                    }
                } else {
                    match super::html_entities::lookup(entity) {
                        Some(c) => out.push(c),
                        None => {
                            out.push('&');
                            out.push_str(entity);
                            out.push(';');
                        }
                    }
                    rest = &after[n + 1..];
                }
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// `&#…;`: hex after an `x` (`AllowHexSpecifier`), else decimal with white space around and a sign
/// (`NumberStyles.Integer`); U+0000 to U+10FFFF without surrogates.
fn numeric_entity(digits: &str) -> Option<char> {
    let value = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()?
    } else {
        let t = digits.trim_matches(|c| matches!(c, '\t'..='\r' | ' '));
        let (negative, t) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = t.parse().ok()?;
        if negative && v != 0 {
            return None;
        }
        v
    };
    char::from_u32(value)
}

/// RFC 822 (RSS) and ISO 8601 (Atom, dc:date) dates in Unix ms; 0 when unreadable.
pub fn parse_date(s: Option<&str>) -> i64 {
    let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return 0;
    };
    if s.encode_utf16().count() > 64 {
        return 0; // no real date is longer; keeps the patterns below cheap
    }
    if let Some(m) = RFC822.captures(s) {
        return rfc822(&m).unwrap_or(0);
    }
    iso_date(s).or_else(|| date_only(s)).unwrap_or(0)
}

static RFC822: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:[A-Za-z]+,?\s*)?([0-9]{1,2})\s+([A-Za-z]{3})[A-Za-z]*\.?\s+([0-9]{2,4})\s+([0-9]{1,2}):([0-9]{2})(?::([0-9]{2}))?\s*(.*)$",
    )
    .expect("Rfc822")
});

/// RFC 822 without a time, as `DateTimeOffset.TryParse` reads it: `[weekday,] day month year` at UTC midnight.
static DATE_ONLY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^(?:([A-Za-z]+),?\s*)?([0-9]{1,2})\s+([A-Za-z]+)\.?\s+([0-9]{4})$")
        .expect("DateOnly")
});

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn rfc822(m: &regex::Captures) -> Option<i64> {
    let group = |i: usize| m.get(i).map_or("", |g| g.as_str());
    let mon = MONTHS
        .iter()
        .position(|x| *x == group(2).to_ascii_lowercase())?;
    let mut year: i32 = group(3).parse().ok()?;
    if group(3).len() == 2 {
        year += if year < 50 { 2000 } else { 1900 };
    }
    let zone = group(7).trim();
    let zb = zone.as_bytes();
    let offset_minutes = if zb.len() == 5
        && matches!(zb[0], b'+' | b'-')
        && zb[1..].iter().all(u8::is_ascii_digit)
    {
        let hhmm: i32 = zone[1..].parse().ok()?;
        (hhmm / 100 * 60 + hhmm % 100) * if zb[0] == b'-' { -1 } else { 1 }
    } else {
        let hours = match zone.to_ascii_uppercase().as_str() {
            "GMT" | "UT" | "UTC" | "Z" => 0,
            "EST" | "CDT" => -5,
            "EDT" => -4,
            "CST" | "MDT" => -6,
            "MST" | "PDT" => -7,
            "PST" => -8,
            _ => 0,
        };
        hours * 60
    };
    let sec: u32 = m.get(6).map_or(Ok(0), |g| g.as_str().parse()).ok()?;
    let (day, hour, minute): (u32, u32, u32) = (
        group(1).parse().ok()?,
        group(4).parse().ok()?,
        group(5).parse().ok()?,
    );
    utc_ms(
        year,
        mon as u32 + 1,
        day,
        hour,
        minute,
        sec.min(59),
        0,
        offset_minutes,
    )
}

/// `new DateTimeOffset(…)`: a real calendar time in years 1 to 9999, an offset within 14 hours, and a UTC time in
/// range; `ticks` (100 ns) are added to the seconds.
#[allow(clippy::too_many_arguments)]
fn utc_ms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    ticks: i64,
    offset_minutes: i32,
) -> Option<i64> {
    if !(1..=9999).contains(&year) || offset_minutes.abs() > 14 * 60 {
        return None;
    }
    let local =
        chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second)?;
    let utc = local - chrono::TimeDelta::minutes(i64::from(offset_minutes));
    let first = chrono::NaiveDate::from_ymd_opt(1, 1, 1)?.and_hms_opt(0, 0, 0)?;
    let last = chrono::NaiveDate::from_ymd_opt(9999, 12, 31)?.and_hms_opt(23, 59, 59)?;
    if utc < first || utc > last {
        return None;
    }
    // ToUnixTimeMilliseconds floors the ticks
    Some(utc.and_utc().timestamp_millis() + ticks.div_euclid(10_000))
}

/// ISO 8601 / RFC 3339 as `DateTimeOffset.TryParse` read them on the C# agent: `YYYY-MM`, `YYYY-MM-DD`, then
/// `T`/`t`/space, `HH:MM`, optional `:SS` and a `.`/`,` fraction, and an optional zone (`Z`, `±HH`, `±HHMM`,
/// `±HH:MM`, a space allowed before it). No zone is UTC.
fn iso_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| -> Option<u32> {
        let part = b.get(from..from + n)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0, |v, d| v * 10 + u32::from(d - b'0')))
    };
    let year = digits(0, 4)? as i32;
    if b.get(4) != Some(&b'-') {
        return None;
    }
    let month = digits(5, 2)?;
    if b.len() == 7 {
        return utc_ms(year, month, 1, 0, 0, 0, 0, 0);
    }
    if b.get(7) != Some(&b'-') {
        return None;
    }
    let day = digits(8, 2)?;
    if b.len() == 10 {
        return utc_ms(year, month, day, 0, 0, 0, 0, 0);
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let hour = digits(11, 2)?;
    if b.get(13) != Some(&b':') {
        return None;
    }
    let minute = digits(14, 2)?;
    let mut i = 16;
    let mut second = 0;
    let mut ticks = 0i64;
    if b.get(i) == Some(&b':') {
        second = digits(i + 1, 2)?;
        i += 3;
        if matches!(b.get(i), Some(b'.' | b',')) {
            // DateTimeParse.ParseFraction, then Math.Round(fraction × ticks per second)
            let mut fraction = 0f64;
            let mut place = 0.1f64;
            let start = i + 1;
            i = start;
            while let Some(d) = b.get(i).filter(|d| d.is_ascii_digit()) {
                fraction += f64::from(d - b'0') * place;
                place *= 0.1;
                i += 1;
            }
            if i == start {
                return None;
            }
            ticks = (fraction * 10_000_000.0).round_ties_even() as i64;
        }
    }
    if b.get(i) == Some(&b' ') && matches!(b.get(i + 1), Some(b'+' | b'-')) {
        i += 1;
    }
    let offset = match b.get(i) {
        None => 0,
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            let hh = digits(i + 1, 2)? as i32;
            let mm = match &b[i + 3..] {
                [] => 0,
                [b':', ..] if b.len() == i + 6 => digits(i + 4, 2)? as i32,
                _ if b.len() == i + 5 => digits(i + 3, 2)? as i32,
                _ => return None,
            };
            if mm > 59 {
                return None;
            }
            (hh * 60 + mm) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return None,
    };
    utc_ms(year, month, day, hour, minute, second, ticks, offset)
}

fn date_only(s: &str) -> Option<i64> {
    const FULL: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    const DAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let m = DATE_ONLY.captures(s)?;
    let name = m.get(3)?.as_str().to_ascii_lowercase();
    let month = MONTHS
        .iter()
        .position(|x| *x == name)
        .or_else(|| FULL.iter().position(|x| *x == name))? as u32
        + 1;
    let (day, year): (u32, i32) = (
        m.get(2)?.as_str().parse().ok()?,
        m.get(4)?.as_str().parse().ok()?,
    );
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    if let Some(weekday) = m.get(1) {
        // a weekday must name the date's own
        use chrono::Datelike;
        let w = weekday.as_str().to_ascii_lowercase();
        let own = DAYS[date.weekday().num_days_from_monday() as usize];
        if w != own && w != own[..3] {
            return None;
        }
    }
    utc_ms(year, month, day, 0, 0, 0, 0, 0)
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
