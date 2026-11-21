//! .NET text rules the phone sees through the C# agent: lengths and cuts in UTF-16 units, `Path.GetFileName`, and
//! `DateTimeOffset.TryParse` for the ISO dates the tools write.

/// `string.Length`: UTF-16 units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of at most `n` UTF-16 units that does not split a character.
pub fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// `s.Length > n ? s[..n] + "…" : s` (a pair that would be split is left out whole).
pub fn cut(s: &str, n: usize) -> String {
    if utf16_len(s) > n {
        format!("{}…", utf16_prefix(s, n))
    } else {
        s.to_string()
    }
}

/// A double through one of the custom formats `0`, `0.#`, `0.##` (`max_decimals` 0, 1, 2) or `#,0` (`grouped`), invariant
/// culture: .NET first takes 15 significant digits, then rounds half away from zero, drops trailing zeros and groups
/// thousands with `,`.
pub fn format_custom(x: f64, max_decimals: usize, grouped: bool) -> String {
    if !x.is_finite() || x == 0.0 {
        return if x.is_nan() {
            "NaN".into()
        } else if x.is_infinite() {
            if x > 0.0 { "∞" } else { "-∞" }.into()
        } else {
            "0".into()
        };
    }
    // 15 significant digits: d.dddddddddddddd × 10^exp
    let sci = format!("{:.14e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let mut digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    // the integer part is digits[..point]
    let mut point = exp + 1;
    if point < 1 {
        let pad = usize::try_from(1 - point).unwrap_or(0);
        digits.splice(0..0, std::iter::repeat_n(0, pad));
        point = 1;
    }
    let mut point = usize::try_from(point).unwrap_or(0);
    if digits.len() < point {
        digits.resize(point, 0);
    }
    let cut = point + max_decimals;
    if digits.len() > cut {
        let up = digits[cut] >= 5;
        digits.truncate(cut);
        if up {
            let mut i = cut;
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    point += 1;
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
    }
    let (int, frac) = digits.split_at(point);
    let first = int
        .iter()
        .position(|&d| d != 0)
        .unwrap_or(int.len().saturating_sub(1));
    let int = &int[first..];
    let frac_len = frac.iter().rposition(|&d| d != 0).map_or(0, |p| p + 1);
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    for (i, d) in int.iter().enumerate() {
        if grouped && i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(char::from(b'0' + d));
    }
    if frac_len > 0 {
        out.push('.');
        out.extend(frac[..frac_len].iter().map(|d| char::from(b'0' + d)));
    }
    out
}

/// `ToUpperInvariant`: one character for one (a character whose upper case is longer, such as `ß`, stays).
pub fn upper_invariant(s: &str) -> String {
    s.chars()
        .map(|c| single(c.to_uppercase()).unwrap_or(c))
        .collect()
}

fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
    let first = it.next()?;
    it.next().is_none().then_some(first)
}

/// `ToLowerInvariant`, one character for one.
pub fn lower_invariant(s: &str) -> String {
    s.chars()
        .map(|c| single(c.to_lowercase()).unwrap_or(c))
        .collect()
}

/// `Path.GetFileName`: the part after the last `/` (and `\` on Windows).
pub fn file_name(path: &str) -> &str {
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    path.rsplit(separators).next().unwrap_or(path)
}

/// `DateTimeOffset.TryParse(text, InvariantCulture, …)` as Unix ms: ISO 8601 / RFC 3339 with or without an offset,
/// RFC 1123, a date alone. A text without an offset is UTC with `assume_utc` (`AssumeUniversal`), else local time.
pub fn parse_date_ms(text: &str, assume_utc: bool) -> Option<i64> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(t).or_else(|_| DateTime::parse_from_rfc2822(t)) {
        return Some(d.timestamp_millis());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
    ] {
        if let Ok(d) = DateTime::parse_from_str(t, format) {
            return Some(d.timestamp_millis());
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(t, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })?;
    if assume_utc {
        Some(naive.and_utc().timestamp_millis())
    } else {
        naive
            .and_local_timezone(chrono::Local)
            .earliest()
            .map(|d| d.timestamp_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_formats_round_half_away_from_zero_on_fifteen_digits() {
        for (x, decimals, grouped, text) in [
            (1.5, 1, false, "1.5"),
            (100.0 / 60.0, 1, false, "1.7"),
            (2.0, 1, false, "2"),
            (12.5, 2, false, "12.5"),
            (12.345, 2, false, "12.35"),
            (2.675, 2, false, "2.68"),
            (0.0, 2, false, "0"),
            (0.004, 2, false, "0"),
            (1e-20, 2, false, "0"),
            (12.5, 0, false, "13"),
            (0.5, 0, false, "1"),
            (99.96, 1, false, "100"),
            (1_234_567.5, 0, true, "1,234,568"),
            (999.5, 0, true, "1,000"),
            (123.0, 0, true, "123"),
            (1234.0, 0, true, "1,234"),
            (-1234.4, 0, true, "-1,234"),
            (1e17, 0, true, "100,000,000,000,000,000"),
        ] {
            assert_eq!(
                format_custom(x, decimals, grouped),
                text,
                "{x} {decimals} {grouped}"
            );
        }
    }

    #[test]
    fn invariant_case_maps_one_character_for_one() {
        assert_eq!(upper_invariant("eur"), "EUR");
        assert_eq!(upper_invariant("straße"), "STRAßE");
        assert_eq!(lower_invariant("Claude-Sonnet"), "claude-sonnet");
    }

    #[test]
    fn lengths_and_cuts_count_utf16_units() {
        assert_eq!(utf16_len("a😀é"), 4);
        assert_eq!(utf16_prefix("ab😀c", 3), "ab");
        assert_eq!(utf16_prefix("ab😀c", 4), "ab😀");
        assert_eq!(utf16_prefix("abc", 10), "abc");
        assert_eq!(cut("abcdef", 4), "abcd…");
        assert_eq!(cut("abcd", 4), "abcd");
        assert_eq!(cut("abc😀", 4), "abc…");
    }

    #[test]
    fn file_names_split_like_dotnet_on_this_os() {
        assert_eq!(file_name("/a/b/file.cs"), "file.cs");
        assert_eq!(file_name("file.cs"), "file.cs");
        assert_eq!(file_name("/a/b/"), "");
        if cfg!(windows) {
            assert_eq!(file_name(r"C:\x\y.md"), "y.md");
        } else {
            assert_eq!(file_name(r"C:\x\y.md"), r"C:\x\y.md");
        }
    }

    #[test]
    fn dates_parse_like_datetimeoffset() {
        assert_eq!(
            parse_date_ms("2026-11-19T08:00:00.000Z", true),
            Some(1_795_075_200_000)
        );
        assert_eq!(
            parse_date_ms("2026-11-19T08:00:00Z", false),
            Some(1_795_075_200_000)
        );
        assert_eq!(
            parse_date_ms("2026-11-19T13:30:00+05:30", false),
            Some(1_795_075_200_000)
        );
        assert_eq!(
            parse_date_ms("2026-11-19T08:00:00", true),
            Some(1_795_075_200_000)
        );
        assert_eq!(
            parse_date_ms("2026-11-19 08:00:00.5", true),
            Some(1_795_075_200_500)
        );
        assert_eq!(
            parse_date_ms("Thu, 19 Nov 2026 08:00:00 GMT", false),
            Some(1_795_075_200_000)
        );
        assert_eq!(parse_date_ms("2026-11-19", true), Some(1_795_046_400_000));
        let local = chrono::NaiveDate::from_ymd_opt(2026, 11, 19)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .timestamp_millis();
        assert_eq!(parse_date_ms("2026-11-19T08:00:00", false), Some(local));
        for bad in ["", "soon", "2026-13-40T00:00:00Z", "12"] {
            assert_eq!(parse_date_ms(bad, true), None, "{bad}");
        }
    }
}
