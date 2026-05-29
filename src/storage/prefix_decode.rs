//! Partial decode of a row's bincode blob: materialize only the referenced column
//! values (padding the rest with `Null`) so projected / aggregate scans don't pay to
//! deserialize columns they never read.
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

struct SelectedValues<'a> {
    columns: &'a [usize],
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
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

impl<'a, 'de> DeserializeSeed<'de> for SelectedValues<'a> {
    type Value = Vec<Value>;

    fn deserialize<D>(self, d: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'a> {
            columns: &'a [usize],
            total_cols: usize,
        }
        impl<'a, 'de> Visitor<'de> for V<'a> {
            type Value = Vec<Value>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a column-value sequence")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Vec<Value>, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut out = vec![Value::Null; self.total_cols];
                let mut wanted = self.columns.iter().copied().peekable();

                for idx in 0..self.total_cols {
                    match wanted.peek().copied() {
                        Some(want) if want == idx => {
                            match seq.next_element::<Value>()? {
                                Some(v) => out[idx] = v,
                                None => break,
                            }
                            wanted.next();
                            if wanted.peek().is_none() {
                                break;
                            }
                        }
                        Some(_) => {
                            if seq.next_element::<Value>()?.is_none() {
                                break;
                            }
                        }
                        None => break,
                    }
                }

                Ok(out)
            }
        }

        d.deserialize_seq(V {
            columns: self.columns,
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

/// Decode a row blob into a full-width `Tuple`, materializing only the sorted,
/// unique column indexes in `columns`. Unrequested values before the last requested
/// column are skipped by the fast bincode cursor when possible, and the tail is
/// left unread.
pub(crate) fn decode_tuple_columns(bytes: &[u8], columns: &[usize], total_cols: usize) -> bincode::Result<Tuple> {
    if let Some(tuple) = try_decode_tuple_columns_fast(bytes, columns, total_cols) {
        return Ok(tuple);
    }

    let values = row_blob_opts().deserialize_seed(SelectedValues { columns, total_cols }, bytes)?;
    Ok(Tuple {
        values,
        row_id: None,
        branch_id: None,
    })
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn read_u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }

    fn read_u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn read_i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn read_u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn read_i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn read_i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn read_f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn read_f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn read_len(&mut self) -> Option<usize> {
        usize::try_from(self.read_u64()?).ok()
    }
}

fn try_decode_tuple_columns_fast(bytes: &[u8], columns: &[usize], total_cols: usize) -> Option<Tuple> {
    let mut cur = ByteCursor::new(bytes);
    let stored_cols = cur.read_len()?;
    let mut out = vec![Value::Null; total_cols];
    let mut wanted = columns.iter().copied().peekable();

    for idx in 0..stored_cols.min(total_cols) {
        match wanted.peek().copied() {
            Some(want) if want == idx => {
                out[idx] = read_value(&mut cur)?;
                wanted.next();
                if wanted.peek().is_none() {
                    return Some(Tuple {
                        values: out,
                        row_id: None,
                        branch_id: None,
                    });
                }
            }
            Some(_) => skip_value(&mut cur)?,
            None => {
                return Some(Tuple {
                    values: out,
                    row_id: None,
                    branch_id: None,
                });
            }
        }
    }

    Some(Tuple {
        values: out,
        row_id: None,
        branch_id: None,
    })
}

fn skip_value(cur: &mut ByteCursor<'_>) -> Option<()> {
    let tag = cur.read_u32()?;
    match tag {
        0 | 20 => Some(()),
        1 => cur.take(1).map(|_| ()),
        2 => cur.take(2).map(|_| ()),
        3 | 5 | 18 => cur.take(4).map(|_| ()),
        4 | 6 | 14 => cur.take(8).map(|_| ()),
        7 | 8 | 9 | 15 => {
            let len = cur.read_len()?;
            cur.take(len).map(|_| ())
        }
        10 => cur.take(16).map(|_| ()),
        16 => {
            let len = cur.read_len()?;
            for _ in 0..len {
                skip_value(cur)?;
            }
            Some(())
        }
        17 => {
            let len = cur.read_len()?;
            cur.take(len.checked_mul(4)?).map(|_| ())
        }
        19 => cur.take(32).map(|_| ()),
        // Date/time serde layouts are not fixed here; fall back to full bincode decode.
        11 | 12 | 13 => None,
        _ => None,
    }
}

fn read_value(cur: &mut ByteCursor<'_>) -> Option<Value> {
    let tag = cur.read_u32()?;
    match tag {
        0 => Some(Value::Null),
        1 => Some(Value::Boolean(cur.read_u8()? != 0)),
        2 => Some(Value::Int2(cur.read_i16()?)),
        3 => Some(Value::Int4(cur.read_i32()?)),
        4 => Some(Value::Int8(cur.read_i64()?)),
        5 => Some(Value::Float4(cur.read_f32()?)),
        6 => Some(Value::Float8(cur.read_f64()?)),
        7 => Some(Value::Numeric(read_string(cur)?)),
        8 => Some(Value::String(read_string(cur)?)),
        9 => {
            let len = cur.read_len()?;
            Some(Value::Bytes(cur.take(len)?.to_vec()))
        }
        10 => {
            let bytes: [u8; 16] = cur.take(16)?.try_into().ok()?;
            Some(Value::Uuid(uuid::Uuid::from_bytes(bytes)))
        }
        14 => Some(Value::Interval(cur.read_i64()?)),
        15 => Some(Value::Json(read_string(cur)?)),
        16 => {
            let len = cur.read_len()?;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(read_value(cur)?);
            }
            Some(Value::Array(values))
        }
        17 => {
            let len = cur.read_len()?;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(cur.read_f32()?);
            }
            Some(Value::Vector(values))
        }
        18 => Some(Value::DictRef {
            dict_id: cur.read_u32()?,
        }),
        19 => {
            let hash: [u8; 32] = cur.take(32)?.try_into().ok()?;
            Some(Value::CasRef { hash })
        }
        20 => Some(Value::ColumnarRef),
        11 | 12 | 13 => None,
        _ => None,
    }
}

fn read_string(cur: &mut ByteCursor<'_>) -> Option<String> {
    let len = cur.read_len()?;
    String::from_utf8(cur.take(len)?.to_vec()).ok()
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

    #[test]
    fn selected_decode_materializes_requested_columns_only() {
        let t = sample();
        let bytes = bincode::serialize(&t).unwrap();
        let full: Tuple = bincode::deserialize(&bytes).unwrap();

        let selected = decode_tuple_columns(&bytes, &[0, 3, 5], 6).unwrap();
        assert_eq!(selected.values.len(), 6);
        assert_eq!(selected.values[0], full.values[0]);
        assert_eq!(selected.values[1], Value::Null);
        assert_eq!(selected.values[2], Value::Null);
        assert_eq!(selected.values[3], full.values[3]);
        assert_eq!(selected.values[4], Value::Null);
        assert_eq!(selected.values[5], full.values[5]);
    }

    #[test]
    fn selected_decode_stops_before_unneeded_tail() {
        let t = sample();
        let bytes = bincode::serialize(&t).unwrap();
        let full: Tuple = bincode::deserialize(&bytes).unwrap();

        let selected = decode_tuple_columns(&bytes, &[1, 3], 6).unwrap();
        assert_eq!(selected.values.len(), 6);
        assert_eq!(selected.values[0], Value::Null);
        assert_eq!(selected.values[1], full.values[1]);
        assert_eq!(selected.values[2], Value::Null);
        assert_eq!(selected.values[3], full.values[3]);
        assert_eq!(selected.values[4], Value::Null);
        assert_eq!(selected.values[5], Value::Null);
    }

    #[test]
    fn selected_decode_falls_back_for_unsupported_skips() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 5, 29).unwrap();
        let t = Tuple::new(vec![
            Value::Int4(7),
            Value::Date(date),
            Value::String("after-date".into()),
        ]);
        let bytes = bincode::serialize(&t).unwrap();
        let full: Tuple = bincode::deserialize(&bytes).unwrap();

        // Date's serde wire shape is intentionally not hand-skipped by the fast
        // cursor. The fallback path must still preserve correctness for later columns.
        let selected = decode_tuple_columns(&bytes, &[2], 3).unwrap();
        assert_eq!(selected.values[0], Value::Null);
        assert_eq!(selected.values[1], Value::Null);
        assert_eq!(selected.values[2], full.values[2]);
    }
}
