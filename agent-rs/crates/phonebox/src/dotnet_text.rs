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
