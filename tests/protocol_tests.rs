//! PostgreSQL Wire Protocol Integration Tests
//!
//! Tests for message encoding/decoding and protocol flows

use heliosdb_nano::{Value, Schema, Column, DataType, ColumnStorageMode};
use bytes::{BufMut, BytesMut};

/// Helper to write a C string (null-terminated)
fn write_cstring(buf: &mut BytesMut, s: &str) {
    buf.put(s.as_bytes());
    buf.put_u8(0);
}

#[tokio::test]
async fn test_message_encoding_authentication_ok() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage, AuthenticationMessage};

    let mut encoder = MessageEncoder::new();
    let msg = BackendMessage::Authentication(AuthenticationMessage::Ok);
    let encoded = encoder.encode(&msg).unwrap();

    // Check message format
    assert_eq!(encoded[0], b'R'); // Authentication message type
    assert_eq!(encoded.len(), 9); // Type (1) + Length (4) + Auth type (4)

    // Check authentication type = 0 (OK)
    let auth_type = i32::from_be_bytes([encoded[5], encoded[6], encoded[7], encoded[8]]);
    assert_eq!(auth_type, 0);
}

#[tokio::test]
async fn test_message_encoding_ready_for_query() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage, TransactionStatus};

    let mut encoder = MessageEncoder::new();
    let msg = BackendMessage::ReadyForQuery {
        status: TransactionStatus::Idle,
    };
    let encoded = encoder.encode(&msg).unwrap();

    assert_eq!(encoded[0], b'Z'); // ReadyForQuery message type
    assert_eq!(encoded[encoded.len() - 1], b'I'); // Idle status
}

#[tokio::test]
async fn test_message_encoding_command_complete() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage};

    let mut encoder = MessageEncoder::new();
    let msg = BackendMessage::CommandComplete {
        tag: "SELECT 5".to_string(),
    };
    let encoded = encoder.encode(&msg).unwrap();

    assert_eq!(encoded[0], b'C'); // CommandComplete message type
    // Verify the tag is in the message
    let tag_start = 5; // After type (1) + length (4)
    let tag_bytes = &encoded[tag_start..encoded.len()-1]; // Exclude null terminator
    assert_eq!(std::str::from_utf8(tag_bytes).unwrap(), "SELECT 5");
}

#[tokio::test]
async fn test_message_encoding_error_response() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage, error_fields};
    use std::collections::HashMap;

    let mut encoder = MessageEncoder::new();
    let mut fields = HashMap::new();
    fields.insert(error_fields::SEVERITY, "ERROR".to_string());
    fields.insert(error_fields::CODE, "42000".to_string());
    fields.insert(error_fields::MESSAGE, "Test error".to_string());

    let msg = BackendMessage::ErrorResponse { fields };
    let encoded = encoder.encode(&msg).unwrap();

    assert_eq!(encoded[0], b'E'); // ErrorResponse message type

    // Verify message contains our error fields
    let msg_str = String::from_utf8_lossy(&encoded);
    assert!(msg_str.contains("ERROR"));
    assert!(msg_str.contains("42000"));
    assert!(msg_str.contains("Test error"));
}

#[tokio::test]
async fn test_message_encoding_row_description() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage, FieldDescription};

    let mut encoder = MessageEncoder::new();
    let msg = BackendMessage::RowDescription {
        fields: vec![
            FieldDescription {
                name: "id".to_string(),
                table_oid: 0,
                column_attr: 1,
                type_oid: 23, // INT4
                type_size: 4,
                type_modifier: -1,
                format_code: 0,
            },
            FieldDescription {
                name: "name".to_string(),
                table_oid: 0,
                column_attr: 2,
                type_oid: 25, // TEXT
                type_size: -1,
                type_modifier: -1,
                format_code: 0,
            },
        ],
    };
    let encoded = encoder.encode(&msg).unwrap();

    assert_eq!(encoded[0], b'T'); // RowDescription message type

    // Check field count (2 fields)
    let field_count = i16::from_be_bytes([encoded[5], encoded[6]]);
    assert_eq!(field_count, 2);
}

#[tokio::test]
async fn test_message_encoding_data_row() {
    use heliosdb_nano::network::protocol::{MessageEncoder, BackendMessage};

    let mut encoder = MessageEncoder::new();
    let msg = BackendMessage::DataRow {
        values: vec![
            Some(b"1".to_vec()),
            Some(b"Alice".to_vec()),
            None, // NULL value
        ],
    };
    let encoded = encoder.encode(&msg).unwrap();

    assert_eq!(encoded[0], b'D'); // DataRow message type

    // Check column count
    let col_count = i16::from_be_bytes([encoded[5], encoded[6]]);
    assert_eq!(col_count, 3);
}

#[tokio::test]
async fn test_message_decoding_query() {
    use heliosdb_nano::network::protocol::{MessageDecoder, FrontendMessage};

    let mut decoder = MessageDecoder::new();

    // Build a Query message: Q + length + "SELECT 1" + null
    // Length field includes itself and payload, but NOT the type byte
    let mut buf = BytesMut::new();
    buf.put_u8(b'Q'); // Query message type

    let query = "SELECT 1";
    let length = 4 + query.len() + 1; // length field (4) + query + null terminator
    buf.put_i32(length as i32);
    write_cstring(&mut buf, query);

    decoder.buffer_data(&buf);
    let msg = decoder.decode().unwrap();

    match msg {
        Some(FrontendMessage::Query { query: q }) => {
            assert_eq!(q, "SELECT 1");
        }
        _ => panic!("Expected Query message, got {:?}", msg),
    }
}

#[tokio::test]
async fn test_message_decoding_parse() {
    use heliosdb_nano::network::protocol::{MessageDecoder, FrontendMessage};

    let mut decoder = MessageDecoder::new();

    // Build a Parse message: P + length + stmt_name + query + param_count + param_types
    let mut buf = BytesMut::new();
    buf.put_u8(b'P'); // Parse message type

    let stmt_name = "";
    let query = "SELECT $1";
    let param_count: i16 = 1;
    let param_type: i32 = 23; // INT4

    let length = 4 + stmt_name.len() + 1 + query.len() + 1 + 2 + 4;
    buf.put_i32(length as i32);
    write_cstring(&mut buf, stmt_name);
    write_cstring(&mut buf, query);
    buf.put_i16(param_count);
    buf.put_i32(param_type);

    decoder.buffer_data(&buf);
    let msg = decoder.decode().unwrap();

    match msg {
        Some(FrontendMessage::Parse { statement_name, query: q, param_types }) => {
            assert_eq!(statement_name, "");
            assert_eq!(q, "SELECT $1");
            assert_eq!(param_types.len(), 1);
            assert_eq!(param_types[0], 23);
        }
        _ => panic!("Expected Parse message, got {:?}", msg),
    }
}

#[tokio::test]
async fn test_message_decoding_bind() {
    use heliosdb_nano::network::protocol::{MessageDecoder, FrontendMessage};

    let mut decoder = MessageDecoder::new();

    // Build a Bind message
    let mut buf = BytesMut::new();
    buf.put_u8(b'B'); // Bind message type

    let portal_name = "";
    let stmt_name = "";
    let format_code_count: i16 = 0;
    let param_count: i16 = 1;
    let param_value = b"42";
    let result_format_count: i16 = 0;

    let length = 4 + portal_name.len() + 1 + stmt_name.len() + 1
        + 2 // format code count
        + 2 // param count
        + 4 + param_value.len() // param length + value
        + 2; // result format count

    buf.put_i32(length as i32);
    write_cstring(&mut buf, portal_name);
    write_cstring(&mut buf, stmt_name);
    buf.put_i16(format_code_count);
    buf.put_i16(param_count);
    buf.put_i32(param_value.len() as i32);
    buf.put(param_value.as_ref());
    buf.put_i16(result_format_count);

    decoder.buffer_data(&buf);
    let msg = decoder.decode().unwrap();

    match msg {
        Some(FrontendMessage::Bind { portal_name: p, statement_name: s, params, .. }) => {
            assert_eq!(p, "");
            assert_eq!(s, "");
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].as_ref().unwrap(), b"42");
        }
        _ => panic!("Expected Bind message, got {:?}", msg),
    }
}

#[tokio::test]
async fn test_message_decoding_execute() {
    use heliosdb_nano::network::protocol::{MessageDecoder, FrontendMessage};

    let mut decoder = MessageDecoder::new();

    // Build an Execute message: E + length + portal_name + max_rows
    let mut buf = BytesMut::new();
    buf.put_u8(b'E'); // Execute message type

    let portal_name = "";
    let max_rows: i32 = 0;

    let length = 4 + portal_name.len() + 1 + 4;
    buf.put_i32(length as i32);
    write_cstring(&mut buf, portal_name);
    buf.put_i32(max_rows);

    decoder.buffer_data(&buf);
    let msg = decoder.decode().unwrap();

    match msg {
        Some(FrontendMessage::Execute { portal_name: p, max_rows: m }) => {
            assert_eq!(p, "");
            assert_eq!(m, 0);
        }
        _ => panic!("Expected Execute message, got {:?}", msg),
    }
}

#[tokio::test]
async fn test_message_decoding_sync() {
    use heliosdb_nano::network::protocol::{MessageDecoder, FrontendMessage};

    let mut decoder = MessageDecoder::new();

    // Build a Sync message: S + length (4)
    let mut buf = BytesMut::new();
    buf.put_u8(b'S'); // Sync message type
    buf.put_i32(4); // Length only

    decoder.buffer_data(&buf);
    let msg = decoder.decode().unwrap();

    match msg {
        Some(FrontendMessage::Sync) => {
            // Success
        }
        _ => panic!("Expected Sync message, got {:?}", msg),
    }
}

#[tokio::test]
async fn test_value_to_pg_text_conversion() {
    use heliosdb_nano::network::protocol::value_to_pg_text;

    // Test various data types
    assert_eq!(value_to_pg_text(&Value::Int4(42)), b"42");
    assert_eq!(value_to_pg_text(&Value::Int8(123456)), b"123456");
    assert_eq!(value_to_pg_text(&Value::String("hello".to_string())), b"hello");
    assert_eq!(value_to_pg_text(&Value::Boolean(true)), b"t");
    assert_eq!(value_to_pg_text(&Value::Boolean(false)), b"f");
    assert_eq!(value_to_pg_text(&Value::Null), b"");
}

#[tokio::test]
async fn test_parse_pg_text_param() {
    use heliosdb_nano::network::protocol::{parse_pg_text_param, type_oid};

    // Test INT4
    let result = parse_pg_text_param(b"42", type_oid::INT4).unwrap();
    assert!(matches!(result, Value::Int4(42)));

    // Test INT8
    let result = parse_pg_text_param(b"123456", type_oid::INT8).unwrap();
    assert!(matches!(result, Value::Int8(123456)));

    // Test BOOL
    let result = parse_pg_text_param(b"t", type_oid::BOOL).unwrap();
    assert!(matches!(result, Value::Boolean(true)));

    let result = parse_pg_text_param(b"false", type_oid::BOOL).unwrap();
    assert!(matches!(result, Value::Boolean(false)));

    // Test TEXT
    let result = parse_pg_text_param(b"hello", type_oid::TEXT).unwrap();
    assert!(matches!(result, Value::String(s) if s == "hello"));

    // Test FLOAT8
    let result = parse_pg_text_param(b"3.14", type_oid::FLOAT8).unwrap();
    assert!(matches!(result, Value::Float8(f) if (f - 3.14).abs() < 0.001));
}

#[tokio::test]
async fn test_schema_to_row_description() {
    use heliosdb_nano::network::protocol::schema_to_row_description;

    let schema = Schema::new(vec![
        Column {
            name: "id".to_string(),
            data_type: DataType::Int4,
            nullable: false,
            primary_key: true,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: ColumnStorageMode::Default,
        },
        Column {
            name: "name".to_string(),
            data_type: DataType::Text,
            nullable: true,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: ColumnStorageMode::Default,
        },
    ]);

    let fields = schema_to_row_description(&schema);

    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "id");
    assert_eq!(fields[0].type_oid, 23); // INT4
    assert_eq!(fields[0].type_size, 4);

    assert_eq!(fields[1].name, "name");
    assert_eq!(fields[1].type_oid, 25); // TEXT
    assert_eq!(fields[1].type_size, -1); // Variable length
}

#[test]
fn test_sqlstate_codes_exist() {
    use heliosdb_nano::network::protocol::sqlstate;

    // Test that common error codes are defined
    assert_eq!(sqlstate::SUCCESSFUL_COMPLETION, "00000");
    assert_eq!(sqlstate::SYNTAX_ERROR, "42601");
    assert_eq!(sqlstate::UNDEFINED_TABLE, "42P01");
    assert_eq!(sqlstate::PROTOCOL_VIOLATION, "08P01");
    assert_eq!(sqlstate::CONNECTION_FAILURE, "08006");
    assert_eq!(sqlstate::INVALID_PASSWORD, "28P01");
    assert_eq!(sqlstate::INTERNAL_ERROR, "XX000");
}
