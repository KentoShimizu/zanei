//! Privacy-preserving text delta extraction.

const ZERO_WIDTH_JOINER: char = '\u{200d}';

/// Returns the current changed region when it has net grapheme-cluster growth.
///
/// Deletions and same-size replacements are suppressed. Callers decide whether
/// the observed value change followed user input before exposing this result.
#[must_use]
pub fn text_delta(baseline: &str, current: &str) -> Option<String> {
    if baseline.is_empty() {
        return (!current.is_empty()).then(|| current.to_owned());
    }

    let baseline_clusters = grapheme_clusters(baseline);
    let current_clusters = grapheme_clusters(current);
    let common_prefix_len = baseline_clusters
        .iter()
        .zip(&current_clusters)
        .take_while(|(baseline, current)| baseline == current)
        .count();
    let max_suffix_len = baseline_clusters
        .len()
        .min(current_clusters.len())
        .saturating_sub(common_prefix_len);
    let common_suffix_len = baseline_clusters
        .iter()
        .rev()
        .zip(current_clusters.iter().rev())
        .take(max_suffix_len)
        .take_while(|(baseline, current)| baseline == current)
        .count();

    let baseline_changed_len = baseline_clusters.len() - common_prefix_len - common_suffix_len;
    let current_changed_len = current_clusters.len() - common_prefix_len - common_suffix_len;
    if current_changed_len <= baseline_changed_len {
        return None;
    }

    Some(current_clusters[common_prefix_len..current_clusters.len() - common_suffix_len].concat())
}

fn grapheme_clusters(value: &str) -> Vec<&str> {
    let mut characters = value.char_indices();
    let Some((_, first)) = characters.next() else {
        return Vec::new();
    };

    let mut clusters = Vec::new();
    let mut cluster_start = 0;
    let mut previous_was_joiner = first == ZERO_WIDTH_JOINER;
    for (index, character) in characters {
        if !is_cluster_extension(character)
            && character != ZERO_WIDTH_JOINER
            && !previous_was_joiner
        {
            clusters.push(&value[cluster_start..index]);
            cluster_start = index;
        }
        previous_was_joiner = character == ZERO_WIDTH_JOINER;
    }
    clusters.push(&value[cluster_start..]);
    clusters
}

fn is_cluster_extension(character: char) -> bool {
    matches!(
        character as u32,
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0x1f3fb..=0x1f3ff
            | 0xe0100..=0xe01ef
    )
}

#[cfg(test)]
mod tests {
    use super::text_delta;

    #[test]
    fn returns_appended_text() {
        assert_eq!(
            text_delta("hello", "hello world").as_deref(),
            Some(" world")
        );
    }

    #[test]
    fn returns_text_inserted_in_the_middle() {
        assert_eq!(
            text_delta("hello world", "hello brave world").as_deref(),
            Some("brave ")
        );
    }

    #[test]
    fn suppresses_deletion() {
        assert_eq!(text_delta("hello world", "hello"), None);
    }

    #[test]
    fn suppresses_same_size_full_replacement() {
        assert_eq!(text_delta("before", "after!"), None);
    }

    #[test]
    fn keeps_combining_marks_and_zwj_sequences_on_cluster_boundaries() {
        assert_eq!(
            text_delta("ab", "ae\u{301}\u{200d}\u{1f4bb}b").as_deref(),
            Some("e\u{301}\u{200d}\u{1f4bb}")
        );
    }

    #[test]
    fn suppresses_growth_inside_an_existing_grapheme_cluster() {
        assert_eq!(text_delta("aeb", "ae\u{301}b"), None);
    }

    #[test]
    fn returns_the_entire_value_when_the_baseline_is_empty() {
        assert_eq!(text_delta("", "hello").as_deref(), Some("hello"));
    }
}
