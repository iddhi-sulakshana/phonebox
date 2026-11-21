//! Which of two versions is newer. C#'s `Newer` read both with `System.Version` (2–4 numbers, a missing one counting
//! as −1) and cut the running version at `-`, so a pre-release was never newer and an alpha never saw 1.0.0. Here the
//! numbers keep `System.Version`'s rules and a SemVer pre-release (`-alpha.2`) orders below its release and by its
//! identifiers; `+build` is ignored (a deliberate departure, plan 7).

use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
enum Identifier {
    Number(u64),
    Text(String),
}

#[derive(Debug)]
struct Version {
    numbers: [i64; 4],
    pre: Option<Vec<Identifier>>,
}

fn without_build(s: &str) -> &str {
    s.split_once('+').map_or(s, |(kept, _)| kept)
}

fn parse(text: &str) -> Option<Version> {
    let text = text.trim();
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(without_build(pre))),
        None => (without_build(text), None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    let mut numbers = [-1i64; 4];
    for (slot, part) in numbers.iter_mut().zip(&parts) {
        let n = part.trim().parse::<i32>().ok().filter(|n| *n >= 0)?;
        *slot = i64::from(n);
    }
    let pre = match pre {
        None => None,
        Some(pre) => Some(
            pre.split('.')
                .map(|id| match id {
                    "" => None,
                    id if id.bytes().all(|b| b.is_ascii_digit()) => Some(
                        id.parse::<u64>()
                            .map_or_else(|_| Identifier::Text(id.to_string()), Identifier::Number),
                    ),
                    id => Some(Identifier::Text(id.to_string())),
                })
                .collect::<Option<Vec<_>>>()?,
        ),
    };
    Some(Version { numbers, pre })
}

fn compare(a: &Version, b: &Version) -> Ordering {
    a.numbers
        .cmp(&b.numbers)
        .then_with(|| match (&a.pre, &b.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x
                .iter()
                .zip(y)
                .map(|pair| match pair {
                    (Identifier::Number(m), Identifier::Number(n)) => m.cmp(n),
                    (Identifier::Number(_), Identifier::Text(_)) => Ordering::Less,
                    (Identifier::Text(_), Identifier::Number(_)) => Ordering::Greater,
                    (Identifier::Text(s), Identifier::Text(t)) => s.cmp(t),
                })
                .find(|o| o.is_ne())
                .unwrap_or_else(|| x.len().cmp(&y.len())),
        })
}

/// Whether the text reads as a version.
pub fn parses(text: &str) -> bool {
    parse(text).is_some()
}

/// Whether `candidate` is a newer version than `current`; false when either does not read as a version.
pub fn newer(candidate: &str, current: &str) -> bool {
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if compare(&a, &b) == Ordering::Greater)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_system_version_with_pre_releases() {
        for (candidate, current) in [
            ("1.0.0-alpha.2", "1.0.0-alpha.1"),
            ("1.0.0", "1.0.0-alpha.9"),
            ("1.0.0-alpha.1", "0.9.23"),
            ("1.0.0-alpha.10", "1.0.0-alpha.9"),
            ("1.0.0", "1.0"),
            ("0.9.24", "0.9.23"),
            ("1.0.0-beta", "1.0.0-alpha.9"),
            ("1.0.0-alpha.1", "1.0.0-alpha"),
            ("1.0.0-alpha", "1.0.0-1"),
            ("1.0.1+build.5", "1.0.0"),
        ] {
            assert!(newer(candidate, current), "{candidate} > {current}");
        }
        for (candidate, current) in [
            ("0.9.23", "1.0.0-alpha.1"),
            ("1.0.0", "1.0.0"),
            ("1.0.0-alpha.1", "1.0.0-alpha.1"),
            ("garbage", "1.0.0"),
            ("1.0.0", "garbage"),
            ("1.0", "1.0.0"),
            ("v1.0.1", "1.0.0"),
            ("1", "0.1"),
            ("1.0.0.0.0", "0.1"),
            ("1.-1.0", "0.1"),
            ("1.0.0-", "0.1"),
            ("1.0.0-alpha..1", "0.1"),
        ] {
            assert!(
                !newer(candidate, current),
                "{candidate} is not newer than {current}"
            );
        }
    }
}
