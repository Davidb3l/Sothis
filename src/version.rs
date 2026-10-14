use semver::Version;

pub fn extract(input: &str) -> Option<Version> {
    input.split_whitespace().find_map(|part| {
        let trimmed = part.trim_matches(|ch: char| matches!(ch, '(' | ')' | '[' | ']' | ',' | ';'));
        let candidate = trimmed.strip_prefix('v').unwrap_or(trimmed);
        Version::parse(candidate).ok()
    })
}

pub fn meets_minimum(actual: &str, minimum: &str) -> Option<bool> {
    let actual = extract(actual)?;
    let minimum = Version::parse(minimum).ok()?;
    Some(actual >= minimum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_versions_from_tool_output() {
        assert_eq!(extract("amt 0.2.0"), Some(Version::new(0, 2, 0)));
        assert_eq!(extract("0.0.7\n"), Some(Version::new(0, 0, 7)));
        assert_eq!(extract("sirius v1.4.0"), Some(Version::new(1, 4, 0)));
    }

    #[test]
    fn compares_semantic_versions() {
        assert_eq!(meets_minimum("1.2.0", "1.2.0"), Some(true));
        assert_eq!(meets_minimum("1.1.9", "1.2.0"), Some(false));
        assert_eq!(meets_minimum("2.0.0", "1.99.99"), Some(true));
        assert_eq!(meets_minimum("1.2.0-alpha.1", "1.2.0"), Some(false));
        assert_eq!(meets_minimum("not-a-version", "1.2.0"), None);
    }
}
