//! One `COM_QUERY` packet as the statements it holds.
//!
//! A MySQL client that negotiated `CLIENT_MULTI_STATEMENTS` may send several
//! statements separated by `;` in one packet (PDO does by default; so do
//! `mysqli::multi_query`, mysql2 and Go's driver with `multiStatements`). Each
//! one is run and answered by itself. The split is done on the text the client
//! sent, before any rewriting, with MySQL's own rules for where a `;` is text:
//! inside `'...'` and `"..."` (backslash escapes and doubled quotes), inside
//! `` `...` ``, and inside `-- `, `#` and `/* */` comments.

/// The statements of `sql`, in order, without their separators. Empty
/// statements (a trailing `;`, `;;`, or a piece that is only comments) are
/// left out.
pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\'' | b'"' | b'`' => {
                i += 1;
                while let Some(&c) = bytes.get(i) {
                    if c == b'\\' && b != b'`' {
                        i += 2;
                        continue;
                    }
                    if c == b {
                        if bytes.get(i + 1) == Some(&b) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes.get(i) == Some(&b'*') && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-')
                && bytes
                    .get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()) =>
            {
                while bytes.get(i).is_some_and(|c| *c != b'\n') {
                    i += 1;
                }
            }
            // `#` opens a comment, except in the operators `#>`, `#>>`, `#-`, `<#>`.
            b'#' if !matches!(bytes.get(i + 1), Some(b'>' | b'-')) && (i == 0 || bytes.get(i - 1) != Some(&b'<')) => {
                while bytes.get(i).is_some_and(|c| *c != b'\n') {
                    i += 1;
                }
            }
            b';' => {
                keep(&mut out, sql, start, i);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    keep(&mut out, sql, start, sql.len());
    out
}

fn keep<'a>(out: &mut Vec<&'a str>, sql: &'a str, from: usize, to: usize) {
    if let Some(piece) = sql.get(from..to) {
        let piece = piece.trim();
        if !super::translator::skip_leading_comments(piece).trim().is_empty() {
            out.push(piece);
        }
    }
}

/// The statement inside a version comment that is a whole `SET` statement,
/// `/*!40101 SET NAMES utf8mb4 */`: MySQL runs it, and a dump's settings
/// (`FOREIGN_KEY_CHECKS=0`, `SQL_MODE`, the saved `@OLD_...` values) depend on
/// that. Other version comments keep being skipped as comments.
pub(crate) fn versioned_set(statement: &str) -> Option<&str> {
    let inner = statement.trim().strip_prefix("/*!")?.strip_suffix("*/")?;
    if inner.contains("*/") {
        return None;
    }
    let inner = inner.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
    super::vars::strip_keyword(inner, "SET").map(|_| inner)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_split_at_its_separators_only() {
        assert_eq!(split_statements("SELECT 1"), ["SELECT 1"]);
        assert_eq!(split_statements("SELECT 1;"), ["SELECT 1"]);
        assert_eq!(split_statements("SET NAMES utf8mb4; INSERT INTO t VALUES (1);;"), [
            "SET NAMES utf8mb4",
            "INSERT INTO t VALUES (1)"
        ]);
        assert_eq!(
            split_statements("INSERT INTO t VALUES ('a;b', \"c;d\", 'it\\'s;', 'x'';'); SELECT `a;b` FROM t"),
            [
                "INSERT INTO t VALUES ('a;b', \"c;d\", 'it\\'s;', 'x'';')",
                "SELECT `a;b` FROM t"
            ]
        );
        assert_eq!(
            split_statements("SELECT 1 /* ; */; -- two; three\nSELECT 2; # four;\nSELECT 3"),
            ["SELECT 1 /* ; */", "-- two; three\nSELECT 2", "# four;\nSELECT 3"]
        );
        assert_eq!(split_statements("SELECT a #> '{x}' FROM t; SELECT 2"), [
            "SELECT a #> '{x}' FROM t",
            "SELECT 2"
        ]);
        assert!(split_statements(" ; /* only a comment */ ; ").is_empty());
        // An unterminated literal is one statement to the end, never a panic.
        assert_eq!(split_statements("SELECT 'a; SELECT 2"), ["SELECT 'a; SELECT 2"]);
    }

    #[test]
    fn a_version_comment_that_is_a_set_statement_is_unwrapped() {
        assert_eq!(
            versioned_set("/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */"),
            Some("SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0")
        );
        assert_eq!(versioned_set("/*!50503 SET NAMES utf8mb4 */"), Some("SET NAMES utf8mb4"));
        assert_eq!(versioned_set("/*!40000 ALTER TABLE `t` DISABLE KEYS */"), None);
        assert_eq!(versioned_set("/* SET x = 1 */"), None);
        assert_eq!(versioned_set("/*!40101 SET a=1 */ SELECT 1 /* x */"), None);
    }
}
