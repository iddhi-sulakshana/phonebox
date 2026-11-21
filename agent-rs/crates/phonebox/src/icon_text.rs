//! Port of `IconText.cs`: repairs icons that an older settings save turned into mojibake (UTF-8 read as Latin-1 or
//! Windows-1252, once or twice).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static REPAIRS: LazyLock<HashMap<String, &'static str>> = LazyLock::new(build_repairs);

/// The intended icon for a known broken form, else the value itself ("" for none).
pub fn repair(value: Option<&str>) -> String {
    match value {
        Some(v) => REPAIRS
            .get(v)
            .map(|s| s.to_string())
            .unwrap_or_else(|| v.to_string()),
        None => String::new(),
    }
}

fn build_repairs() -> HashMap<String, &'static str> {
    // Undefined Windows-1252 bytes keep their C1 values, as in Windows ANSI decoding.
    const C1: [char; 32] = [
        '\u{20ac}', '\u{0081}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02c6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008d}',
        '\u{017d}', '\u{008f}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02dc}', '\u{2122}', '\u{0161}', '\u{203a}',
        '\u{0153}', '\u{009d}', '\u{017e}', '\u{0178}',
    ];
    let mut result = HashMap::new();
    for icon in [
        "\u{2328}\u{fe0f}",
        "\u{2699}\u{fe0f}",
        "\u{2702}\u{fe0f}",
        "\u{2733}\u{fe0f}",
    ] {
        let first: String = icon.chars().take(1).collect();
        let mut forms: HashSet<String> = [icon.to_string(), first].into_iter().collect();
        for _ in 0..2 {
            let mut next = HashSet::new();
            for form in &forms {
                let bytes = form.as_bytes();
                next.insert(bytes.iter().map(|&b| b as char).collect::<String>()); // Latin-1
                next.insert(
                    bytes
                        .iter()
                        .map(|&b| {
                            if (0x80..=0x9f).contains(&b) {
                                C1[(b - 0x80) as usize]
                            } else {
                                b as char
                            }
                        })
                        .collect::<String>(),
                );
            }
            for broken in &next {
                result.insert(broken.clone(), icon);
            }
            forms = next;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::store::ConfigStore;

    /// A config file written as JSON and the store reading it.
    fn fixture(json: serde_json::Value) -> (tempfile::TempDir, std::path::PathBuf, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json.to_string()).unwrap();
        let store = ConfigStore::new(path.clone());
        (dir, path, store)
    }

    fn latin1(bytes: &[u8]) -> String {
        bytes.iter().map(|b| *b as char).collect()
    }

    #[test]
    fn known_double_encoded_icons_are_recovered_without_reencoding_other_text() {
        for (broken, expected) in [
            (
                "\u{00e2}\u{0152}\u{00a8}\u{00ef}\u{00b8}\u{008f}",
                "\u{2328}\u{fe0f}",
            ),
            (
                "\u{00e2}\u{0161}\u{2122}\u{00ef}\u{00b8}\u{008f}",
                "\u{2699}\u{fe0f}",
            ),
            (
                "\u{00e2}\u{0153}\u{201a}\u{00ef}\u{00b8}\u{008f}",
                "\u{2702}\u{fe0f}",
            ),
            (
                "\u{00e2}\u{0153}\u{00b3}\u{00ef}\u{00b8}\u{008f}",
                "\u{2733}\u{fe0f}",
            ),
        ] {
            let (_d, _p, store) = fixture(serde_json::json!({
                "pages": [{"id": "p", "title": "Türkçe — 日本語", "icon": broken, "widgets": [{"type": "note"}]}],
                "shortcuts": [{"id": "s", "label": "Français", "icon": broken, "cmd": "private-command"}],
            }));
            let config = store.get();
            assert_eq!(config.pages[0].icon, expected);
            assert_eq!(config.shortcuts[0].icon, expected);
            assert_eq!(config.pages[0].title, "Türkçe — 日本語");
            assert_eq!(config.shortcuts[0].cmd, "private-command");
        }
    }

    #[test]
    fn saved_icon_strings_survive_ansi_reading_and_keep_other_unicode_text() {
        let symbols = "\u{2328}\u{fe0f} \u{2699}\u{fe0f} \u{2702}\u{fe0f} \u{2733}\u{fe0f} 🏠";
        let (_d, path, store) = fixture(
            serde_json::json!({"shortcuts": [{"id": "s", "icon": symbols, "label": "Türkçe", "cmd": "test"}]}),
        );
        let json = crate::config::write::to_dotnet_json(&store.get());
        store.save(&json, None).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "no byte order mark"
        );
        let ansi: serde_json::Value = serde_json::from_str(&latin1(&bytes)).unwrap();
        assert_eq!(ansi["shortcuts"][0]["icon"], symbols);
        assert_eq!(ConfigStore::new(path).get().shortcuts[0].label, "Türkçe");
    }

    #[test]
    fn existing_broken_icons_are_persisted_once_with_an_original_backup() {
        let (_d, path, store) = fixture(serde_json::json!({
            "pages": [{"id": "p", "icon": "\u{00e2}\u{0152}\u{00a8}\u{00ef}\u{00b8}\u{008f}", "widgets": [{"type": "note"}]}],
            // stable keys first, so the icon is what needs writing, not key generation
            "lan": {"token": "stable-phone", "webhookToken": "stable-webhook"},
        }));
        let before = std::fs::read(&path).unwrap();
        store.persist_generated();
        assert_eq!(
            ConfigStore::new(path.clone()).get().pages[0].icon,
            "\u{2328}\u{fe0f}"
        );
        assert_eq!(
            std::fs::read(path.with_extension("json.bak")).unwrap(),
            before
        );
        let after = std::fs::read(&path).unwrap();
        store.persist_generated();
        assert_eq!(std::fs::read(&path).unwrap(), after);
    }

    #[test]
    fn latin1_and_repeated_ansi_corruption_are_recovered_by_exact_match_only() {
        for icon in ["⌨️", "⚙️", "✂️", "✳️"] {
            let mut broken = icon.to_string();
            for _ in 0..2 {
                broken = latin1(broken.as_bytes());
                assert_eq!(repair(Some(&broken)), icon);
            }
            let custom = format!("custom {broken}");
            assert_eq!(repair(Some(&custom)), custom);
        }
    }

    #[test]
    fn valid_emoji_and_custom_icon_text_are_unchanged() {
        for icon in ["⌨️", "⚙️", "✂️", "✳️", "🏠", "DB", "İş", "â custom", "λ"]
        {
            let (_d, _p, store) = fixture(
                serde_json::json!({"shortcuts": [{"id": "s", "icon": icon, "cmd": "test"}]}),
            );
            assert_eq!(store.get().shortcuts[0].icon, icon);
        }
    }
}
