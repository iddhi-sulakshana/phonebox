//! A release's assets as `release.yml` names them, and the `SHA256SUMS` listing that vouches for them
//! (`UpdateService.cs:428-486`).

use std::path::Path;

use serde_json::Value;

/// `UpdateService.Release`: the version (the tag without its leading v/V), the release page, and the links of the
/// phone app, this machine's agent zip and the checksum file.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: Option<String>,
    pub apk: Option<String>,
    pub agent_zip: Option<String>,
    pub sums: Option<String>,
}

/// .NET's `KeyNotFoundException` text (a missing property of the GitHub answer).
pub const MISSING_KEY: &str = "The given key was not present in the dictionary.";

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "Object",
        Value::Array(_) => "Array",
        Value::String(_) => "String",
        Value::Number(_) => "Number",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Null => "Null",
    }
}

/// .NET's `InvalidOperationException` text for a JSON element of the wrong kind.
fn wrong_kind(expected: &str, v: &Value) -> String {
    format!(
        "The requested operation requires an element of type '{expected}', but the target element has type '{}'.",
        kind(v)
    )
}

/// `JsonElement.GetProperty`.
pub(crate) fn property<'a>(v: &'a Value, key: &str) -> Result<&'a Value, String> {
    match v {
        Value::Object(map) => map.get(key).ok_or_else(|| MISSING_KEY.to_string()),
        other => Err(wrong_kind("Object", other)),
    }
}

/// `JsonElement.GetString`: null for a JSON null, an error for anything but a string.
pub(crate) fn string(v: &Value) -> Result<Option<String>, String> {
    match v {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        other => Err(wrong_kind("String", other)),
    }
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    text.len() >= suffix.len()
        && text.as_bytes()[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// `SelectAssets`: `PhoneBox-*.apk`, `PhoneBoxAgent-*-<rid>.zip` and `SHA256SUMS-*.txt` (any case; the last match
/// wins). A missing or mistyped property fails with .NET's text, as the C# check did.
pub fn select_assets(release: &Value, rid: &str) -> Result<Release, String> {
    let (mut apk, mut agent_zip, mut sums) = (None, None, None);
    let assets = property(release, "assets")?;
    let Value::Array(items) = assets else {
        return Err(wrong_kind("Array", assets));
    };
    let zip_suffix = format!("-{rid}.zip");
    for asset in items {
        let name = string(property(asset, "name")?)?.unwrap_or_default();
        let url = string(property(asset, "browser_download_url")?)?;
        if starts_with_ignore_case(&name, "PhoneBox-") && ends_with_ignore_case(&name, ".apk") {
            apk = url.clone();
        }
        if starts_with_ignore_case(&name, "PhoneBoxAgent-")
            && ends_with_ignore_case(&name, &zip_suffix)
        {
            agent_zip = url.clone();
        }
        if starts_with_ignore_case(&name, "SHA256SUMS-") && ends_with_ignore_case(&name, ".txt") {
            sums = url;
        }
    }
    let tag = string(property(release, "tag_name")?)?.unwrap_or_default();
    let url = string(property(release, "html_url")?)?;
    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        url,
        apk,
        agent_zip,
        sums,
    })
}

/// `Uri.UnescapeDataString`: `%XX` escapes decoded (invalid ones kept as they are).
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `AssetName`: the download link's last path segment, decoded (a trailing `/` stays on it, as `Uri.Segments`); None
/// for a link that is not a URL.
pub fn asset_name(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let path = url.path();
    if path == "/" || path.is_empty() {
        return Some("/".into());
    }
    let (trimmed, slash) = match path.strip_suffix('/') {
        Some(t) => (t, "/"),
        None => (path, ""),
    };
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    Some(format!("{}{slash}", unescape(last)))
}

/// `ExpectedSha256`: the lower-case SHA-256 a `sha256sum` listing gives for `file_name` (`<hex>  name` or
/// `<hex> *name`), or None when it is not listed or the text is not such a listing (an error page saved instead).
pub fn expected_sha256(sums: &str, file_name: &str) -> Option<String> {
    for raw in sums.split('\n') {
        let line = raw.trim_end_matches('\r');
        let bytes = line.as_bytes();
        // the first 65 characters are ASCII when they pass, so byte and character positions agree up to there
        if bytes.len() < 66 || bytes[64] != b' ' || !bytes[..64].iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        if line[65..].trim_start_matches(['*', ' ']) == file_name {
            return Some(line[..64].to_ascii_lowercase());
        }
    }
    None
}

/// `HashMatches`: the file's SHA-256 is `expected_hex` (any case); false when it cannot be read.
pub fn hash_matches(file: &Path, expected_hex: &str) -> bool {
    use sha2::Digest;
    let Ok(mut f) = std::fs::File::open(file) else {
        return false;
    };
    let mut hasher = sha2::Sha256::new();
    if std::io::copy(&mut f, &mut hasher).is_err() {
        return false;
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex == expected_hex.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_099: &str = include_str!("testdata/release-0.9.9.json");
    const SUMS_099: &str = include_str!("testdata/SHA256SUMS-0.9.9.txt");

    fn release() -> serde_json::Value {
        serde_json::from_str(RELEASE_099).unwrap()
    }

    #[test]
    fn every_rid_finds_its_zip_the_app_and_the_checksums_and_each_is_listed() {
        for rid in [
            "win-x64",
            "linux-x64",
            "linux-arm64",
            "osx-arm64",
            "osx-x64",
        ] {
            let r = select_assets(&release(), rid).unwrap();
            assert_eq!(r.version, "0.9.9");
            let zip = asset_name(r.agent_zip.as_deref().unwrap()).unwrap();
            let apk = asset_name(r.apk.as_deref().unwrap()).unwrap();
            assert_eq!(zip, format!("PhoneBoxAgent-0.9.9-{rid}.zip"));
            assert_eq!(apk, "PhoneBox-0.9.9.apk");
            assert_eq!(
                asset_name(r.sums.as_deref().unwrap()).unwrap(),
                "SHA256SUMS-0.9.9.txt"
            );
            assert!(expected_sha256(SUMS_099, &zip).is_some(), "{rid}");
            assert!(expected_sha256(SUMS_099, &apk).is_some());
        }
    }

    #[test]
    fn the_linux_x64_zip_is_not_taken_for_the_arm64_one() {
        let x64 = expected_sha256(SUMS_099, "PhoneBoxAgent-0.9.9-linux-x64.zip").unwrap();
        let arm = expected_sha256(SUMS_099, "PhoneBoxAgent-0.9.9-linux-arm64.zip").unwrap();
        assert_ne!(x64, arm);
    }

    #[test]
    fn listing_formats_are_read() {
        let hash = "a".repeat(64);
        let name = "PhoneBox-1.0.0.apk";
        assert_eq!(
            expected_sha256(&format!("{hash}  {name}\n"), name).as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            expected_sha256(&format!("{hash} *{name}\r\n"), name).as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            expected_sha256(&format!("{}  {name}", hash.to_uppercase()), name).as_deref(),
            Some(hash.as_str())
        );
    }

    #[test]
    fn names_that_are_not_listed_give_nothing() {
        for name in [
            "PhoneBox-0.9.9.apk.idsig",
            "PhoneBox-0.9.9",
            "phonebox-0.9.9.apk",
            "SHA256SUMS-0.9.9.txt",
        ] {
            assert_eq!(expected_sha256(SUMS_099, name), None, "{name}");
        }
    }

    #[test]
    fn text_that_is_not_a_listing_gives_nothing() {
        let name = "PhoneBox-0.9.9.apk";
        for text in [
            "<!DOCTYPE html><html><body>Rate limit</body></html>".to_string(),
            String::new(),
            format!("{}  {name}", "z".repeat(64)),
        ] {
            assert_eq!(expected_sha256(&text, name), None, "{text}");
        }
    }

    #[test]
    fn the_asset_name_comes_from_the_link_not_the_cached_name() {
        assert_eq!(
            asset_name("https://github.com/ersinkoc/phonebox/releases/download/v0.9.9/PhoneBoxAgent-0.9.9-win-x64.zip")
                .as_deref(),
            Some("PhoneBoxAgent-0.9.9-win-x64.zip")
        );
        // percent-decoded, as Uri.UnescapeDataString
        assert_eq!(
            asset_name("https://example.invalid/a/My%20File.zip").as_deref(),
            Some("My File.zip")
        );
    }

    #[test]
    fn a_release_without_a_checksum_file_has_no_sums() {
        let json = serde_json::json!({
            "tag_name": "v1.0.0",
            "html_url": "https://example.invalid",
            "assets": [{ "name": "PhoneBox-1.0.0.apk", "browser_download_url": "https://example.invalid/PhoneBox-1.0.0.apk" }],
        });
        let r = select_assets(&json, "win-x64").unwrap();
        assert_eq!((r.sums, r.agent_zip), (None, None));
        assert!(r.apk.is_some());
    }

    #[test]
    fn a_file_matches_only_its_own_hash() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.apk");
        std::fs::write(&file, "PhoneBox").unwrap();
        let hash: String = sha2::Sha256::digest(b"PhoneBox")
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        assert!(hash_matches(&file, &hash));
        assert!(!hash_matches(&file, &"0".repeat(64)));
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"!")
            .unwrap();
        assert!(!hash_matches(&file, &hash));
    }

    #[test]
    fn a_release_that_misses_a_key_fails_as_dotnet_does() {
        let json = serde_json::json!({ "tag_name": "v1.0.0", "assets": [] });
        assert_eq!(
            select_assets(&json, "win-x64").unwrap_err(),
            "The given key was not present in the dictionary."
        );
    }
}
