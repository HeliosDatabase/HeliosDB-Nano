//! The ONE place a PostgreSQL-wire result column's `(oid, size, format_code)`
//! and per-column text rendering are decided.
//!
//! `RowDescription` fields and `DataRow` payloads used to be derived
//! independently: the descriptor read the DECLARED type through
//! `effective_result_format`, the payload read the RAW Bind request through
//! `requested_result_format`, and the text encoders never saw a declared type
//! at all. That is how a field could be advertised `(text, binary)` while
//! carrying int4 bytes, or a `timestamptz` column could be advertised as 1184
//! while its text form carried no zone offset (GH#23).
//!
//! Now [`wire_plan`] computes a [`ColumnCodec`] per column from the schema and
//! the portal's result formats, and BOTH messages read it: the descriptor via
//! [`field_descriptions`], the payload via the handler's encoders. Describe and
//! Execute call `wire_plan` on the same immutable inputs
//! (`PreparedStatement::result_schema`, `Portal::result_formats`), so what is
//! advertised is what is encoded — by construction, not by agreement.
//!
//! A binary cell is encoded BY THE DECLARED TYPE ([`encode_binary`]), never
//! by the runtime value's variant: an inferred `int8` column whose row value
//! is `Value::Int4` ships 8 bytes, and a value the declared type cannot
//! represent in binary is REFUSED (SQLSTATE 22P03) rather than sent under
//! the wrong width or silently downgraded to text for one cell — the
//! protocol has no per-cell format.
//!
//! GH#25 (bytea text forms, binary encoders for further types) plugs into
//! [`ColumnCodec`] / [`encode_binary`] here.

use super::handler::{datatype_to_oid, datatype_to_size};
use super::messages::FieldDescription;
use crate::{DataType, Error, Result, Schema, Value};

/// How a value in this column is rendered in TEXT format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextForm {
    /// The value's own text rendering — the per-`Value` match in the
    /// handler's encoders, byte-for-byte as before.
    Natural,
    /// `timestamp with time zone`: `%Y-%m-%d %H:%M:%S%.6f` followed by the
    /// session zone's offset. The server pins `TimeZone=UTC` at startup
    /// (`PgConnectionHandler` sends `ParameterStatus("TimeZone", "UTC")`),
    /// so the suffix is [`TIMESTAMPTZ_UTC_SUFFIX`]. Without it a typed
    /// client re-resolves the instant in its own zone.
    TimestamptzUtc,
}

/// The offset PostgreSQL appends to a `timestamptz` text value under
/// `TimeZone=UTC`. Not a tunable: it is the rendering of the zone the server
/// already fixes for every session.
pub(super) const TIMESTAMPTZ_UTC_SUFFIX: &str = "+00";

/// Everything the wire needs to know about one result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColumnCodec {
    /// The DECLARED type the column is advertised as — the premise
    /// [`encode_binary`] encodes against when `format == 1`.
    pub data_type: DataType,
    /// `RowDescription.dataTypeID`.
    pub oid: i32,
    /// `RowDescription.dataTypeSize`.
    pub size: i16,
    /// `RowDescription.formatCode` AND the format the DataRow cell is encoded
    /// in: 0 text, 1 binary. Binary is granted only when the client asked for
    /// it and the declared type has a binary encoder.
    pub format: i16,
    /// Text rendering of the cell when `format == 0`.
    pub text: TextForm,
}

/// THE function: the per-column codec for `schema` under the portal's
/// `result_formats` (`[]` / `[0]` / all-zero = text everywhere; `[1]` = binary
/// everywhere the type allows; per-column otherwise).
pub(super) fn wire_plan(schema: &Schema, result_formats: &[i16]) -> Vec<ColumnCodec> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, col)| ColumnCodec {
            data_type: col.data_type.clone(),
            oid: datatype_to_oid(&col.data_type),
            size: datatype_to_size(&col.data_type),
            format: effective_result_format(&col.data_type, result_formats, index),
            text: text_form(&col.data_type),
        })
        .collect()
}

/// `RowDescription` fields for `schema`, one per `plan` entry.
pub(super) fn field_descriptions(schema: &Schema, plan: &[ColumnCodec]) -> Vec<FieldDescription> {
    schema
        .columns
        .iter()
        .zip(plan)
        .map(|(col, codec)| FieldDescription {
            name: col.name.clone(),
            table_oid: 0,
            column_attr_num: 0,
            data_type_oid: codec.oid,
            data_type_size: codec.size,
            type_modifier: -1,
            format_code: codec.format,
        })
        .collect()
}

/// True when every column of `plan` is sent in text format — the precondition
/// for the direct (zero-copy) DataRow encoder.
pub(super) fn all_text(plan: &[ColumnCodec]) -> bool {
    plan.iter().all(|c| c.format == 0)
}

/// The message prefix of every refusal [`encode_binary`] raises. The
/// handler's SQLSTATE classifier maps it to `22P03
/// invalid_binary_representation`; keeping it a const owned by the single
/// emitter means the two cannot drift.
pub(super) const INVALID_BINARY_REPRESENTATION_MARKER: &str = "invalid binary representation";

/// Encode one non-NULL cell in BINARY format by the column's DECLARED type
/// (`codec.data_type`, the type `RowDescription` advertised) — never by the
/// runtime value's variant (GH#23).
///
/// Why: an expression column is typed by inference (`length("note")` →
/// `int8`) but produced at runtime by the function (`Value::Int4`). Encoding
/// the value's own width sent 4 bytes under OID 20, which tokio-postgres
/// (Prisma's engine, always binary) rejects. So integers are widened to the
/// declared width (`Int2`/`Int4` → `int8`), floats to `float8`, and a value
/// the declared type cannot represent — `Value::Int4` under `text`
/// (`coalesce("n", 0)` typed by the inferencer's Text fallback),
/// `Value::Boolean` under `int4` — is REFUSED with a
/// [`INVALID_BINARY_REPRESENTATION_MARKER`] error (22P03 on the wire). It is
/// never sent under the wrong width and never silently downgraded to text for
/// that one cell: the protocol fixes the format per column, not per cell.
///
/// Narrowing (`Value::Int8` under `int4`) is allowed only when the value fits;
/// out of range is refused the same way. Only the declared types
/// `datatype_has_binary_result` grants binary to can reach here; any other
/// declared type is refused (unreachable via [`wire_plan`], kept closed).
pub(super) fn encode_binary(value: &Value, codec: &ColumnCodec) -> Result<Vec<u8>> {
    let refuse = || {
        Error::query_execution(format!(
            "{INVALID_BINARY_REPRESENTATION_MARKER}: a {} value cannot be sent as {} (OID {}) in binary format",
            value.data_type(),
            codec.data_type,
            codec.oid
        ))
    };
    match &codec.data_type {
        DataType::Boolean => match value {
            Value::Boolean(b) => Ok(vec![u8::from(*b)]),
            _ => Err(refuse()),
        },
        DataType::Int2 => match value {
            Value::Int2(i) => Ok(i.to_be_bytes().to_vec()),
            Value::Int4(i) => i16::try_from(*i)
                .map(|i| i.to_be_bytes().to_vec())
                .map_err(|_| refuse()),
            Value::Int8(i) => i16::try_from(*i)
                .map(|i| i.to_be_bytes().to_vec())
                .map_err(|_| refuse()),
            _ => Err(refuse()),
        },
        DataType::Int4 => match value {
            Value::Int2(i) => Ok(i32::from(*i).to_be_bytes().to_vec()),
            Value::Int4(i) => Ok(i.to_be_bytes().to_vec()),
            Value::Int8(i) => i32::try_from(*i)
                .map(|i| i.to_be_bytes().to_vec())
                .map_err(|_| refuse()),
            _ => Err(refuse()),
        },
        DataType::Int8 => match value {
            Value::Int2(i) => Ok(i64::from(*i).to_be_bytes().to_vec()),
            Value::Int4(i) => Ok(i64::from(*i).to_be_bytes().to_vec()),
            Value::Int8(i) => Ok(i.to_be_bytes().to_vec()),
            _ => Err(refuse()),
        },
        DataType::Float4 => match value {
            Value::Float4(f) => Ok(f.to_be_bytes().to_vec()),
            // `float8` → `float4` rounds, as PostgreSQL's own cast does.
            Value::Float8(f) => Ok((*f as f32).to_be_bytes().to_vec()),
            Value::Int2(i) => Ok(f32::from(*i).to_be_bytes().to_vec()),
            Value::Int4(i) => Ok((*i as f32).to_be_bytes().to_vec()),
            Value::Int8(i) => Ok((*i as f32).to_be_bytes().to_vec()),
            _ => Err(refuse()),
        },
        DataType::Float8 => match value {
            Value::Float4(f) => Ok(f64::from(*f).to_be_bytes().to_vec()),
            Value::Float8(f) => Ok(f.to_be_bytes().to_vec()),
            Value::Int2(i) => Ok(f64::from(*i).to_be_bytes().to_vec()),
            Value::Int4(i) => Ok(f64::from(*i).to_be_bytes().to_vec()),
            Value::Int8(i) => Ok((*i as f64).to_be_bytes().to_vec()),
            _ => Err(refuse()),
        },
        // `numeric`'s binary send format (`numeric_send`): base-10000 digit
        // groups — see [`encode_numeric_binary`]. A stored `Value::Numeric` is
        // the canonical text; an integer is exact in numeric; a float is sent
        // as the shortest round-trip decimal of the f64 (the value the client
        // would have parsed from the text form). A payload the numeric parser
        // cannot read is refused, never sent as text under format 1 (GH#23
        // candidate 3: `RETURNING round(price, 2)` under a binary Bind).
        DataType::Numeric => match value {
            Value::Numeric(s) | Value::String(s) => encode_numeric_binary(s).ok_or_else(refuse),
            Value::Int2(i) => encode_numeric_binary(&i.to_string()).ok_or_else(refuse),
            Value::Int4(i) => encode_numeric_binary(&i.to_string()).ok_or_else(refuse),
            Value::Int8(i) => encode_numeric_binary(&i.to_string()).ok_or_else(refuse),
            Value::Float4(f) => encode_numeric_binary(&f64::from(*f).to_string()).ok_or_else(refuse),
            Value::Float8(f) => encode_numeric_binary(&f.to_string()).ok_or_else(refuse),
            _ => Err(refuse()),
        },
        // `text`'s binary send format is its bytes. A JSON value is stored as
        // its text, so it is representable; anything else (an integer under a
        // Text-by-fallback expression, a timestamp, a uuid) is refused — the
        // client was told `text` and would decode a number it cannot detect.
        DataType::Text | DataType::Varchar(_) => match value {
            Value::String(s) | Value::Json(s) => Ok(s.as_bytes().to_vec()),
            _ => Err(refuse()),
        },
        DataType::Bytea => match value {
            Value::Bytes(b) => Ok(b.clone()),
            _ => Err(refuse()),
        },
        DataType::Uuid => match value {
            Value::Uuid(u) => Ok(u.as_bytes().to_vec()),
            Value::String(s) => uuid::Uuid::parse_str(s)
                .map(|u| u.as_bytes().to_vec())
                .map_err(|_| refuse()),
            _ => Err(refuse()),
        },
        // No binary encoder for this declared type: `wire_plan` never grants
        // format 1 to it, so this is unreachable through the plan — and stays
        // closed if a caller ever hands a hand-built codec.
        _ => Err(refuse()),
    }
}

/// PostgreSQL's `numeric_send` wire form of a decimal in TEXT (`"-12345.6789"`,
/// `"0.05"`, `"NaN"`, `"Infinity"`): four big-endian `int16` header words —
/// `ndigits`, `weight`, `sign`, `dscale` — followed by `ndigits` base-10000
/// digit groups, most significant first, the first group carrying weight
/// `10000^weight`. `sign` is `0x0000` positive, `0x4000` negative, `0xC000`
/// NaN, `0xD000` +Infinity, `0xF000` -Infinity (utils/adt/numeric.c). Leading
/// and trailing zero GROUPS are stripped and zero is always positive with
/// weight 0, exactly as `strip_var` leaves it; `dscale` is the number of
/// fractional decimal digits in the text, so `1.50` round-trips as `1.50`.
///
/// Returns `None` for anything that is not a plain signed decimal or one of
/// the three special tokens (an exponent form, an empty string, a stray
/// character) — the caller refuses the cell (22P03) rather than guess.
fn encode_numeric_binary(text: &str) -> Option<Vec<u8>> {
    const DEC_DIGITS: usize = 4;
    const NUMERIC_POS: u16 = 0x0000;
    const NUMERIC_NEG: u16 = 0x4000;
    const NUMERIC_NAN: u16 = 0xC000;
    const NUMERIC_PINF: u16 = 0xD000;
    const NUMERIC_NINF: u16 = 0xF000;

    fn header(ndigits: i16, weight: i16, sign: u16, dscale: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 2 * usize::try_from(ndigits).unwrap_or(0));
        out.extend_from_slice(&ndigits.to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
        out
    }

    let text = text.trim();
    if let Some(special) = crate::sql::numeric_special::parse_special(text) {
        use crate::sql::numeric_special::Special;
        let sign = match special {
            Special::NaN => NUMERIC_NAN,
            Special::PosInf => NUMERIC_PINF,
            Special::NegInf => NUMERIC_NINF,
        };
        return Some(header(0, 0, sign, 0));
    }

    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let dscale = i16::try_from(frac_part.len()).ok()?;
    let int_part = int_part.trim_start_matches('0');

    // `set_var_from_str`: decimal digits laid out so that the integer part
    // ends on a group boundary — `offset` leading zeros pad the first group.
    let int_len = int_part.len();
    let (weight, offset) = if int_len == 0 {
        (-1i64, 0usize)
    } else {
        let groups = int_len.div_ceil(DEC_DIGITS);
        (i64::try_from(groups).ok()? - 1, groups * DEC_DIGITS - int_len)
    };
    let decimal_digits = std::iter::repeat_n(0u8, offset)
        .chain(int_part.bytes().map(|b| b - b'0'))
        .chain(frac_part.bytes().map(|b| b - b'0'));
    let mut digits: Vec<i16> = Vec::with_capacity((offset + int_len + frac_part.len()).div_ceil(DEC_DIGITS));
    let mut group: i16 = 0;
    let mut in_group = 0usize;
    for d in decimal_digits {
        group = group * 10 + i16::from(d);
        in_group += 1;
        if in_group == DEC_DIGITS {
            digits.push(group);
            group = 0;
            in_group = 0;
        }
    }
    if in_group > 0 {
        for _ in in_group..DEC_DIGITS {
            group *= 10;
        }
        digits.push(group);
    }

    // `strip_var`: drop leading zero groups (each lowers the weight) and
    // trailing zero groups; an all-zero value is `0` with weight 0, positive.
    let mut weight = weight;
    let leading_zero_groups = digits.iter().take_while(|g| **g == 0).count();
    let mut digits = digits.split_off(leading_zero_groups);
    weight -= i64::try_from(leading_zero_groups).ok()?;
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let (weight, sign) = if digits.is_empty() {
        (0, NUMERIC_POS)
    } else if negative {
        (weight, NUMERIC_NEG)
    } else {
        (weight, NUMERIC_POS)
    };

    let mut out = header(
        i16::try_from(digits.len()).ok()?,
        i16::try_from(weight).ok()?,
        sign,
        dscale,
    );
    for group in digits {
        out.extend_from_slice(&group.to_be_bytes());
    }
    Some(out)
}

fn text_form(data_type: &DataType) -> TextForm {
    match data_type {
        DataType::Timestamptz => TextForm::TimestamptzUtc,
        _ => TextForm::Natural,
    }
}

/// The format the client asked for on column `column_index`: an empty list
/// means text, a single code applies to every column, otherwise per column
/// (missing entries are text).
fn requested_result_format(result_formats: &[i16], column_index: usize) -> i16 {
    match result_formats {
        [] => 0,
        [single] => *single,
        _ => result_formats.get(column_index).copied().unwrap_or(0),
    }
}

/// The format actually used: binary only when requested AND the declared
/// type has a binary encoder (`value_to_pg_binary` in the handler).
fn effective_result_format(data_type: &DataType, result_formats: &[i16], column_index: usize) -> i16 {
    let requested = requested_result_format(result_formats, column_index);
    if requested == 1 && datatype_has_binary_result(data_type) {
        1
    } else {
        0
    }
}

/// The declared types this wire encoder can send in binary format. Widening
/// this list requires a matching `typsend`-shaped encoder arm in
/// [`encode_binary`]; never advertise a type the encoder does not produce.
/// `Numeric` joined in GH#23 candidate 3 ([`encode_numeric_binary`]): a
/// binary-only driver (tokio-postgres under Prisma) asked for format 1 on a
/// `numeric` field and, with no encoder, was handed the text form under
/// format 0 — well-formed for a client that reads Describe(Portal), wrong
/// bytes for one that decodes by the format it requested.
fn datatype_has_binary_result(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Boolean
            | DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Float4
            | DataType::Float8
            | DataType::Numeric
            | DataType::Bytea
            | DataType::Text
            | DataType::Varchar(_)
            | DataType::Uuid
    )
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Column;

    fn schema() -> Schema {
        Schema::new(vec![
            Column::new("n", DataType::Int4),
            Column::new("amt", DataType::Numeric),
            Column::new("tsz", DataType::Timestamptz),
            Column::new("ts", DataType::Timestamp),
        ])
    }

    #[test]
    fn text_request_is_text_everywhere() {
        for formats in [&[][..], &[0][..], &[0, 0, 0, 0][..]] {
            let plan = wire_plan(&schema(), formats);
            assert!(all_text(&plan), "{formats:?}");
            assert_eq!(plan[0].oid, 23);
            assert_eq!(plan[1].oid, 1700);
            assert_eq!(plan[2].oid, 1184);
            assert_eq!(plan[3].oid, 1114);
        }
    }

    #[test]
    fn binary_is_granted_only_where_an_encoder_exists() {
        // int4 and numeric have binary encoders; the two timestamp types do not.
        let plan = wire_plan(&schema(), &[1]);
        assert_eq!(plan.iter().map(|c| c.format).collect::<Vec<_>>(), vec![1, 1, 0, 0]);
        assert!(!all_text(&plan));
        let plan = wire_plan(&schema(), &[1, 1, 0, 1]);
        assert_eq!(plan.iter().map(|c| c.format).collect::<Vec<_>>(), vec![1, 1, 0, 0]);
        let plan = wire_plan(&schema(), &[1, 0, 0, 1]);
        assert_eq!(plan.iter().map(|c| c.format).collect::<Vec<_>>(), vec![1, 0, 0, 0]);
    }

    #[test]
    fn only_timestamptz_carries_the_utc_text_form() {
        let plan = wire_plan(&schema(), &[]);
        assert_eq!(plan[2].text, TextForm::TimestamptzUtc);
        assert_eq!(plan[3].text, TextForm::Natural);
        assert_eq!(plan[0].text, TextForm::Natural);
    }

    #[test]
    fn binary_encoding_follows_the_declared_type_not_the_value() {
        let codec = |dt: DataType| ColumnCodec {
            oid: datatype_to_oid(&dt),
            size: datatype_to_size(&dt),
            format: 1,
            text: TextForm::Natural,
            data_type: dt,
        };
        // Widening to the declared width.
        assert_eq!(
            encode_binary(&Value::Int4(5), &codec(DataType::Int8)).unwrap(),
            5i64.to_be_bytes()
        );
        assert_eq!(
            encode_binary(&Value::Int2(5), &codec(DataType::Int4)).unwrap(),
            5i32.to_be_bytes()
        );
        assert_eq!(
            encode_binary(&Value::Float4(1.5), &codec(DataType::Float8)).unwrap(),
            1.5f64.to_be_bytes()
        );
        // Same width: unchanged.
        assert_eq!(
            encode_binary(&Value::Int4(5), &codec(DataType::Int4)).unwrap(),
            5i32.to_be_bytes()
        );
        assert_eq!(
            encode_binary(&Value::Boolean(true), &codec(DataType::Boolean)).unwrap(),
            vec![1]
        );
        assert_eq!(
            encode_binary(&Value::String("hi".into()), &codec(DataType::Text)).unwrap(),
            b"hi".to_vec()
        );
        // Narrowing only when the value fits.
        assert_eq!(
            encode_binary(&Value::Int8(7), &codec(DataType::Int4)).unwrap(),
            7i32.to_be_bytes()
        );
        assert!(encode_binary(&Value::Int8(i64::MAX), &codec(DataType::Int4)).is_err());
        // Not representable in the declared type: refused, never mis-sized.
        for (value, dt) in [
            (Value::Int4(7), DataType::Text),
            (Value::Boolean(true), DataType::Int4),
            (Value::String("x".into()), DataType::Int8),
            (Value::Int4(7), DataType::Uuid),
            (Value::Timestamp(chrono::Utc::now()), DataType::Text),
        ] {
            let err = encode_binary(&value, &codec(dt.clone())).unwrap_err();
            assert!(
                err.to_string().contains(INVALID_BINARY_REPRESENTATION_MARKER),
                "{value:?} under {dt:?}: {err}"
            );
        }
    }

    /// GH#23 candidate 3: `numeric` under a binary Bind is PostgreSQL's
    /// `numeric_send` form — header `(ndigits, weight, sign, dscale)` and
    /// base-10000 groups — for a stored `Value::Numeric`, for an integer, for
    /// a float, and for the three special tokens.
    #[test]
    fn numeric_binary_is_the_postgres_numeric_send_form() {
        let codec = ColumnCodec {
            oid: datatype_to_oid(&DataType::Numeric),
            size: datatype_to_size(&DataType::Numeric),
            format: 1,
            text: TextForm::Natural,
            data_type: DataType::Numeric,
        };
        let enc = |v: Value| encode_binary(&v, &codec).unwrap();
        let be = |words: &[i16]| words.iter().flat_map(|w| w.to_be_bytes()).collect::<Vec<u8>>();
        let sign = |s: u16| s.to_be_bytes().to_vec();
        // `round(10.456, 2)` = 10.46 → groups [10, 4600], weight 0, dscale 2.
        assert_eq!(
            enc(Value::Numeric("10.46".into())),
            [be(&[2, 0]), sign(0x0000), be(&[2, 10, 4600])].concat()
        );
        // Weight: 12345.6789 → [1, 2345, 6789] with the first group at 10000^1.
        assert_eq!(
            enc(Value::Numeric("12345.6789".into())),
            [be(&[3, 1]), sign(0x0000), be(&[4, 1, 2345, 6789])].concat()
        );
        // Negative; a fraction below 1/10000 lowers the weight below -1.
        assert_eq!(
            enc(Value::Numeric("-0.00005".into())),
            [be(&[1, -2]), sign(0x4000), be(&[5, 5000])].concat()
        );
        // The scale is the text's fractional digit count: 1.50 keeps dscale 2
        // (and its non-zero trailing group 5000); trailing zero GROUPS are
        // stripped (1.0000 → one group, dscale 4; 10000 → one group at weight 1).
        assert_eq!(
            enc(Value::Numeric("1.50".into())),
            [be(&[2, 0]), sign(0x0000), be(&[2, 1, 5000])].concat()
        );
        assert_eq!(
            enc(Value::Numeric("1.0000".into())),
            [be(&[1, 0]), sign(0x0000), be(&[4, 1])].concat()
        );
        assert_eq!(
            enc(Value::Numeric("10000".into())),
            [be(&[1, 1]), sign(0x0000), be(&[0, 1])].concat()
        );
        // Zero is ndigits 0, weight 0, positive — also for "-0.0" (dscale 1).
        assert_eq!(
            enc(Value::Numeric("0".into())),
            [be(&[0, 0]), sign(0x0000), be(&[0])].concat()
        );
        assert_eq!(
            enc(Value::Numeric("-0.0".into())),
            [be(&[0, 0]), sign(0x0000), be(&[1])].concat()
        );
        // Integers and floats are exact / shortest-round-trip decimals.
        assert_eq!(enc(Value::Int4(7)), [be(&[1, 0]), sign(0x0000), be(&[0, 7])].concat());
        assert_eq!(
            enc(Value::Int8(-123_456_789)),
            [be(&[3, 2]), sign(0x4000), be(&[0, 1, 2345, 6789])].concat()
        );
        assert_eq!(
            enc(Value::Float8(2.5)),
            [be(&[2, 0]), sign(0x0000), be(&[1, 2, 5000])].concat()
        );
        // Specials: no digits, the sign word carries the kind.
        assert_eq!(
            enc(Value::Numeric("NaN".into())),
            [be(&[0, 0]), sign(0xC000), be(&[0])].concat()
        );
        assert_eq!(
            enc(Value::Numeric("Infinity".into())),
            [be(&[0, 0]), sign(0xD000), be(&[0])].concat()
        );
        assert_eq!(
            enc(Value::Numeric("-Infinity".into())),
            [be(&[0, 0]), sign(0xF000), be(&[0])].concat()
        );
        // Not a numeric payload: refused, never guessed.
        for value in [
            Value::Numeric("1e5".into()),
            Value::Numeric(".".into()),
            Value::Numeric(String::new()),
            Value::String("abc".into()),
            Value::Boolean(true),
        ] {
            let err = encode_binary(&value, &codec).unwrap_err();
            assert!(
                err.to_string().contains(INVALID_BINARY_REPRESENTATION_MARKER),
                "{value:?}: {err}"
            );
        }
    }

    #[test]
    fn descriptor_reads_the_plan_verbatim() {
        let s = schema();
        let plan = wire_plan(&s, &[1]);
        let fields = field_descriptions(&s, &plan);
        assert_eq!(fields.len(), 4);
        for (f, c) in fields.iter().zip(&plan) {
            assert_eq!(f.data_type_oid, c.oid);
            assert_eq!(f.data_type_size, c.size);
            assert_eq!(f.format_code, c.format);
        }
        assert_eq!(fields[0].name, "n");
    }
}
