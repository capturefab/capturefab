//! Small text helpers for human hints: typo distance and shell quoting.

#![forbid(unsafe_code)]
/// Case-insensitive Levenshtein distance, counted in characters.
pub(crate) fn edit_distance(left: &str, right: &str) -> usize {
    lowercase_distance(&left.to_lowercase(), &right.to_lowercase())
}

fn lowercase_distance(left: &str, right: &str) -> usize {
    if left == right {
        return 0;
    }
    // Shared ends contribute no edits. Count whole characters so both
    // slices stay on UTF-8 boundaries, including case-expanded characters.
    let prefix: usize = left
        .chars()
        .zip(right.chars())
        .take_while(|(a, b)| a == b)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let (left, right) = (left.split_at(prefix).1, right.split_at(prefix).1);
    let suffix: usize = left
        .chars()
        .rev()
        .zip(right.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let (left, right) = (
        left.split_at(left.len() - suffix).0,
        right.split_at(right.len() - suffix).0,
    );
    let (left_len, right_len) = (left.chars().count(), right.chars().count());
    if left_len == 0 {
        return right_len;
    }
    if right_len == 0 {
        return left_len;
    }
    // Distance is symmetric; keep the row on the shorter input.
    let (left, right) = if left_len < right_len {
        (right, left)
    } else {
        (left, right)
    };
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.chars().enumerate() {
        let mut diagonal = previous[0];
        previous[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let above = previous[j + 1];
            previous[j + 1] = (diagonal + usize::from(a != *b))
                .min(above + 1)
                .min(previous[j] + 1);
            diagonal = above;
        }
    }
    previous[right.len()]
}

/// The distance to a plausible typo: a prefix of one another, or within
/// about a third of the typed length.
pub(crate) fn close_distance(typed: &str, candidate: &str) -> Option<usize> {
    let typed_lower = typed.to_lowercase();
    let candidate_lower = candidate.to_lowercase();
    if typed_lower.is_empty() || candidate_lower.is_empty() {
        return None;
    }
    let length = typed_lower.chars().count();
    let prefix = length >= 3
        && candidate_lower.chars().count() >= 3
        && (candidate_lower.starts_with(&typed_lower) || typed_lower.starts_with(&candidate_lower));
    let distance = lowercase_distance(&typed_lower, &candidate_lower);
    (prefix || distance <= length.div_ceil(3)).then_some(distance)
}

/// `value` as one shell word: unchanged when it is plainly safe, otherwise
/// in single quotes.
pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@=+,%".contains(c))
    {
        value.into()
    } else {
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('\'');
        for (index, part) in value.split('\'').enumerate() {
            if index > 0 {
                quoted.push_str("'\\''");
            }
            quoted.push_str(part);
        }
        quoted.push('\'');
        quoted
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
        let long = "a".repeat(4096);
        assert_eq!(edit_distance("a", &long), 4095);
        assert_eq!(edit_distance(&long, "a"), 4095);
    }

    #[test]
    fn reused_distance_row_matches_a_full_matrix_for_unicode_and_repeated_letters() {
        fn reference(left: &str, right: &str) -> usize {
            let left: Vec<_> = left.to_lowercase().chars().collect();
            let right: Vec<_> = right.to_lowercase().chars().collect();
            let mut matrix = vec![vec![0; right.len() + 1]; left.len() + 1];
            for (index, row) in matrix.iter_mut().enumerate() {
                row[0] = index;
            }
            for (index, cell) in matrix[0].iter_mut().enumerate() {
                *cell = index;
            }
            for (i, a) in left.iter().enumerate() {
                for (j, b) in right.iter().enumerate() {
                    matrix[i + 1][j + 1] = (matrix[i][j] + usize::from(a != b))
                        .min(matrix[i][j + 1] + 1)
                        .min(matrix[i + 1][j] + 1);
                }
            }
            matrix[left.len()][right.len()]
        }
        let words = [
            "", "a", "ab", "ba", "aab", "abab", "baba", "é", "e\u{301}", "İ", "i\u{307}", "ΟΣ",
            "ος", "οσ", "🚀a", "kitten", "sitting",
        ];
        for left in words {
            for right in words {
                assert_eq!(
                    edit_distance(left, right),
                    reference(left, right),
                    "{left:?}, {right:?}"
                );
                let (left, right) = (format!("🚀{left}é"), format!("🚀{right}é"));
                assert_eq!(edit_distance(&left, &right), reference(&left, &right));
            }
        }
    }

    #[test]
    fn close_names_are_typos_not_strangers() {
        assert_eq!(close_distance("bnech", "bench"), Some(2));
        assert_eq!(close_distance("benc", "bench"), Some(1));
        assert_eq!(close_distance("lab", "bench"), None);
        assert_eq!(close_distance("", "bench"), None);
        assert_eq!(close_distance("bench", ""), None);
        assert_eq!(close_distance("cam", "CAMeraLongName"), Some(11));
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
        for value in [
            "'",
            "''",
            "'a'",
            "a''b",
            "é '🚀'",
            "a\nb",
            "$(echo x)",
            "\\'",
        ] {
            assert_eq!(
                shell_quote(value),
                format!("'{}'", value.replace('\'', "'\\''"))
            );
        }
    }
}
