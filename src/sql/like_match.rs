//! v01.7: the one SQL `LIKE` matcher, shared by the evaluator, the storage
//! filter pushdown, the PostgreSQL catalog emulation and the MySQL
//! `SHOW … LIKE` helpers, so the same pattern answers the same rows whatever
//! route a statement takes.
//!
//! `%` matches any sequence of characters (newline included), `_` exactly one
//! character, and the escape character (PostgreSQL's default is backslash)
//! makes the next pattern character literal. The match is iterative with one
//! backtrack point (the last `%`), so its worst case is O(text × pattern) and
//! it allocates nothing — no regex is compiled per row, and a pattern with many
//! `%` cannot run away.

/// Why a pattern cannot be used (PostgreSQL SQLSTATE 22025).
pub const LIKE_PATTERN_ENDS_WITH_ESCAPE: &str = "LIKE pattern must not end with escape character";

/// The marker of an ESCAPE clause that is not one character (SQLSTATE 22025).
pub const INVALID_ESCAPE_STRING: &str = "invalid escape string";

enum Token {
    Any,
    One,
    Lit(char),
}

/// The token at byte `p` of `pattern` and the byte offset after it, or `None`
/// at the end or for a trailing escape character.
fn token(pattern: &str, p: usize, escape: Option<char>) -> Option<(Token, usize)> {
    let mut chars = pattern.get(p..)?.chars();
    let c = chars.next()?;
    let after = p + c.len_utf8();
    if Some(c) == escape {
        let next = chars.next()?;
        return Some((Token::Lit(next), after + next.len_utf8()));
    }
    Some(match c {
        '%' => (Token::Any, after),
        '_' => (Token::One, after),
        other => (Token::Lit(other), after),
    })
}

fn chars_equal(a: char, b: char, case_insensitive: bool) -> bool {
    a == b || (case_insensitive && a.to_lowercase().eq(b.to_lowercase()))
}

/// Whether the pattern ends with an unescaped escape character (an error in
/// PostgreSQL). Checked once per call; cheap.
fn ends_with_escape(pattern: &str, escape: Option<char>) -> bool {
    let Some(esc) = escape else {
        return false;
    };
    let mut trailing = false;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == esc {
            match chars.next() {
                Some(_) => trailing = false,
                None => trailing = true,
            }
        } else {
            trailing = false;
        }
    }
    trailing
}

/// `text LIKE pattern [ESCAPE escape]`; `case_insensitive` gives ILIKE.
/// `None` when the pattern ends with the escape character.
pub fn like_match(text: &str, pattern: &str, escape: Option<char>, case_insensitive: bool) -> Option<bool> {
    if ends_with_escape(pattern, escape) {
        return None;
    }
    let mut t = 0usize;
    let mut p = 0usize;
    // (pattern offset after the last `%`, text offset that `%` resumes from)
    let mut backtrack: Option<(usize, usize)> = None;
    loop {
        let text_char = text.get(t..).and_then(|rest| rest.chars().next());
        let matched = match token(pattern, p, escape) {
            Some((Token::Any, np)) => {
                backtrack = Some((np, t));
                p = np;
                continue;
            }
            Some((Token::One, np)) => text_char.map(|c| (t + c.len_utf8(), np)),
            Some((Token::Lit(want), np)) => text_char
                .filter(|c| chars_equal(*c, want, case_insensitive))
                .map(|c| (t + c.len_utf8(), np)),
            None => {
                if text_char.is_none() {
                    return Some(true);
                }
                None
            }
        };
        match matched {
            Some((nt, np)) => {
                t = nt;
                p = np;
            }
            None => {
                // Let the last `%` swallow one more character and retry.
                let Some((bp, bt)) = backtrack else {
                    return Some(false);
                };
                let c = text.get(bt..).and_then(|rest| rest.chars().next());
                let Some(c) = c else {
                    return Some(false);
                };
                let nbt = bt + c.len_utf8();
                backtrack = Some((bp, nbt));
                t = nbt;
                p = bp;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    fn m(text: &str, pattern: &str) -> bool {
        like_match(text, pattern, Some('\\'), false).expect("valid pattern")
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    fn basics() {
        assert!(m("abc", "abc"));
        assert!(!m("abc", "ab"));
        assert!(m("", ""));
        assert!(!m("a", ""));
        assert!(m("", "%"));
        assert!(m("a\nb", "a%b"), "% spans a newline");
        assert!(m("a\nb", "a_b"), "_ matches a newline");
        assert!(m("100%", "100\\%"));
        assert!(!m("100x", "100\\%"));
        assert!(m("a_b", "a\\_b"));
        assert!(!m("axb", "a\\_b"));
        assert!(m("é", "_"), "_ is one character, not one byte");
        assert!(!m("éé", "_"));
        assert!(m("xabcx", "%abc%"));
        assert!(m("abcabc", "%abc"));
        assert!(m("aXbXc", "a%b%c"));
        assert!(!m("ab", "a%b%c"));
        assert!(m("a\\b", "a\\\\b"), "an escaped escape is literal");
        assert_eq!(like_match("a", "a\\", Some('\\'), false), None);
        assert_eq!(like_match("a\\", "a\\", None, false), Some(true), "no escape");
        assert_eq!(like_match("100%", "100!%", Some('!'), false), Some(true));
        assert_eq!(like_match("100x", "100!%", Some('!'), false), Some(false));
        assert_eq!(like_match("ABC", "a_c", Some('\\'), true), Some(true));
        assert_eq!(like_match("ÄB", "äb", Some('\\'), true), Some(true));
        assert_eq!(like_match("ABC", "a_c", Some('\\'), false), Some(false));
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    fn adversarial_patterns_are_fast() {
        let text = "a".repeat(100);
        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert!(!m(&text, "%a%a%a%a%a%a%b"));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
    }

    /// Oracle: the regex translation the evaluator used before, with the
    /// dot-matches-newline flag, over a small exhaustive alphabet.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    fn agrees_with_a_regex_oracle() {
        fn oracle(text: &str, pattern: &str) -> bool {
            let mut re = String::from("(?s)^");
            let mut chars = pattern.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            re.push_str(&regex::escape(&n.to_string()));
                        }
                    }
                    '%' => re.push_str(".*"),
                    '_' => re.push('.'),
                    other => re.push_str(&regex::escape(&other.to_string())),
                }
            }
            re.push('$');
            regex::Regex::new(&re).expect("regex").is_match(text)
        }
        let alphabet = ['a', 'b', '%', '_', '\\', '\n'];
        let mut strings = vec![String::new()];
        for _ in 0..4 {
            let mut next = Vec::new();
            for s in &strings {
                for c in alphabet {
                    let mut t = s.clone();
                    t.push(c);
                    next.push(t);
                }
            }
            strings.extend(next);
            strings.sort();
            strings.dedup();
        }
        let texts: Vec<&String> = strings.iter().filter(|s| s.len() <= 3).collect();
        for pattern in &strings {
            let Some(_) = like_match("", pattern, Some('\\'), false) else {
                continue;
            };
            for text in &texts {
                assert_eq!(
                    m(text, pattern),
                    oracle(text, pattern),
                    "text {text:?} pattern {pattern:?}"
                );
            }
        }
    }
}
