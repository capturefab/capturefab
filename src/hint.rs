//! Small text helpers for human hints: typo distance and shell quoting.
//! Always compiled, so builds without the window share them.

/// Case-insensitive Levenshtein distance, counted in characters.
pub fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.to_lowercase().chars().collect();
    let right: Vec<char> = right.to_lowercase().chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.iter().enumerate() {
        let mut current = vec![i + 1; right.len() + 1];
        for (j, b) in right.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(a != b))
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        previous = current;
    }
    previous[right.len()]
}

/// Whether `candidate` is a plausible typo of `typed`: a prefix of one
/// another, or within about a third of the typed length.
pub fn is_close(typed: &str, candidate: &str) -> bool {
    let typed_lower = typed.to_lowercase();
    let candidate_lower = candidate.to_lowercase();
    if typed_lower.is_empty() || candidate_lower.is_empty() {
        return false;
    }
    let length = typed_lower.chars().count();
    (length >= 3
        && candidate_lower.chars().count() >= 3
        && (candidate_lower.starts_with(&typed_lower) || typed_lower.starts_with(&candidate_lower)))
        || edit_distance(typed, candidate) <= length.div_ceil(3)
}

/// `value` as one shell word: unchanged when it is plainly safe, otherwise
/// in single quotes.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@=+,%".contains(c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_edits_without_case() {
        assert_eq!(edit_distance("bench", "bnech"), 2);
        assert_eq!(edit_distance("SIM0", "sim:0"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("Kamera", "kamera"), 0);
    }

    #[test]
    fn close_names_are_typos_not_strangers() {
        assert!(is_close("bnech", "bench"));
        assert!(is_close("benc", "bench"));
        assert!(!is_close("lab", "bench"));
        assert!(!is_close("", "bench"));
    }

    #[test]
    fn quoting_keeps_plain_words_and_quotes_the_rest() {
        assert_eq!(shell_quote("sim:0"), "sim:0");
        assert_eq!(shell_quote("gige:192.168.1.20"), "gige:192.168.1.20");
        assert_eq!(
            shell_quote("avfoundation:HD Pro Webcam C920"),
            "'avfoundation:HD Pro Webcam C920'"
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
