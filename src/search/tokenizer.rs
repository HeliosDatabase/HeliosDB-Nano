//! Unicode-aware tokenizer for full-text indexing.
//!
//! HelixDB-inspired (idea 2).
//!
//! Splits input on Unicode word boundaries, drops anything that isn't
//! an alphanumeric word, and lowercases the survivors. Suitable as a
//! BM25 default; downstream callers that need stemming or stopword
//! removal can layer those passes on top.

use unicode_segmentation::UnicodeSegmentation;

/// Tokenize a string into a list of normalised terms.
///
/// - Splits on Unicode word boundaries.
/// - Keeps only segments that contain at least one alphanumeric
///   character (drops punctuation, whitespace).
/// - Lowercases each surviving segment.
#[must_use]
pub fn tokenize(input: &str) -> Vec<String> {
    input
        .unicode_words()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_words_lowercased() {
        let toks = tokenize("Hello, World!");
        assert_eq!(toks, vec!["hello", "world"]);
    }

    #[test]
    fn drops_punctuation_only_segments() {
        let toks = tokenize("foo... bar??? baz");
        assert_eq!(toks, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn handles_unicode_words() {
        let toks = tokenize("Café résumé naïve");
        assert_eq!(toks, vec!["café", "résumé", "naïve"]);
    }

    #[test]
    fn handles_cjk_words() {
        // unicode-segmentation splits CJK into per-character words by default.
        let toks = tokenize("日本語");
        assert!(!toks.is_empty());
        assert!(toks.iter().all(|t| !t.is_empty()));
    }

    #[test]
    fn empty_input() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
        assert!(tokenize("!!!").is_empty());
    }

    #[test]
    fn alphanumeric_kept() {
        let toks = tokenize("abc123 def-456");
        assert!(toks.contains(&"abc123".to_string()));
        assert!(toks.contains(&"def".to_string()));
        assert!(toks.contains(&"456".to_string()));
    }
}
