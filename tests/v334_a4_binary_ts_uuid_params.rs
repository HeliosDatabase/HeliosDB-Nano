//! Regression for checklist item **A4** — "Binary TIMESTAMP and UUID parameter
//! INPUTS rejected" (psycopg3 binary mode).
//!
//! Acceptance: binary-format TIMESTAMP (OID 1114) and UUID (OID 2950) parameters
//! are accepted and correctly decoded.
//!
//! Root cause: `decode_binary_parameter` in `src/protocol/postgres/prepared.rs`
//! lacks arms for OID 1114 / 2950, so they fall through to `Value::Bytes` and
//! later fail to cast ("Cannot cast Bytes(...) to TIMESTAMP/UUID"). Tested via
//! the public `decode_parameter(data, format=1, oid)` entry point.

use heliosdb_nano::{protocol::postgres::prepared::decode_parameter, Value};

const PG_EPOCH_UNIX_SECS: i64 = 946_684_800; // 2000-01-01T00:00:00Z

#[test]
fn a4_binary_timestamp_param_decodes() {
    // OID 1114 TIMESTAMP, binary format: 8-byte big-endian i64 microseconds
    // since 2000-01-01 00:00:00 UTC.
    let micros_zero: i64 = 0; // == 2000-01-01T00:00:00Z
    match decode_parameter(&micros_zero.to_be_bytes(), 1, 1114).expect("decode ts @ pg-epoch") {
        Value::Timestamp(ts) => assert_eq!(
            ts.timestamp(),
            PG_EPOCH_UNIX_SECS,
            "binary TIMESTAMP micros=0 must decode to 2000-01-01T00:00:00Z"
        ),
        other => panic!("binary TIMESTAMP param must decode to Value::Timestamp, got {other:?}"),
    }

    // +1 day offset.
    let micros_one_day: i64 = 86_400 * 1_000_000;
    match decode_parameter(&micros_one_day.to_be_bytes(), 1, 1114).expect("decode ts +1d") {
        Value::Timestamp(ts) => assert_eq!(ts.timestamp(), PG_EPOCH_UNIX_SECS + 86_400),
        other => panic!("binary TIMESTAMP param must decode to Value::Timestamp, got {other:?}"),
    }
}

#[test]
fn a4_binary_uuid_param_decodes() {
    // OID 2950 UUID, binary format: 16 raw bytes (RFC 4122).
    let bytes: [u8; 16] = [
        0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00,
    ];
    match decode_parameter(&bytes, 1, 2950).expect("decode uuid") {
        Value::Uuid(u) => assert_eq!(u.as_bytes(), &bytes, "binary UUID param must round-trip its 16 bytes"),
        other => panic!("binary UUID param must decode to Value::Uuid, got {other:?}"),
    }
}
