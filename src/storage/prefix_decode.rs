//! Partial decode of a row's bincode blob: materialize only the first `prefix_len`
//! column values (padding the rest with `Null`) so projected / aggregate scans don't
//! pay to deserialize trailing columns they never read.
//!
//! Issue #1 follow-up: on a wide table the costly columns (large text, JSON, vectors)
//! sit at the row tail, while hot analytical queries reference early columns
//! (`session_id`, `type`, `input_tokens`). Decoding only the needed prefix and
//! stopping skips the expensive tail entirely.
//!
//! Correctness rests on the caller passing `prefix_len >= every column index the
//! query references`; callers fall back to a full decode whenever that set is
//! uncertain (wildcards, joins, subqueries, unresolved columns). Tuples stay
//! full-width (tail padded with `Null`), so downstream column indices are unchanged —
//! the padding columns are, by construction, never read.

use crate::types::{Tuple, Value};
use bincode::Options;
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use std::fmt;

struct PrefixValues {
    prefix_len: usize,
    total_cols: usize,
}

impl<'de> DeserializeSeed<'de> for PrefixValues {
    type Value = Vec<Value>;

    fn deserialize<D>(self, d: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V {
            prefix_len: usize,
            total_cols: usize,
        }
        impl<'de> Visitor<'de> for V {
            type Value = Vec<Value>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a column-value sequence")
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Vec<Value>, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut out = Vec::with_capacity(self.total_cols.max(self.prefix_len));
                // Pull only the prefix; stop early and leave the tail unread.
                while out.len() < self.prefix_len {
                    match seq.next_element::<Value>()? {
                        Some(v) => out.push(v),
                        None => break, // row narrower than prefix_len
                    }
                }
                // Pad to full width so downstream positional indexing is unaffected.
                while out.len() < self.total_cols {
                    out.push(Value::Null);
                }
                Ok(out)
            }
        }
        d.deserialize_seq(V {
            prefix_len: self.prefix_len,
            total_cols: self.total_cols,
        })
    }
}

/// Options matching the top-level `bincode::serialize`/`deserialize` used to write rows
/// (fixed-int, little-endian), but tolerating the trailing bytes we deliberately leave
/// unread (the skipped tail values and the serialized `row_id`).
fn row_blob_opts() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .allow_trailing_bytes()
}

/// Decode a row blob into a full-width `Tuple`, materializing only the first
/// `prefix_len` values (the rest are `Value::Null`). `row_id` is left `None`; the scan
/// recovers it from the row key.
pub(crate) fn decode_tuple_prefix(bytes: &[u8], prefix_len: usize, total_cols: usize) -> bincode::Result<Tuple> {
    let values = row_blob_opts().deserialize_seed(PrefixValues { prefix_len, total_cols }, bytes)?;
    Ok(Tuple {
        values,
        row_id: None,
        branch_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tuple {
        Tuple::new(vec![
            Value::Int4(1),
            Value::String("sess-2".into()),
            Value::Boolean(true),
            Value::Int8(99),
            // "expensive tail" stand-ins:
            Value::String("a-large-prompt-body".into()),
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        ])
    }

    #[test]
    fn prefix_decode_matches_full_then_pads() {
        let t = sample();
        let bytes = bincode::serialize(&t).unwrap();
        let full: Tuple = bincode::deserialize(&bytes).unwrap();
        assert_eq!(full.values.len(), 6);

        // prefix 2: first two real, rest Null, width preserved.
        let p = decode_tuple_prefix(&bytes, 2, 6).unwrap();
        assert_eq!(p.values.len(), 6);
        assert_eq!(p.values[0], Value::Int4(1));
        assert_eq!(p.values[1], Value::String("sess-2".into()));
        assert_eq!(p.values[2], Value::Null);
        assert_eq!(p.values[5], Value::Null);

        // prefix 4: stops before the expensive tail (string + vector).
        let p4 = decode_tuple_prefix(&bytes, 4, 6).unwrap();
        assert_eq!(&p4.values[..4], &full.values[..4]);
        assert_eq!(p4.values[4], Value::Null);
        assert_eq!(p4.values[5], Value::Null);

        // prefix >= width: identical to a full decode.
        let pall = decode_tuple_prefix(&bytes, 6, 6).unwrap();
        assert_eq!(pall.values, full.values);
        let pover = decode_tuple_prefix(&bytes, 10, 6).unwrap();
        assert_eq!(pover.values, full.values);
    }
}
