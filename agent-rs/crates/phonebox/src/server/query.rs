//! Query-string parameters as ASP.NET minimal APIs bind them: names in any case; several values of one name are one
//! value joined with "," (`StringValues.ToString()`); an `int?` / `long?` that is present must parse with
//! `TryParse`, so an empty value or "1,2" is a 400.

pub type Pairs = [(String, String)];

/// A number parameter that is present but does not parse (ASP.NET answers 400).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotANumber;

/// The parameter's value (several joined with ","), or None when it is not in the query.
pub fn value(q: &Pairs, name: &str) -> Option<String> {
    let mut found = q
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .peekable();
    found.peek()?;
    Some(found.collect::<Vec<_>>().join(","))
}

/// `NumberStyles.Integer`: white space around (tab to carriage return, and space) and one leading sign.
fn trimmed(v: &str) -> &str {
    v.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
}

/// An `int?` parameter: Ok(None) when absent, Err(NotANumber) (a 400) when present but not an int.
pub fn int32(q: &Pairs, name: &str) -> Result<Option<i32>, NotANumber> {
    value(q, name)
        .map(|v| trimmed(&v).parse().map_err(|_| NotANumber))
        .transpose()
}

/// A `long?` parameter, as [`int32`].
pub fn int64(q: &Pairs, name: &str) -> Result<Option<i64>, NotANumber> {
    value(q, name)
        .map(|v| trimmed(&v).parse().map_err(|_| NotANumber))
        .transpose()
}

/// A `bool?` parameter: Ok(None) when absent, Err (a 400) when present but not `bool.TryParse`-able ("true" or
/// "false" in any case, white space and NULs around allowed).
pub fn boolean(q: &Pairs, name: &str) -> Result<Option<bool>, NotANumber> {
    value(q, name)
        .map(|v| {
            let v = v.trim_matches(|c: char| c.is_whitespace() || c == '\0');
            if v.eq_ignore_ascii_case("true") {
                Ok(true)
            } else if v.eq_ignore_ascii_case("false") {
                Ok(false)
            } else {
                Err(NotANumber)
            }
        })
        .transpose()
}

/// A `double?` parameter (`double.TryParse`, invariant): Ok(None) when absent, Err(NotANumber) (a 400) when present
/// but not a number.
pub fn float64(q: &Pairs, name: &str) -> Result<Option<f64>, NotANumber> {
    value(q, name)
        .map(|v| crate::config::normalize::parse_invariant_double(&v).ok_or(NotANumber))
        .transpose()
}

/// A JSON body bound like an ASP.NET typed parameter: a JSON content type (else 415) and a JSON object (an empty,
/// invalid or null body is a 400). Look its members up with [`json_field`]: names match in any case.
pub fn json_object(
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<serde_json::Map<String, serde_json::Value>, axum::http::StatusCode> {
    let json_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("application/json"));
    if !json_type {
        return Err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    match serde_json::from_slice(body) {
        Ok(serde_json::Value::Object(o)) => Ok(o),
        _ => Err(axum::http::StatusCode::BAD_REQUEST),
    }
}

/// A member of a bound JSON body, its name in any case (the first one that matches).
pub fn json_field<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Option<&'a serde_json::Value> {
    body.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// A `string?` member: absent or null is None; any other kind does not bind (a 400).
pub fn json_string<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<&'a str>, NotANumber> {
    match json_field(body, name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(NotANumber),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn values_are_found_in_any_case_and_joined() {
        assert_eq!(value(&q(&[("Name", "a")]), "name").as_deref(), Some("a"));
        assert_eq!(
            value(&q(&[("name", "a"), ("x", "1"), ("NAME", "b")]), "name").as_deref(),
            Some("a,b")
        );
        assert_eq!(value(&q(&[("name", "")]), "name").as_deref(), Some(""));
        assert_eq!(value(&q(&[]), "name"), None);
    }

    #[test]
    fn bools_follow_try_parse() {
        assert_eq!(boolean(&q(&[]), "upload"), Ok(None));
        assert_eq!(boolean(&q(&[("Upload", "TRUE")]), "upload"), Ok(Some(true)));
        assert_eq!(
            boolean(&q(&[("upload", " false\0")]), "upload"),
            Ok(Some(false))
        );
        for bad in ["", "1", "yes", "true,true", "tru e"] {
            assert_eq!(
                boolean(&q(&[("upload", bad)]), "upload"),
                Err(NotANumber),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn numbers_follow_try_parse() {
        assert_eq!(int32(&q(&[]), "top"), Ok(None));
        assert_eq!(int32(&q(&[("top", "5")]), "top"), Ok(Some(5)));
        assert_eq!(int32(&q(&[("TOP", " -7 ")]), "top"), Ok(Some(-7)));
        assert_eq!(int32(&q(&[("top", "+3")]), "top"), Ok(Some(3)));
        for bad in ["", "x", "1,2", "+-3", "99999999999", "1.5", "\u{a0}5"] {
            assert_eq!(
                int32(&q(&[("top", bad)]), "top"),
                Err(NotANumber),
                "{bad:?}"
            );
        }
        assert_eq!(
            int32(&q(&[("top", "1"), ("top", "2")]), "top"),
            Err(NotANumber)
        );
        assert_eq!(
            int64(&q(&[("ev", "99999999999")]), "ev"),
            Ok(Some(99_999_999_999))
        );
        assert_eq!(int64(&q(&[("ev", "")]), "ev"), Err(NotANumber));
    }
}
