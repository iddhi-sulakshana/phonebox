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
