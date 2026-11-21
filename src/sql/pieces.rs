//! A statement in lexical pieces, for the readers of statement forms the SQL
//! grammar in use does not know: words, quoted identifiers, string literals and
//! single punctuation characters, each with its place and its parenthesis depth.
//! Comments were removed by the parser before any of them runs.

/// A lexical piece of a statement: where it is, at which parenthesis depth, and
/// whether it is a bare word (a keyword or an unquoted identifier).
pub(crate) struct Piece {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) depth: usize,
    pub(crate) word: bool,
}

/// `sql` in pieces: words, quoted identifiers, string literals and single
/// punctuation characters. Comments were removed by the parser before this runs.
pub(crate) fn pieces(sql: &str) -> Vec<Piece> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if byte == b'\'' || byte == b'"' {
            i += 1;
            while let Some(&inner) = bytes.get(i) {
                i += 1;
                if inner == byte {
                    if bytes.get(i) == Some(&byte) {
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            out.push(Piece {
                start,
                end: i,
                depth,
                word: false,
            });
            continue;
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80 {
            while bytes
                .get(i)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$' || *b >= 0x80)
            {
                i += 1;
            }
            out.push(Piece {
                start,
                end: i,
                depth,
                word: true,
            });
            continue;
        }
        if byte == b')' {
            depth = depth.saturating_sub(1);
        }
        out.push(Piece {
            start,
            end: i + 1,
            depth,
            word: false,
        });
        if byte == b'(' {
            depth += 1;
        }
        i += 1;
    }
    out
}
