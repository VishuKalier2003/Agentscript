// Glob matching for repository paths ("/" separators): "**" matches any number of whole path
// segments, "*" any run of characters within a segment, "?" one character within a segment, and a
// pattern ending in "/" matches everything under that directory.

/** Check whether a repository path matches a glob pattern
 * Input
    - pattern: &str - glob, such as a directory followed by a double star, or "*.py"
    - path: &str - repository-relative path
 * Output
    - bool
*/
pub(crate) fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./");
    let pattern = if pattern.ends_with('/') {
        format!("{pattern}**")
    } else {
        pattern.to_string()
    };
    let parts = pattern.split('/').collect::<Vec<_>>();
    let segments = path.split('/').collect::<Vec<_>>();
    segments_match(&parts, &segments)
}

/** Match pattern segments against path segments, expanding "**"
 * Input
    - parts: &[&str] - pattern segments
    - segments: &[&str] - path segments
 * Output
    - bool
*/
fn segments_match(parts: &[&str], segments: &[&str]) -> bool {
    match parts.first() {
        None => segments.is_empty(),
        Some(&"**") => {
            (0..=segments.len()).any(|skip| segments_match(&parts[1..], &segments[skip..]))
        }
        Some(part) => {
            !segments.is_empty()
                && segment_matches(part, segments[0])
                && segments_match(&parts[1..], &segments[1..])
        }
    }
}

/** Match one segment with "*" and "?" wildcards
 * Input
    - pattern: &str - pattern segment
    - text: &str - path segment
 * Output
    - bool
*/
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut resume) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(position) = star {
            p = position + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::matches;

    /** Check globstar, single-segment wildcards, and directory patterns
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn matches_globs() {
        assert!(matches("src/payments/**", "src/payments/a/b.py"));
        assert!(matches("src/payments/**", "src/payments/b.py"));
        assert!(!matches("src/payments/**", "src/pay/b.py"));
        assert!(matches("**/*.py", "a/b/c.py"));
        assert!(matches("**/*.py", "c.py"));
        assert!(matches("app/*.py", "app/x.py"));
        assert!(!matches("app/*.py", "app/sub/x.py"));
        assert!(matches("app/", "app/sub/x.py"));
        assert!(matches("a?c.rs", "abc.rs"));
        assert!(matches("app/payments.py", "app/payments.py"));
    }
}
