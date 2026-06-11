//! R3.4 typed columnar batch format v2.
//!
//! # On-disk format
//!
//! A stored `col:{table}:{column}:{batch_id}` value is one of:
//!
//! - **v1** (legacy, read-compatible forever): `bincode(ColumnBatch)` — the
//!   enum-tagged `Vec<Value>` format every release before R3.4 wrote.
//! - **v2**: `[0xFF, b'H', b'V', b'2']` magic followed by
//!   `bincode(BatchV2)` — per-type fixed-width buffers plus a bit-packed
//!   validity bitmap and embedded `BatchStats`.
//!
//! ## Why the magic prefix is unambiguous
//!
//! v1 bincode starts with the `column: String` field, whose first 8 bytes are
//! the little-endian u64 byte length of the column name. Matching the magic
//! would therefore require a column name of exactly 0x325648FF bytes
//! (~844 MB) — impossible in practice (and bytes 4..8 of any real length are
//! zero while bincode's subsequent payload makes the full collision
//! probability exactly zero for names under 4 GiB). Readers check the 4-byte
//! magic; anything else is decoded as v1.
//!
//! ## Typed encodings (one per batch, chosen by the writer)
//!
//! A batch is encoded as v2 only when every non-NULL slot holds the **same**
//! `Value` variant:
//!
//! - `Int2` / `Int4` / `Int8` → `TypedData::Int { width, data: Vec<i64> }`.
//!   Width promotion to one i64 buffer (the kernels all operate on i64) with
//!   the original width recorded per batch so decoding reproduces the exact
//!   variant (`Int4(7)` round-trips as `Int4(7)`, never `Int8(7)`). Batches
//!   mixing integer widths stay v1 (they do not occur through SQL writes,
//!   where a column has one declared width).
//! - `Float4` / `Float8` → `TypedData::Float { width, data: Vec<f64> }`.
//!   f32→f64 promotion is exact, so round-trip is exact.
//! - `Boolean` → `TypedData::Bool` (bit-packed).
//! - `String` → `TypedData::Text { dict, codes }`: per-batch dictionary of
//!   unique strings in first-occurrence order + one u32 code per slot.
//!   A batch holds at most `BATCH_SIZE` (1024) slots, so codes always fit
//!   u32 (and the dictionary at most 1024 entries). A per-batch dictionary
//!   was chosen over reusing `storage/dictionary.rs` (the global
//!   `STORAGE DICTIONARY` `dict:{table}:{column}` map) because it keeps the
//!   batch fully self-contained: no extra point read, no cross-key
//!   consistency or locking with the dictionary manager, and dictionary
//!   GROUP BY kernels only need per-batch codes anyway.
//!
//! NULL slots are recorded in the validity bitmap and store a zero filler in
//! the data buffer. Slots beyond `slot_count` read as NULL (same rule as v1
//! short value vectors). Any other variant mix (timestamps, decimals, mixed
//! families, all-NULL batches with no type evidence) stays v1.
//!
//! ## Embedded stats vs the `colz:` sidecar
//!
//! v2 embeds the batch's `BatchStats` (min/max/null_count). The `colz:`
//! sidecar is **still written** for v2 batches: zone-map pruning reads the
//! sidecar *before* fetching any batch (that is its entire point — pruned
//! batches cost no batch I/O), so teaching `batch_can_match` to read embedded
//! stats could never replace it. Keeping the sidecar means zero changes to
//! the R3.1 pruning/backfill/manifest machinery; the embedded copy feeds
//! in-memory kernels (e.g. dense small-int group keys) without a sidecar
//! lookup. This was the cheaper integration by far.
//!
//! ## Switches
//!
//! - `HELIOS_BATCH_V2_OFF=1` — writers emit v1 only; readers continue to read
//!   both formats (kill switch).
//! - `HELIOS_BATCH_V2_MIGRATE=1` — scans that decode a v1 batch re-encode it
//!   as v2 best-effort (under the zone-stats write lock, compare-and-swap on
//!   the raw bytes). Default OFF for this release.

use serde::{Deserialize, Serialize};

use super::columnar::{BatchStats, ColumnBatch, BATCH_SIZE};
use crate::Value;

/// Magic prefix identifying a v2-encoded batch (see module docs for why this
/// cannot collide with v1 bincode).
pub const BATCH_V2_MAGIC: [u8; 4] = [0xFF, b'H', b'V', b'2'];

/// R3.4 kill switch: when `HELIOS_BATCH_V2_OFF` is set, writers emit v1
/// (bincode) batches only. Readers always read both formats.
pub fn batch_v2_write_enabled() -> bool {
    static OFF: once_cell::sync::Lazy<bool> =
        once_cell::sync::Lazy::new(|| std::env::var("HELIOS_BATCH_V2_OFF").is_ok());
    !*OFF
}

/// R3.4 lazy migration gate: `HELIOS_BATCH_V2_MIGRATE=1` lets scans rewrite
/// v1 batches as v2 (best-effort, under the zone-stats write lock). Default
/// OFF for this release.
pub fn batch_v2_migrate_enabled() -> bool {
    static ON: once_cell::sync::Lazy<bool> =
        once_cell::sync::Lazy::new(|| std::env::var("HELIOS_BATCH_V2_MIGRATE").as_deref() == Ok("1"));
    *ON
}

/// Original integer width of a v2 int batch (decode reproduces the exact
/// `Value` variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntWidth {
    /// `Value::Int2`
    I2,
    /// `Value::Int4`
    I4,
    /// `Value::Int8`
    I8,
}

impl IntWidth {
    /// Rebuild the original `Value` variant from the promoted i64.
    #[inline]
    pub fn value(self, v: i64) -> Value {
        match self {
            IntWidth::I2 => Value::Int2(v as i16),
            IntWidth::I4 => Value::Int4(v as i32),
            IntWidth::I8 => Value::Int8(v),
        }
    }
}

/// Original float width of a v2 float batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatWidth {
    /// `Value::Float4`
    F4,
    /// `Value::Float8`
    F8,
}

impl FloatWidth {
    /// Rebuild the original `Value` variant from the promoted f64.
    #[inline]
    pub fn value(self, v: f64) -> Value {
        match self {
            FloatWidth::F4 => Value::Float4(v as f32),
            FloatWidth::F8 => Value::Float8(v),
        }
    }
}

/// On-disk typed payload of a v2 batch. NULL slots store zero fillers; the
/// validity bitmap is authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypedData {
    /// Width-promoted integer buffer.
    Int {
        /// Original `Value` variant width.
        width: IntWidth,
        /// One i64 per slot.
        data: Vec<i64>,
    },
    /// Width-promoted float buffer.
    Float {
        /// Original `Value` variant width.
        width: FloatWidth,
        /// One f64 per slot.
        data: Vec<f64>,
    },
    /// Bit-packed booleans (bit `i % 8` of byte `i / 8`).
    Bool {
        /// `ceil(slot_count / 8)` bytes.
        bits: Vec<u8>,
    },
    /// Dictionary-coded text.
    Text {
        /// Unique strings in first-occurrence order (≤ `BATCH_SIZE` entries).
        dict: Vec<String>,
        /// One dictionary code per slot (0 filler on NULL slots).
        codes: Vec<u32>,
    },
}

/// On-disk v2 batch (bincode-serialized after the magic prefix).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchV2 {
    column: String,
    start_row_id: u64,
    /// Number of slots covered (== values.len() of the v1 equivalent).
    slot_count: u32,
    /// Bit-packed validity: bit set ⇔ slot is non-NULL.
    validity: Vec<u8>,
    /// Embedded zone stats (same values as the `colz:` sidecar entry).
    stats: BatchStats,
    data: TypedData,
}

/// In-memory typed view of a decoded v2 batch. Buffers are slot-indexed and
/// kernel-friendly; `Bool` is byte-expanded (one 0/1 byte per slot) so the
/// predicate/aggregate loops are pure byte ops.
#[derive(Debug, Clone)]
pub enum TypedValues {
    /// Width-promoted integers, one per slot.
    Int {
        /// Original `Value` variant width.
        width: IntWidth,
        /// One i64 per slot.
        data: Vec<i64>,
    },
    /// Width-promoted floats, one per slot.
    Float {
        /// Original `Value` variant width.
        width: FloatWidth,
        /// One f64 per slot.
        data: Vec<f64>,
    },
    /// One 0/1 byte per slot.
    Bool {
        /// Byte-expanded booleans.
        data: Vec<u8>,
    },
    /// Dictionary-coded text.
    Text {
        /// Unique strings in first-occurrence order.
        dict: Vec<String>,
        /// One dictionary code per slot.
        codes: Vec<u32>,
    },
}

/// In-memory typed companion of a `ColumnBatch` decoded from v2 storage.
/// Carried alongside the materialized `values` vector (which keeps every
/// existing row-at-a-time path working unchanged); the vectorized kernels
/// operate on these buffers instead.
#[derive(Debug, Clone)]
pub struct TypedBatch {
    /// One 0/1 byte per slot (1 ⇔ non-NULL). Length == slot count; slots
    /// beyond the buffers read as NULL.
    pub validity: Vec<u8>,
    /// Embedded zone stats (identical to the batch's `colz:` sidecar entry).
    pub stats: BatchStats,
    /// The typed buffers.
    pub data: TypedValues,
}

/// Expand a bit-packed bitmap into one 0/1 byte per slot.
fn expand_bits(bits: &[u8], slots: usize) -> Vec<u8> {
    let mut out = vec![0u8; slots];
    for (i, slot) in out.iter_mut().enumerate() {
        if let Some(byte) = bits.get(i / 8) {
            *slot = (byte >> (i % 8)) & 1;
        }
    }
    out
}

/// Pack one-byte-per-slot 0/1 flags into a bitmap.
fn pack_bits(flags: impl ExactSizeIterator<Item = bool>) -> Vec<u8> {
    let mut out = vec![0u8; flags.len().div_ceil(8)];
    for (i, flag) in flags.enumerate() {
        if flag {
            if let Some(byte) = out.get_mut(i / 8) {
                *byte |= 1 << (i % 8);
            }
        }
    }
    out
}

/// Try to build the on-disk typed payload for `values`. Returns `None` when
/// the batch is not v2-eligible: empty, all-NULL (no type evidence), any
/// unsupported variant, or non-NULL slots of more than one variant.
fn build_typed_data(values: &[Value]) -> Option<TypedData> {
    if values.is_empty() || values.len() > BATCH_SIZE {
        return None;
    }
    // Find the first non-null value to pick the target encoding.
    let first = values.iter().find(|v| !matches!(v, Value::Null))?;
    match first {
        Value::Int2(_) => {
            let mut data = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Int2(x) => data.push(*x as i64),
                    Value::Null => data.push(0),
                    _ => return None,
                }
            }
            Some(TypedData::Int {
                width: IntWidth::I2,
                data,
            })
        }
        Value::Int4(_) => {
            let mut data = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Int4(x) => data.push(*x as i64),
                    Value::Null => data.push(0),
                    _ => return None,
                }
            }
            Some(TypedData::Int {
                width: IntWidth::I4,
                data,
            })
        }
        Value::Int8(_) => {
            let mut data = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Int8(x) => data.push(*x),
                    Value::Null => data.push(0),
                    _ => return None,
                }
            }
            Some(TypedData::Int {
                width: IntWidth::I8,
                data,
            })
        }
        Value::Float4(_) => {
            let mut data = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Float4(x) => data.push(*x as f64),
                    Value::Null => data.push(0.0),
                    _ => return None,
                }
            }
            Some(TypedData::Float {
                width: FloatWidth::F4,
                data,
            })
        }
        Value::Float8(_) => {
            let mut data = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Float8(x) => data.push(*x),
                    Value::Null => data.push(0.0),
                    _ => return None,
                }
            }
            Some(TypedData::Float {
                width: FloatWidth::F8,
                data,
            })
        }
        Value::Boolean(_) => {
            let mut flags = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::Boolean(x) => flags.push(*x),
                    Value::Null => flags.push(false),
                    _ => return None,
                }
            }
            Some(TypedData::Bool {
                bits: pack_bits(flags.into_iter()),
            })
        }
        Value::String(_) => {
            let mut dict: Vec<String> = Vec::new();
            let mut index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            let mut codes = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::String(s) => {
                        let code = match index.get(s) {
                            Some(&code) => code,
                            None => {
                                let code = dict.len() as u32;
                                dict.push(s.clone());
                                index.insert(s.clone(), code);
                                code
                            }
                        };
                        codes.push(code);
                    }
                    Value::Null => codes.push(0),
                    _ => return None,
                }
            }
            Some(TypedData::Text { dict, codes })
        }
        _ => None,
    }
}

/// Encode `batch` for storage: v2 when enabled and the value set is
/// homogeneously typed, v1 (plain bincode) otherwise. `stats` must be the
/// freshly computed `BatchStats` for the batch (shared with the `colz:`
/// sidecar write so the embedded and sidecar copies are identical).
pub fn encode_column_batch(batch: &ColumnBatch, stats: &BatchStats) -> crate::Result<Vec<u8>> {
    if batch_v2_write_enabled() {
        if let Some(data) = build_typed_data(&batch.values) {
            let v2 = BatchV2 {
                column: batch.column.clone(),
                start_row_id: batch.start_row_id,
                slot_count: batch.values.len() as u32,
                validity: pack_bits(batch.values.iter().map(|v| !matches!(v, Value::Null))),
                stats: stats.clone(),
                data,
            };
            let mut out = Vec::with_capacity(64 + batch.values.len() * 8);
            out.extend_from_slice(&BATCH_V2_MAGIC);
            bincode::serialize_into(&mut out, &v2)
                .map_err(|e| crate::Error::storage(format!("Columnar v2 serialize failed: {}", e)))?;
            return Ok(out);
        }
    }
    bincode::serialize(batch).map_err(|e| crate::Error::storage(format!("Columnar serialize failed: {}", e)))
}

/// Would `batch` encode as v2? (Used by the lazy-migration path to avoid
/// taking the write lock for ineligible batches.)
pub fn batch_is_v2_eligible(batch: &ColumnBatch) -> bool {
    build_typed_data(&batch.values).is_some()
}

/// Decode a stored batch of either format. v2 batches come back with the
/// `typed` companion populated **and** `values` fully materialized, so every
/// pre-R3.4 row-at-a-time consumer keeps working unchanged; v1 batches come
/// back exactly as before (`typed == None` ⇒ scalar paths).
pub fn decode_column_batch(raw: &[u8]) -> crate::Result<ColumnBatch> {
    if raw.len() >= BATCH_V2_MAGIC.len() && raw[..BATCH_V2_MAGIC.len()] == BATCH_V2_MAGIC {
        let v2: BatchV2 = bincode::deserialize(&raw[BATCH_V2_MAGIC.len()..])
            .map_err(|e| crate::Error::storage(format!("Columnar v2 deserialize failed: {}", e)))?;
        let slots = v2.slot_count as usize;
        let validity = expand_bits(&v2.validity, slots);

        let (values, data) = match v2.data {
            TypedData::Int { width, data } => {
                let values: Vec<Value> = data
                    .iter()
                    .zip(&validity)
                    .map(|(&v, &ok)| if ok != 0 { width.value(v) } else { Value::Null })
                    .collect();
                (values, TypedValues::Int { width, data })
            }
            TypedData::Float { width, data } => {
                let values: Vec<Value> = data
                    .iter()
                    .zip(&validity)
                    .map(|(&v, &ok)| if ok != 0 { width.value(v) } else { Value::Null })
                    .collect();
                (values, TypedValues::Float { width, data })
            }
            TypedData::Bool { bits } => {
                let data = expand_bits(&bits, slots);
                let values: Vec<Value> = data
                    .iter()
                    .zip(&validity)
                    .map(|(&v, &ok)| if ok != 0 { Value::Boolean(v != 0) } else { Value::Null })
                    .collect();
                (values, TypedValues::Bool { data })
            }
            TypedData::Text { dict, codes } => {
                let values: Vec<Value> = codes
                    .iter()
                    .zip(&validity)
                    .map(|(&code, &ok)| {
                        if ok != 0 {
                            dict.get(code as usize)
                                .map(|s| Value::String(s.clone()))
                                .unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    })
                    .collect();
                (values, TypedValues::Text { dict, codes })
            }
        };

        return Ok(ColumnBatch {
            column: v2.column,
            start_row_id: v2.start_row_id,
            values,
            typed: Some(TypedBatch {
                validity,
                stats: v2.stats,
                data,
            }),
        });
    }

    bincode::deserialize(raw).map_err(|e| crate::Error::storage(format!("Columnar deserialize failed: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch_of(values: Vec<Value>) -> ColumnBatch {
        ColumnBatch {
            column: "v".to_string(),
            start_row_id: 0,
            values,
            typed: None,
        }
    }

    fn roundtrip(values: Vec<Value>) -> ColumnBatch {
        let batch = batch_of(values);
        let stats = BatchStats::from_batch(&batch);
        let encoded = encode_column_batch(&batch, &stats).unwrap();
        decode_column_batch(&encoded).unwrap()
    }

    #[test]
    fn int_widths_roundtrip_exactly() {
        for values in [
            vec![Value::Int2(-3), Value::Null, Value::Int2(7)],
            vec![Value::Int4(1), Value::Int4(i32::MIN), Value::Null],
            vec![Value::Int8(i64::MAX), Value::Null, Value::Int8(-1)],
        ] {
            let decoded = roundtrip(values.clone());
            assert_eq!(decoded.values, values);
            assert!(decoded.typed.is_some(), "expected v2 for {values:?}");
        }
    }

    #[test]
    fn floats_bools_text_roundtrip() {
        let floats = vec![Value::Float8(1.5), Value::Null, Value::Float8(-0.25)];
        assert_eq!(roundtrip(floats.clone()).values, floats);

        let f4 = vec![Value::Float4(2.5), Value::Float4(f32::NAN), Value::Null];
        let decoded = roundtrip(f4);
        assert!(matches!(decoded.values[0], Value::Float4(x) if x == 2.5));
        assert!(matches!(decoded.values[1], Value::Float4(x) if x.is_nan()));
        assert!(matches!(decoded.values[2], Value::Null));

        let bools = vec![Value::Boolean(true), Value::Null, Value::Boolean(false)];
        assert_eq!(roundtrip(bools.clone()).values, bools);

        let text = vec![
            Value::String("a".into()),
            Value::Null,
            Value::String("b".into()),
            Value::String("a".into()),
        ];
        let decoded = roundtrip(text.clone());
        assert_eq!(decoded.values, text);
        match &decoded.typed.unwrap().data {
            TypedValues::Text { dict, codes } => {
                assert_eq!(dict, &vec!["a".to_string(), "b".to_string()]);
                assert_eq!(codes, &vec![0, 0, 1, 0]);
            }
            other => panic!("expected text typed data, got {other:?}"),
        }
    }

    #[test]
    fn mixed_or_unsupported_batches_stay_v1() {
        for values in [
            vec![Value::Int4(1), Value::Int8(2)],                 // mixed widths
            vec![Value::Int4(1), Value::String("x".into())],      // mixed families
            vec![Value::Null, Value::Null],                       // no type evidence
            vec![Value::Numeric("1.5".into())],                   // unsupported type
            Vec::new(),                                           // empty
        ] {
            let batch = batch_of(values.clone());
            let stats = BatchStats::from_batch(&batch);
            let encoded = encode_column_batch(&batch, &stats).unwrap();
            assert_ne!(
                encoded.get(..4),
                Some(&BATCH_V2_MAGIC[..]),
                "expected v1 for {values:?}"
            );
            let decoded = decode_column_batch(&encoded).unwrap();
            assert_eq!(decoded.values, values);
            assert!(decoded.typed.is_none());
        }
    }

    #[test]
    fn v2_embeds_the_same_stats_as_the_sidecar() {
        let mut values = vec![Value::Null; BATCH_SIZE];
        values[3] = Value::Int8(-5);
        values[900] = Value::Int8(40);
        let decoded = roundtrip(values);
        let typed = decoded.typed.unwrap();
        assert_eq!(typed.stats.min, Some(Value::Int8(-5)));
        assert_eq!(typed.stats.max, Some(Value::Int8(40)));
        assert_eq!(typed.stats.null_count, BATCH_SIZE as u32 - 2);
        assert_eq!(typed.validity.iter().filter(|&&b| b != 0).count(), 2);
    }
}
