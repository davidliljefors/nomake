//! Version requirements: a requirement is a version or a dotted prefix of one (`5.4` matches `5.4.7`).

use std::cmp::Ordering;

/// Whether `version` equals `requirement` or starts with `requirement` plus a dot.
pub fn matches(version: &str, requirement: &str) -> bool {
    version == requirement
        || version
            .strip_prefix(requirement)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Numeric parts compare as numbers, so `2.13` is above `2.9`.
pub fn compare(left: &str, right: &str) -> Ordering {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => {
                let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    _ => a.cmp(b),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// `5.4.7` -> `5.4`: the series of a version, a requirement that still allows its patch releases.
pub fn series(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    parts[..parts.len().min(2)].join(".")
}

/// The distinct series of `versions`, in the order they first appear.
pub fn series_of(versions: &[String]) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for version in versions {
        let found = series(version);
        if !all.contains(&found) {
            all.push(found);
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_drop_the_patch_number() {
        assert_eq!(series("5.4.7"), "5.4");
        assert_eq!(series("1.90"), "1.90");
        assert_eq!(series("3"), "3");
        let versions: Vec<String> = ["5.4.8", "5.4.7", "5.3.6"]
            .iter()
            .map(|v| v.to_string())
            .collect();
        assert_eq!(series_of(&versions), ["5.4", "5.3"]);
    }

    #[test]
    fn requirements_match_whole_dotted_prefixes() {
        assert!(matches("2.13.2", "2.13") && matches("2.13", "2.13") && matches("2.13.2", "2"));
        assert!(!matches("2.130.0", "2.13") && !matches("3.0.0", "2"));
    }

    #[test]
    fn numbers_compare_as_numbers() {
        assert_eq!(compare("2.13.0", "2.9.1"), Ordering::Greater);
        assert_eq!(compare("2.13", "2.13.0"), Ordering::Less);
    }
}
