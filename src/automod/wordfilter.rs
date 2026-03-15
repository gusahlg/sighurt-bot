use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

/// Zero-width and invisible characters that should be stripped before matching
const ZERO_WIDTH_CHARS: &[char] = &[
    '\u{200B}', // Zero-width space
    '\u{200C}', // Zero-width non-joiner
    '\u{200D}', // Zero-width joiner
    '\u{FEFF}', // Zero-width no-break space / BOM
    '\u{00AD}', // Soft hyphen
    '\u{2060}', // Word joiner
    '\u{180E}', // Mongolian vowel separator
];

/// High-performance word filter using Aho-Corasick algorithm.
/// Provides O(n + m) matching where n = content length, m = total pattern length.
pub struct WordFilter;

impl WordFilter {
    pub fn new() -> Self {
        Self
    }

    /// Build an Aho-Corasick automaton from filtered words.
    /// Returns None if the word list is empty.
    #[inline]
    fn build_automaton(filtered_words: &[String]) -> Option<AhoCorasick> {
        if filtered_words.is_empty() {
            return None;
        }
        AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostFirst)
            .build(filtered_words)
            .ok()
    }

    /// Normalize content to defeat common bypass techniques.
    /// Strips zero-width characters and applies basic leetspeak normalization.
    fn normalize(content: &str) -> String {
        let mut normalized = String::with_capacity(content.len());

        for ch in content.chars() {
            // Skip zero-width / invisible characters
            if ZERO_WIDTH_CHARS.contains(&ch) {
                continue;
            }

            // Skip combining diacritical marks (Unicode category Mn: U+0300..U+036F)
            if ('\u{0300}'..='\u{036F}').contains(&ch) {
                continue;
            }

            // Basic leetspeak normalization
            let normalized_ch = match ch {
                '4' | '@' => 'a',
                '3' => 'e',
                '1' | '!' => 'i',
                '0' => 'o',
                '5' | '$' => 's',
                '7' => 't',
                _ => ch,
            };

            normalized.push(normalized_ch);
        }

        normalized
    }

    /// Check if content contains any filtered words using Aho-Corasick.
    /// This is O(n + m) instead of O(n * m) for linear search.
    /// Also checks a normalized version to catch common bypass techniques.
    #[inline]
    pub fn check(&self, content: &str, filtered_words: &[String]) -> bool {
        let Some(ac) = Self::build_automaton(filtered_words) else {
            return false;
        };

        // Check original content first (fast path)
        if ac.is_match(content) {
            return true;
        }

        // Check normalized content to catch zero-width char and leetspeak bypasses
        let normalized = Self::normalize(content);
        if normalized != content {
            ac.is_match(&normalized)
        } else {
            false
        }
    }

    /// Find all matching filtered words in the content.
    /// Returns references to the matched words from the input slice.
    pub fn find_matches<'a>(&self, content: &str, filtered_words: &'a [String]) -> Vec<&'a String> {
        let Some(ac) = Self::build_automaton(filtered_words) else {
            return Vec::new();
        };

        let mut matched_indices: Vec<usize> = ac
            .find_iter(content)
            .map(|m| m.pattern().as_usize())
            .collect();

        // Also check normalized content
        let normalized = Self::normalize(content);
        if normalized != content {
            let norm_matches: Vec<usize> = ac
                .find_iter(&normalized)
                .map(|m| m.pattern().as_usize())
                .collect();
            matched_indices.extend(norm_matches);
        }

        // Deduplicate indices (same word might match multiple times)
        matched_indices.sort_unstable();
        matched_indices.dedup();

        matched_indices
            .into_iter()
            .filter_map(|i| filtered_words.get(i))
            .collect()
    }
}

impl Default for WordFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_filter_list() {
        let filter = WordFilter::new();
        let words: Vec<String> = vec![];
        assert!(!filter.check("hello world", &words));
        assert!(filter.find_matches("hello world", &words).is_empty());
    }

    #[test]
    fn test_no_match() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string(), "forbidden".to_string()];
        assert!(!filter.check("this is a clean message", &words));
    }

    #[test]
    fn test_exact_match() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string()];
        assert!(filter.check("this has badword in it", &words));
    }

    #[test]
    fn test_case_insensitive() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string()];
        assert!(filter.check("this has BADWORD in it", &words));
        assert!(filter.check("this has BadWord in it", &words));
        assert!(filter.check("this has bAdWoRd in it", &words));
    }

    #[test]
    fn test_substring_match() {
        let filter = WordFilter::new();
        let words = vec!["bad".to_string()];
        assert!(filter.check("this is badword", &words));
        assert!(filter.check("notsobadly", &words));
    }

    #[test]
    fn test_multiple_words() {
        let filter = WordFilter::new();
        let words = vec!["bad".to_string(), "evil".to_string(), "forbidden".to_string()];
        assert!(filter.check("this is evil", &words));
        assert!(filter.check("bad things", &words));
        assert!(filter.check("forbidden zone", &words));
    }

    #[test]
    fn test_find_matches_single() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string(), "forbidden".to_string()];
        let matches = filter.find_matches("this has badword", &words);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "badword");
    }

    #[test]
    fn test_find_matches_multiple() {
        let filter = WordFilter::new();
        let words = vec!["bad".to_string(), "evil".to_string()];
        let matches = filter.find_matches("bad and evil things", &words);
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&&"bad".to_string()));
        assert!(matches.contains(&&"evil".to_string()));
    }

    #[test]
    fn test_find_matches_repeated() {
        let filter = WordFilter::new();
        let words = vec!["bad".to_string()];
        let matches = filter.find_matches("bad bad bad", &words);
        // Should deduplicate - same word appears once
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_unicode_content() {
        let filter = WordFilter::new();
        let words = vec!["test".to_string()];
        assert!(filter.check("🎉 this is a test 🎉", &words));
    }

    #[test]
    fn test_empty_content() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string()];
        assert!(!filter.check("", &words));
    }

    #[test]
    fn test_special_characters_in_words() {
        let filter = WordFilter::new();
        let words = vec!["test.word".to_string()];
        assert!(filter.check("this is a test.word here", &words));
    }

    #[test]
    fn test_overlapping_patterns() {
        let filter = WordFilter::new();
        let words = vec!["abc".to_string(), "bcd".to_string()];
        // "abcd" contains both "abc" and "bcd"
        assert!(filter.check("abcd", &words));
        let matches = filter.find_matches("abcd", &words);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_zero_width_bypass() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string()];
        // Zero-width space inserted in the middle
        assert!(filter.check("b\u{200B}adword", &words));
        // Zero-width joiner
        assert!(filter.check("bad\u{200D}word", &words));
        // Zero-width non-joiner
        assert!(filter.check("badw\u{200C}ord", &words));
        // BOM character
        assert!(filter.check("bad\u{FEFF}word", &words));
    }

    #[test]
    fn test_leetspeak_bypass() {
        let filter = WordFilter::new();
        let words = vec!["badword".to_string()];
        // Basic leetspeak: 4=a, 0=o
        assert!(filter.check("b4dw0rd", &words));
    }

    #[test]
    fn test_combining_marks_bypass() {
        let filter = WordFilter::new();
        let words = vec!["bad".to_string()];
        // Combining acute accent after 'a'
        assert!(filter.check("ba\u{0301}d", &words));
    }

    #[test]
    fn test_normalize_preserves_clean_text() {
        let normalized = WordFilter::normalize("hello world");
        assert_eq!(normalized, "hello world");
    }
}
