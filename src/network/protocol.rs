//! PostgreSQL wire protocol v3 message parsing and encoding
//!
//! Reference: https://www.postgresql.org/docs/current/protocol.html

use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;
use std::io::{self, Cursor};
use crate::{DataType, Value, Schema};

/// PostgreSQL protocol version (3.0)
pub const PROTOCOL_VERSION: i32 = 196608; // 3.0 in major.minor format

/// Maximum message size (10MB)
const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

/// Frontend message types (client to server)
#[derive(Debug, Clone)]
pub enum FrontendMessage {
    /// Startup message (no message type byte)
    Startup {
        protocol_version: i32,
        params: HashMap<String, String>,
    },
    /// Simple query
    Query {
        query: String,
    },
    /// Parse statement
    Parse {
        statement_name: String,
        query: String,
        param_types: Vec<i32>,
    },
    /// Bind parameters to statement
    Bind {
        portal_name: String,
        statement_name: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    /// Execute portal
    Execute {
        portal_name: String,
        max_rows: i32,
    },
    /// Describe statement or portal
    Describe {
        kind: DescribeKind,
        name: String,
    },
    /// Close statement or portal
    Close {
        kind: CloseKind,
        name: String,
    },
    /// Sync (end of extended query)
    Sync,
    /// Flush
    Flush,
    /// Terminate connection
    Terminate,
    /// Password response
    PasswordMessage {
        password: String,
    },
}

/// Describe message kind
#[derive(Debug, Clone, Copy)]
pub enum DescribeKind {
    /// Describe a statement
    Statement,
    /// Describe a portal
    Portal,
}

/// Close message kind
#[derive(Debug, Clone, Copy)]
pub enum CloseKind {
    /// Close a statement
    Statement,
    /// Close a portal
    Portal,
}

/// Backend message types (server to client)
#[derive(Debug, Clone)]
pub enum BackendMessage {
    /// Authentication request
    Authentication(AuthenticationMessage),
    /// Backend key data (for cancellation)
    BackendKeyData {
        process_id: i32,
        secret_key: i32,
    },
    /// Bind complete
    BindComplete,
    /// Close complete
    CloseComplete,
    /// Command complete
    CommandComplete {
        tag: String,
    },
    /// Data row
    DataRow {
        values: Vec<Option<Vec<u8>>>,
    },
    /// Empty query response
    EmptyQueryResponse,
    /// Error response
    ErrorResponse {
        fields: HashMap<u8, String>,
    },
    /// No data
    NoData,
    /// Notice response
    NoticeResponse {
        fields: HashMap<u8, String>,
    },
    /// Parameter description
    ParameterDescription {
        param_types: Vec<i32>,
    },
    /// Parameter status
    ParameterStatus {
        name: String,
        value: String,
    },
    /// Parse complete
    ParseComplete,
    /// Ready for query
    ReadyForQuery {
        status: TransactionStatus,
    },
    /// Row description
    RowDescription {
        fields: Vec<FieldDescription>,
    },
}

/// Authentication message types
#[derive(Debug, Clone)]
pub enum AuthenticationMessage {
    /// Authentication successful
    Ok,
    /// Kerberos V5
    KerberosV5,
    /// Clear text password
    CleartextPassword,
    /// MD5 password
    MD5Password {
        salt: [u8; 4],
    },
    /// SCRAM-SHA-256
    SASL {
        mechanisms: Vec<String>,
    },
    /// SASL continue
    SASLContinue {
        data: Vec<u8>,
    },
    /// SASL final
    SASLFinal {
        data: Vec<u8>,
    },
}

/// Transaction status
#[derive(Debug, Clone, Copy)]
pub enum TransactionStatus {
    /// Idle (not in transaction)
    Idle,
    /// In transaction block
    InTransaction,
    /// Failed transaction
    Failed,
}

/// Field description in row description
#[derive(Debug, Clone)]
pub struct FieldDescription {
    /// Field name
    pub name: String,
    /// Table OID (0 if not from table)
    pub table_oid: i32,
    /// Column attribute number (0 if not from table)
    pub column_attr: i16,
    /// Data type OID
    pub type_oid: i32,
    /// Data type size (-1 for variable)
    pub type_size: i16,
    /// Type modifier
    pub type_modifier: i32,
    /// Format code (0 = text, 1 = binary)
    pub format_code: i16,
}

/// PostgreSQL error/notice field codes
#[allow(dead_code)]
pub mod error_fields {
    /// Severity (always present)
    pub const SEVERITY: u8 = b'S';
    /// Severity (non-localized)
    pub const SEVERITY_NON_LOCALIZED: u8 = b'V';
    /// SQL state code
    pub const CODE: u8 = b'C';
    /// Message (always present)
    pub const MESSAGE: u8 = b'M';
    /// Detail
    pub const DETAIL: u8 = b'D';
    /// Hint
    pub const HINT: u8 = b'H';
    /// Position
    pub const POSITION: u8 = b'P';
    /// Internal position
    pub const INTERNAL_POSITION: u8 = b'p';
    /// Internal query
    pub const INTERNAL_QUERY: u8 = b'q';
    /// Where
    pub const WHERE: u8 = b'W';
    /// Schema name
    pub const SCHEMA: u8 = b's';
    /// Table name
    pub const TABLE: u8 = b't';
    /// Column name
    pub const COLUMN: u8 = b'c';
    /// Data type name
    pub const DATA_TYPE: u8 = b'd';
    /// Constraint name
    pub const CONSTRAINT: u8 = b'n';
    /// File
    pub const FILE: u8 = b'F';
    /// Line
    pub const LINE: u8 = b'L';
    /// Routine
    pub const ROUTINE: u8 = b'R';
}

/// PostgreSQL SQLSTATE error codes
/// Reference: https://www.postgresql.org/docs/current/errcodes-appendix.html
#[allow(dead_code)]
pub mod sqlstate {
    // Class 00 - Successful Completion
    pub const SUCCESSFUL_COMPLETION: &str = "00000";

    // Class 01 - Warning
    pub const WARNING: &str = "01000";
    pub const DYNAMIC_RESULT_SETS_RETURNED: &str = "0100C";
    pub const PRIVILEGE_NOT_GRANTED: &str = "01007";
    pub const PRIVILEGE_NOT_REVOKED: &str = "01006";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "01004";
    pub const DEPRECATED_FEATURE: &str = "01P01";

    // Class 02 - No Data
    pub const NO_DATA: &str = "02000";
    pub const NO_ADDITIONAL_DYNAMIC_RESULT_SETS_RETURNED: &str = "02001";

    // Class 03 - SQL Statement Not Yet Complete
    pub const SQL_STATEMENT_NOT_YET_COMPLETE: &str = "03000";

    // Class 08 - Connection Exception
    pub const CONNECTION_EXCEPTION: &str = "08000";
    pub const CONNECTION_DOES_NOT_EXIST: &str = "08003";
    pub const CONNECTION_FAILURE: &str = "08006";
    pub const SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION: &str = "08001";
    pub const SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION: &str = "08004";
    pub const TRANSACTION_RESOLUTION_UNKNOWN: &str = "08007";
    pub const PROTOCOL_VIOLATION: &str = "08P01";

    // Class 09 - Triggered Action Exception
    pub const TRIGGERED_ACTION_EXCEPTION: &str = "09000";

    // Class 0A - Feature Not Supported
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";

    // Class 0B - Invalid Transaction Initiation
    pub const INVALID_TRANSACTION_INITIATION: &str = "0B000";

    // Class 0F - Locator Exception
    pub const LOCATOR_EXCEPTION: &str = "0F000";
    pub const INVALID_LOCATOR_SPECIFICATION: &str = "0F001";

    // Class 0L - Invalid Grantor
    pub const INVALID_GRANTOR: &str = "0L000";
    pub const INVALID_GRANT_OPERATION: &str = "0LP01";

    // Class 0P - Invalid Role Specification
    pub const INVALID_ROLE_SPECIFICATION: &str = "0P000";

    // Class 0Z - Diagnostics Exception
    pub const DIAGNOSTICS_EXCEPTION: &str = "0Z000";
    pub const STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER: &str = "0Z002";

    // Class 20 - Case Not Found
    pub const CASE_NOT_FOUND: &str = "20000";

    // Class 21 - Cardinality Violation
    pub const CARDINALITY_VIOLATION: &str = "21000";

    // Class 22 - Data Exception
    pub const DATA_EXCEPTION: &str = "22000";
    pub const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";
    pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const ERROR_IN_ASSIGNMENT: &str = "22005";
    pub const ESCAPE_CHARACTER_CONFLICT: &str = "2200B";
    pub const INDICATOR_OVERFLOW: &str = "22022";
    pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
    pub const INVALID_ARGUMENT_FOR_LOGARITHM: &str = "2201E";
    pub const INVALID_ARGUMENT_FOR_NTILE_FUNCTION: &str = "22014";
    pub const INVALID_ARGUMENT_FOR_NTH_VALUE_FUNCTION: &str = "22016";
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION: &str = "2201G";
    pub const INVALID_CHARACTER_VALUE_FOR_CAST: &str = "22018";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const INVALID_ESCAPE_CHARACTER: &str = "22019";
    pub const INVALID_ESCAPE_OCTET: &str = "2200D";
    pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
    pub const NONSTANDARD_USE_OF_ESCAPE_CHARACTER: &str = "22P06";
    pub const INVALID_INDICATOR_PARAMETER_VALUE: &str = "22010";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201W";
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: &str = "2201X";
    pub const INVALID_TABLESAMPLE_ARGUMENT: &str = "2202H";
    pub const INVALID_TABLESAMPLE_REPEAT: &str = "2202G";
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: &str = "2200C";
    pub const MOST_SPECIFIC_TYPE_MISMATCH: &str = "2200G";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const NULL_VALUE_NO_INDICATOR_PARAMETER: &str = "22002";
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    pub const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
    pub const STRING_DATA_RIGHT_TRUNCATION_DATA: &str = "22001";
    pub const SUBSTRING_ERROR: &str = "22011";
    pub const TRIM_ERROR: &str = "22027";
    pub const UNTERMINATED_C_STRING: &str = "22024";
    pub const ZERO_LENGTH_CHARACTER_STRING: &str = "2200F";
    pub const FLOATING_POINT_EXCEPTION: &str = "22P01";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
    pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
    pub const UNTRANSLATABLE_CHARACTER: &str = "22P05";
    pub const NOT_AN_XML_DOCUMENT: &str = "2200L";
    pub const INVALID_XML_DOCUMENT: &str = "2200M";
    pub const INVALID_XML_CONTENT: &str = "2200N";
    pub const INVALID_XML_COMMENT: &str = "2200S";
    pub const INVALID_XML_PROCESSING_INSTRUCTION: &str = "2200T";

    // Class 23 - Integrity Constraint Violation
    pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";
    pub const RESTRICT_VIOLATION: &str = "23001";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const EXCLUSION_VIOLATION: &str = "23P01";

    // Class 24 - Invalid Cursor State
    pub const INVALID_CURSOR_STATE: &str = "24000";

    // Class 25 - Invalid Transaction State
    pub const INVALID_TRANSACTION_STATE: &str = "25000";
    pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
    pub const BRANCH_TRANSACTION_ALREADY_ACTIVE: &str = "25002";
    pub const HELD_CURSOR_REQUIRES_SAME_ISOLATION_LEVEL: &str = "25008";
    pub const INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION: &str = "25003";
    pub const INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION: &str = "25004";
    pub const NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION: &str = "25005";
    pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
    pub const SCHEMA_AND_DATA_STATEMENT_MIXING_NOT_SUPPORTED: &str = "25007";
    pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
    pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";

    // Class 26 - Invalid SQL Statement Name
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";

    // Class 27 - Triggered Data Change Violation
    pub const TRIGGERED_DATA_CHANGE_VIOLATION: &str = "27000";

    // Class 28 - Invalid Authorization Specification
    pub const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";
    pub const INVALID_PASSWORD: &str = "28P01";

    // Class 2B - Dependent Privilege Descriptors Still Exist
    pub const DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST: &str = "2B000";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";

    // Class 2D - Invalid Transaction Termination
    pub const INVALID_TRANSACTION_TERMINATION: &str = "2D000";

    // Class 2F - SQL Routine Exception
    pub const SQL_ROUTINE_EXCEPTION: &str = "2F000";
    pub const FUNCTION_EXECUTED_NO_RETURN_STATEMENT: &str = "2F005";
    pub const MODIFYING_SQL_DATA_NOT_PERMITTED: &str = "2F002";
    pub const PROHIBITED_SQL_STATEMENT_ATTEMPTED: &str = "2F003";
    pub const READING_SQL_DATA_NOT_PERMITTED: &str = "2F004";

    // Class 34 - Invalid Cursor Name
    pub const INVALID_CURSOR_NAME: &str = "34000";

    // Class 38 - External Routine Exception
    pub const EXTERNAL_ROUTINE_EXCEPTION: &str = "38000";
    pub const CONTAINING_SQL_NOT_PERMITTED: &str = "38001";
    pub const MODIFYING_SQL_DATA_NOT_PERMITTED_EXTERNAL: &str = "38002";
    pub const PROHIBITED_SQL_STATEMENT_ATTEMPTED_EXTERNAL: &str = "38003";
    pub const READING_SQL_DATA_NOT_PERMITTED_EXTERNAL: &str = "38004";

    // Class 39 - External Routine Invocation Exception
    pub const EXTERNAL_ROUTINE_INVOCATION_EXCEPTION: &str = "39000";
    pub const INVALID_SQLSTATE_RETURNED: &str = "39001";
    pub const NULL_VALUE_NOT_ALLOWED_EXTERNAL: &str = "39004";
    pub const TRIGGER_PROTOCOL_VIOLATED: &str = "39P01";
    pub const SRF_PROTOCOL_VIOLATED: &str = "39P02";
    pub const EVENT_TRIGGER_PROTOCOL_VIOLATED: &str = "39P03";

    // Class 3B - Savepoint Exception
    pub const SAVEPOINT_EXCEPTION: &str = "3B000";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";

    // Class 3D - Invalid Catalog Name
    pub const INVALID_CATALOG_NAME: &str = "3D000";

    // Class 3F - Invalid Schema Name
    pub const INVALID_SCHEMA_NAME: &str = "3F000";

    // Class 40 - Transaction Rollback
    pub const TRANSACTION_ROLLBACK: &str = "40000";
    pub const TRANSACTION_INTEGRITY_CONSTRAINT_VIOLATION: &str = "40002";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const STATEMENT_COMPLETION_UNKNOWN: &str = "40003";
    pub const DEADLOCK_DETECTED: &str = "40P01";

    // Class 42 - Syntax Error or Access Rule Violation
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: &str = "42000";
    pub const SYNTAX_ERROR: &str = "42601";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const CANNOT_COERCE: &str = "42846";
    pub const GROUPING_ERROR: &str = "42803";
    pub const WINDOWING_ERROR: &str = "42P20";
    pub const INVALID_RECURSION: &str = "42P19";
    pub const INVALID_FOREIGN_KEY: &str = "42830";
    pub const INVALID_NAME: &str = "42602";
    pub const NAME_TOO_LONG: &str = "42622";
    pub const RESERVED_NAME: &str = "42939";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const INDETERMINATE_DATATYPE: &str = "42P18";
    pub const COLLATION_MISMATCH: &str = "42P21";
    pub const INDETERMINATE_COLLATION: &str = "42P22";
    pub const WRONG_OBJECT_TYPE: &str = "42809";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const UNDEFINED_PARAMETER: &str = "42P02";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_CURSOR: &str = "42P03";
    pub const DUPLICATE_DATABASE: &str = "42P04";
    pub const DUPLICATE_FUNCTION: &str = "42723";
    pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_ALIAS: &str = "42712";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const AMBIGUOUS_FUNCTION: &str = "42725";
    pub const AMBIGUOUS_PARAMETER: &str = "42P08";
    pub const AMBIGUOUS_ALIAS: &str = "42P09";
    pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
    pub const INVALID_COLUMN_DEFINITION: &str = "42611";
    pub const INVALID_CURSOR_DEFINITION: &str = "42P11";
    pub const INVALID_DATABASE_DEFINITION: &str = "42P12";
    pub const INVALID_FUNCTION_DEFINITION: &str = "42P13";
    pub const INVALID_PREPARED_STATEMENT_DEFINITION: &str = "42P14";
    pub const INVALID_SCHEMA_DEFINITION: &str = "42P15";
    pub const INVALID_TABLE_DEFINITION: &str = "42P16";
    pub const INVALID_OBJECT_DEFINITION: &str = "42P17";

    // Class 44 - WITH CHECK OPTION Violation
    pub const WITH_CHECK_OPTION_VIOLATION: &str = "44000";

    // Class 53 - Insufficient Resources
    pub const INSUFFICIENT_RESOURCES: &str = "53000";
    pub const DISK_FULL: &str = "53100";
    pub const OUT_OF_MEMORY: &str = "53200";
    pub const TOO_MANY_CONNECTIONS: &str = "53300";
    pub const CONFIGURATION_LIMIT_EXCEEDED: &str = "53400";

    // Class 54 - Program Limit Exceeded
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const STATEMENT_TOO_COMPLEX: &str = "54001";
    pub const TOO_MANY_COLUMNS: &str = "54011";
    pub const TOO_MANY_ARGUMENTS: &str = "54023";

    // Class 55 - Object Not In Prerequisite State
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
    pub const OBJECT_IN_USE: &str = "55006";
    pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";

    // Class 57 - Operator Intervention
    pub const OPERATOR_INTERVENTION: &str = "57000";
    pub const QUERY_CANCELED: &str = "57014";
    pub const ADMIN_SHUTDOWN: &str = "57P01";
    pub const CRASH_SHUTDOWN: &str = "57P02";
    pub const CANNOT_CONNECT_NOW: &str = "57P03";
    pub const DATABASE_DROPPED: &str = "57P04";

    // Class 58 - System Error
    pub const SYSTEM_ERROR: &str = "58000";
    pub const IO_ERROR: &str = "58030";
    pub const UNDEFINED_FILE: &str = "58P01";
    pub const DUPLICATE_FILE: &str = "58P02";

    // Class F0 - Configuration File Error
    pub const CONFIG_FILE_ERROR: &str = "F0000";
    pub const LOCK_FILE_EXISTS: &str = "F0001";

    // Class HV - Foreign Data Wrapper Error
    pub const FDW_ERROR: &str = "HV000";
    pub const FDW_COLUMN_NAME_NOT_FOUND: &str = "HV005";
    pub const FDW_DYNAMIC_PARAMETER_VALUE_NEEDED: &str = "HV002";
    pub const FDW_FUNCTION_SEQUENCE_ERROR: &str = "HV010";
    pub const FDW_INCONSISTENT_DESCRIPTOR_INFORMATION: &str = "HV021";
    pub const FDW_INVALID_ATTRIBUTE_VALUE: &str = "HV024";
    pub const FDW_INVALID_COLUMN_NAME: &str = "HV007";
    pub const FDW_INVALID_COLUMN_NUMBER: &str = "HV008";
    pub const FDW_INVALID_DATA_TYPE: &str = "HV004";
    pub const FDW_INVALID_DATA_TYPE_DESCRIPTORS: &str = "HV006";
    pub const FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER: &str = "HV091";
    pub const FDW_INVALID_HANDLE: &str = "HV00B";
    pub const FDW_INVALID_OPTION_INDEX: &str = "HV00C";
    pub const FDW_INVALID_OPTION_NAME: &str = "HV00D";
    pub const FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH: &str = "HV090";
    pub const FDW_INVALID_STRING_FORMAT: &str = "HV00A";
    pub const FDW_INVALID_USE_OF_NULL_POINTER: &str = "HV009";
    pub const FDW_TOO_MANY_HANDLES: &str = "HV014";
    pub const FDW_OUT_OF_MEMORY: &str = "HV001";
    pub const FDW_NO_SCHEMAS: &str = "HV00P";
    pub const FDW_OPTION_NAME_NOT_FOUND: &str = "HV00J";
    pub const FDW_REPLY_HANDLE: &str = "HV00K";
    pub const FDW_SCHEMA_NOT_FOUND: &str = "HV00Q";
    pub const FDW_TABLE_NOT_FOUND: &str = "HV00R";
    pub const FDW_UNABLE_TO_CREATE_EXECUTION: &str = "HV00L";
    pub const FDW_UNABLE_TO_CREATE_REPLY: &str = "HV00M";
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION: &str = "HV00N";

    // Class P0 - PL/pgSQL Error
    pub const PLPGSQL_ERROR: &str = "P0000";
    pub const RAISE_EXCEPTION: &str = "P0001";
    pub const NO_DATA_FOUND: &str = "P0002";
    pub const TOO_MANY_ROWS: &str = "P0003";
    pub const ASSERT_FAILURE: &str = "P0004";

    // Class XX - Internal Error
    pub const INTERNAL_ERROR: &str = "XX000";
    pub const DATA_CORRUPTED: &str = "XX001";
    pub const INDEX_CORRUPTED: &str = "XX002";
}

/// Message encoder
pub struct MessageEncoder {
    buf: BytesMut,
}

impl MessageEncoder {
    /// Create a new encoder
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(1024),
        }
    }

    /// Encode a backend message
    #[allow(clippy::indexing_slicing)]
    // SAFETY: Buffer slicing uses positions derived from our own writes; lengths are validated
    pub fn encode(&mut self, msg: &BackendMessage) -> io::Result<Vec<u8>> {
        self.buf.clear();

        match msg {
            BackendMessage::Authentication(auth) => self.encode_authentication(auth)?,
            BackendMessage::BackendKeyData { process_id, secret_key } => {
                self.buf.put_u8(b'K');
                self.buf.put_i32(12); // length
                self.buf.put_i32(*process_id);
                self.buf.put_i32(*secret_key);
            }
            BackendMessage::BindComplete => {
                self.buf.put_u8(b'2');
                self.buf.put_i32(4); // length
            }
            BackendMessage::CloseComplete => {
                self.buf.put_u8(b'3');
                self.buf.put_i32(4); // length
            }
            BackendMessage::CommandComplete { tag } => {
                self.buf.put_u8(b'C');
                let len = 4 + tag.len() + 1;
                self.buf.put_i32(len as i32);
                self.buf.put(tag.as_bytes());
                self.buf.put_u8(0);
            }
            BackendMessage::DataRow { values } => {
                self.buf.put_u8(b'D');
                let len_pos = self.buf.len();
                self.buf.put_i32(0); // placeholder for length

                self.buf.put_i16(values.len() as i16);
                for value in values {
                    match value {
                        Some(v) => {
                            self.buf.put_i32(v.len() as i32);
                            self.buf.put(v.as_slice());
                        }
                        None => {
                            self.buf.put_i32(-1); // NULL
                        }
                    }
                }

                // Update length
                let total_len = self.buf.len() - len_pos;
                let mut len_buf = &mut self.buf[len_pos..len_pos + 4];
                len_buf.put_i32(total_len as i32);
            }
            BackendMessage::EmptyQueryResponse => {
                self.buf.put_u8(b'I');
                self.buf.put_i32(4); // length
            }
            BackendMessage::ErrorResponse { fields } => {
                self.encode_error_or_notice(b'E', fields)?;
            }
            BackendMessage::NoData => {
                self.buf.put_u8(b'n');
                self.buf.put_i32(4); // length
            }
            BackendMessage::NoticeResponse { fields } => {
                self.encode_error_or_notice(b'N', fields)?;
            }
            BackendMessage::ParameterDescription { param_types } => {
                self.buf.put_u8(b't');
                let len = 4 + 2 + (param_types.len() * 4);
                self.buf.put_i32(len as i32);
                self.buf.put_i16(param_types.len() as i16);
                for oid in param_types {
                    self.buf.put_i32(*oid);
                }
            }
            BackendMessage::ParameterStatus { name, value } => {
                self.buf.put_u8(b'S');
                let len = 4 + name.len() + 1 + value.len() + 1;
                self.buf.put_i32(len as i32);
                self.buf.put(name.as_bytes());
                self.buf.put_u8(0);
                self.buf.put(value.as_bytes());
                self.buf.put_u8(0);
            }
            BackendMessage::ParseComplete => {
                self.buf.put_u8(b'1');
                self.buf.put_i32(4); // length
            }
            BackendMessage::ReadyForQuery { status } => {
                self.buf.put_u8(b'Z');
                self.buf.put_i32(5); // length
                self.buf.put_u8(match status {
                    TransactionStatus::Idle => b'I',
                    TransactionStatus::InTransaction => b'T',
                    TransactionStatus::Failed => b'E',
                });
            }
            BackendMessage::RowDescription { fields } => {
                self.buf.put_u8(b'T');
                let len_pos = self.buf.len();
                self.buf.put_i32(0); // placeholder for length

                self.buf.put_i16(fields.len() as i16);
                for field in fields {
                    self.buf.put(field.name.as_bytes());
                    self.buf.put_u8(0);
                    self.buf.put_i32(field.table_oid);
                    self.buf.put_i16(field.column_attr);
                    self.buf.put_i32(field.type_oid);
                    self.buf.put_i16(field.type_size);
                    self.buf.put_i32(field.type_modifier);
                    self.buf.put_i16(field.format_code);
                }

                // Update length
                let total_len = self.buf.len() - len_pos;
                let mut len_buf = &mut self.buf[len_pos..len_pos + 4];
                len_buf.put_i32(total_len as i32);
            }
        }

        Ok(self.buf.to_vec())
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Buffer slicing uses positions from our own writes
    fn encode_authentication(&mut self, auth: &AuthenticationMessage) -> io::Result<()> {
        self.buf.put_u8(b'R');

        match auth {
            AuthenticationMessage::Ok => {
                self.buf.put_i32(8); // length
                self.buf.put_i32(0); // type = OK
            }
            AuthenticationMessage::KerberosV5 => {
                self.buf.put_i32(8);
                self.buf.put_i32(2);
            }
            AuthenticationMessage::CleartextPassword => {
                self.buf.put_i32(8);
                self.buf.put_i32(3);
            }
            AuthenticationMessage::MD5Password { salt } => {
                self.buf.put_i32(12);
                self.buf.put_i32(5);
                self.buf.put(salt.as_ref());
            }
            AuthenticationMessage::SASL { mechanisms } => {
                let len_pos = self.buf.len();
                self.buf.put_i32(0); // placeholder
                self.buf.put_i32(10); // SASL type

                for mech in mechanisms {
                    self.buf.put(mech.as_bytes());
                    self.buf.put_u8(0);
                }
                self.buf.put_u8(0); // terminator

                // Update length
                let total_len = self.buf.len() - len_pos;
                let mut len_buf = &mut self.buf[len_pos..len_pos + 4];
                len_buf.put_i32(total_len as i32);
            }
            AuthenticationMessage::SASLContinue { data } => {
                self.buf.put_i32(4 + 4 + data.len() as i32);
                self.buf.put_i32(11); // SASL continue type
                self.buf.put(data.as_slice());
            }
            AuthenticationMessage::SASLFinal { data } => {
                self.buf.put_i32(4 + 4 + data.len() as i32);
                self.buf.put_i32(12); // SASL final type
                self.buf.put(data.as_slice());
            }
        }

        Ok(())
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Buffer slicing uses positions from our own writes
    fn encode_error_or_notice(&mut self, msg_type: u8, fields: &HashMap<u8, String>) -> io::Result<()> {
        self.buf.put_u8(msg_type);
        let len_pos = self.buf.len();
        self.buf.put_i32(0); // placeholder for length

        for (field_type, value) in fields {
            self.buf.put_u8(*field_type);
            self.buf.put(value.as_bytes());
            self.buf.put_u8(0);
        }
        self.buf.put_u8(0); // terminator

        // Update length
        let total_len = self.buf.len() - len_pos;
        let mut len_buf = &mut self.buf[len_pos..len_pos + 4];
        len_buf.put_i32(total_len as i32);

        Ok(())
    }
}

impl Default for MessageEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Message decoder
pub struct MessageDecoder {
    buf: BytesMut,
}

impl MessageDecoder {
    /// Create a new decoder
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8192),
        }
    }

    /// Add data to the buffer
    pub fn buffer_data(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to decode a frontend message from the buffer
    #[allow(clippy::indexing_slicing)]
    // SAFETY: Buffer indexing is guarded by length checks (`.is_empty()`, `.len() >= N`)
    pub fn decode(&mut self) -> io::Result<Option<FrontendMessage>> {
        // Check for regular messages (with type byte) first
        if self.buf.is_empty() {
            return Ok(None);
        }

        let first_byte = self.buf[0];

        // Check if this looks like a regular message type (ASCII letter or known type)
        let is_regular_message = matches!(first_byte,
            b'Q' | b'P' | b'B' | b'E' | b'D' | b'C' | b'S' | b'H' | b'X' | b'p'
        );

        // If not a regular message, try startup message
        if !is_regular_message && self.buf.len() >= 4 {
            let mut cursor = Cursor::new(&self.buf[..]);
            let msg_len = cursor.get_i32() as usize;

            if msg_len > MAX_MESSAGE_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Message too large",
                ));
            }

            // If this looks like a startup message (reasonable length, starts with protocol version)
            if self.buf.len() >= msg_len && msg_len >= 8 {
                let protocol_version = cursor.get_i32();

                // Check if this is a valid protocol version
                if protocol_version == PROTOCOL_VERSION || protocol_version == 80877103 {
                    // Decode startup message
                    let msg = self.decode_startup()?;
                    self.buf.advance(msg_len);
                    return Ok(Some(msg));
                }
            }
        }

        let msg_type = first_byte;

        if self.buf.len() < 5 {
            return Ok(None); // Need at least type + length
        }

        let msg_len = {
            let mut cursor = Cursor::new(&self.buf[1..]);
            cursor.get_i32() as usize
        };

        if msg_len > MAX_MESSAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message too large",
            ));
        }

        let total_len = 1 + msg_len; // type byte + length field + payload

        if self.buf.len() < total_len {
            return Ok(None); // Not enough data yet
        }

        // Decode the message
        let msg = match msg_type {
            b'Q' => self.decode_query()?,
            b'P' => self.decode_parse()?,
            b'B' => self.decode_bind()?,
            b'E' => self.decode_execute()?,
            b'D' => self.decode_describe()?,
            b'C' => self.decode_close()?,
            b'S' => Some(FrontendMessage::Sync),
            b'H' => Some(FrontendMessage::Flush),
            b'X' => Some(FrontendMessage::Terminate),
            b'p' => self.decode_password()?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unknown message type: {}", msg_type as char),
                ));
            }
        };

        // Advance buffer past the message
        self.buf.advance(total_len);

        Ok(msg)
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after length validation in decode()
    fn decode_startup(&mut self) -> io::Result<FrontendMessage> {
        let mut cursor = Cursor::new(&self.buf[..]);
        let _msg_len = cursor.get_i32();
        let protocol_version = cursor.get_i32();

        // Special case: SSL request
        if protocol_version == 80877103 {
            // For now, we don't support SSL
            return Ok(FrontendMessage::Startup {
                protocol_version,
                params: HashMap::new(),
            });
        }

        let mut params = HashMap::new();

        loop {
            let key = read_cstring(&mut cursor)?;
            if key.is_empty() {
                break;
            }
            let value = read_cstring(&mut cursor)?;
            params.insert(key, value);
        }

        Ok(FrontendMessage::Startup {
            protocol_version,
            params,
        })
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_query(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let query = read_cstring(&mut cursor)?;

        Ok(Some(FrontendMessage::Query { query }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_parse(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let statement_name = read_cstring(&mut cursor)?;
        let query = read_cstring(&mut cursor)?;
        let num_params = cursor.get_i16() as usize;

        let mut param_types = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            param_types.push(cursor.get_i32());
        }

        Ok(Some(FrontendMessage::Parse {
            statement_name,
            query,
            param_types,
        }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_bind(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let portal_name = read_cstring(&mut cursor)?;
        let statement_name = read_cstring(&mut cursor)?;

        // Parameter format codes
        let num_format_codes = cursor.get_i16() as usize;
        let mut param_formats = Vec::with_capacity(num_format_codes);
        for _ in 0..num_format_codes {
            param_formats.push(cursor.get_i16());
        }

        // Parameter values
        let num_params = cursor.get_i16() as usize;
        let mut params = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            let param_len = cursor.get_i32();
            if param_len == -1 {
                params.push(None); // NULL
            } else {
                let mut param_data = vec![0u8; param_len as usize];
                cursor.copy_to_slice(&mut param_data);
                params.push(Some(param_data));
            }
        }

        // Result format codes
        let num_result_formats = cursor.get_i16() as usize;
        let mut result_formats = Vec::with_capacity(num_result_formats);
        for _ in 0..num_result_formats {
            result_formats.push(cursor.get_i16());
        }

        Ok(Some(FrontendMessage::Bind {
            portal_name,
            statement_name,
            param_formats,
            params,
            result_formats,
        }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_execute(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let portal_name = read_cstring(&mut cursor)?;
        let max_rows = cursor.get_i32();

        Ok(Some(FrontendMessage::Execute {
            portal_name,
            max_rows,
        }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_describe(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let kind_byte = cursor.get_u8();
        let name = read_cstring(&mut cursor)?;

        let kind = match kind_byte {
            b'S' => DescribeKind::Statement,
            b'P' => DescribeKind::Portal,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid describe kind",
                ));
            }
        };

        Ok(Some(FrontendMessage::Describe { kind, name }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_close(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let kind_byte = cursor.get_u8();
        let name = read_cstring(&mut cursor)?;

        let kind = match kind_byte {
            b'S' => CloseKind::Statement,
            b'P' => CloseKind::Portal,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid close kind",
                ));
            }
        };

        Ok(Some(FrontendMessage::Close { kind, name }))
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: Called after `self.buf.len() >= total_len` validation in decode()
    fn decode_password(&mut self) -> io::Result<Option<FrontendMessage>> {
        let mut cursor = Cursor::new(&self.buf[1..]);
        let _msg_len = cursor.get_i32();
        let password = read_cstring(&mut cursor)?;

        Ok(Some(FrontendMessage::PasswordMessage { password }))
    }
}

impl Default for MessageDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a null-terminated C string from cursor
fn read_cstring(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        if !cursor.has_remaining() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Unexpected end of data while reading C string",
            ));
        }
        let byte = cursor.get_u8();
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    String::from_utf8(bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("Invalid UTF-8: {}", e))
    })
}

/// PostgreSQL type OIDs
#[allow(dead_code)]
pub mod type_oid {
    pub const BOOL: i32 = 16;
    pub const BYTEA: i32 = 17;
    pub const CHAR: i32 = 18;
    pub const INT8: i32 = 20;
    pub const INT2: i32 = 21;
    pub const INT4: i32 = 23;
    pub const TEXT: i32 = 25;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const DATE: i32 = 1082;
    pub const TIME: i32 = 1083;
    pub const VARCHAR: i32 = 1043;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const UUID: i32 = 2950;
    pub const JSON: i32 = 114;
    pub const JSONB: i32 = 3802;
}

/// Convert DataType to PostgreSQL type OID
pub fn datatype_to_oid(dt: &DataType) -> i32 {
    match dt {
        DataType::Boolean => type_oid::BOOL,
        DataType::Int4 => type_oid::INT4,
        DataType::Int8 => type_oid::INT8,
        DataType::Float4 => type_oid::FLOAT4,
        DataType::Float8 => type_oid::FLOAT8,
        DataType::Text => type_oid::TEXT,
        DataType::Varchar(_) => type_oid::VARCHAR,
        DataType::Timestamp => type_oid::TIMESTAMP,
        DataType::Timestamptz => type_oid::TIMESTAMPTZ,
        DataType::Date => type_oid::DATE,
        DataType::Time => type_oid::TIME,
        DataType::Uuid => type_oid::UUID,
        DataType::Json => type_oid::JSON,
        DataType::Jsonb => type_oid::JSONB,
        DataType::Bytea => type_oid::BYTEA,
        _ => type_oid::TEXT, // Default to TEXT for unknown types
    }
}

/// Convert Value to PostgreSQL wire format (text)
pub fn value_to_pg_text(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => vec![],
        Value::Boolean(b) => (if *b { "t" } else { "f" }).as_bytes().to_vec(),
        Value::Int4(i) => i.to_string().as_bytes().to_vec(),
        Value::Int8(i) => i.to_string().as_bytes().to_vec(),
        Value::Float4(f) => f.to_string().as_bytes().to_vec(),
        Value::Float8(f) => f.to_string().as_bytes().to_vec(),
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::Timestamp(ts) => ts.to_rfc3339().as_bytes().to_vec(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string().as_bytes().to_vec(),
        Value::Time(t) => t.format("%H:%M:%S%.f").to_string().as_bytes().to_vec(),
        Value::Uuid(u) => u.to_string().as_bytes().to_vec(),
        Value::Json(j) => j.to_string().as_bytes().to_vec(),
        _ => value.to_string().as_bytes().to_vec(),
    }
}

/// Parse PostgreSQL text format parameter to Value
///
/// This handles parameter values sent in text format from the client.
pub fn parse_pg_text_param(data: &[u8], type_oid: i32) -> Result<Value, std::io::Error> {
    let text = std::str::from_utf8(data).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid UTF-8: {}", e))
    })?;

    let value = match type_oid {
        type_oid::BOOL => {
            match text {
                "t" | "true" | "TRUE" | "1" => Value::Boolean(true),
                "f" | "false" | "FALSE" | "0" => Value::Boolean(false),
                _ => return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid boolean value"
                )),
            }
        }
        type_oid::INT2 => {
            let i = text.parse::<i16>().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid int2: {}", e))
            })?;
            Value::Int4(i as i32) // Store as Int4
        }
        type_oid::INT4 => {
            let i = text.parse::<i32>().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid int4: {}", e))
            })?;
            Value::Int4(i)
        }
        type_oid::INT8 => {
            let i = text.parse::<i64>().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid int8: {}", e))
            })?;
            Value::Int8(i)
        }
        type_oid::FLOAT4 => {
            let f = text.parse::<f32>().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid float4: {}", e))
            })?;
            Value::Float4(f)
        }
        type_oid::FLOAT8 => {
            let f = text.parse::<f64>().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid float8: {}", e))
            })?;
            Value::Float8(f)
        }
        type_oid::TEXT | type_oid::VARCHAR => {
            Value::String(text.to_string())
        }
        type_oid::BYTEA => {
            Value::Bytes(data.to_vec())
        }
        type_oid::UUID => {
            let uuid = uuid::Uuid::parse_str(text).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid UUID: {}", e))
            })?;
            Value::Uuid(uuid)
        }
        type_oid::JSON | type_oid::JSONB => {
            // Validate JSON is parseable
            let _json: serde_json::Value = serde_json::from_str(text).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid JSON: {}", e))
            })?;
            // Store as String for bincode compatibility
            Value::Json(text.to_string())
        }
        type_oid::TIMESTAMP | type_oid::TIMESTAMPTZ => {
            let ts = chrono::DateTime::parse_from_rfc3339(text)
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid timestamp: {}", e))
                })?
                .with_timezone(&chrono::Utc);
            Value::Timestamp(ts)
        }
        type_oid::DATE => {
            let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid date: {}", e))
                })?;
            Value::Date(date)
        }
        type_oid::TIME => {
            let time = chrono::NaiveTime::parse_from_str(text, "%H:%M:%S")
                .or_else(|_| chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f"))
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid time: {}", e))
                })?;
            Value::Time(time)
        }
        _ => {
            // Default: treat as text
            Value::String(text.to_string())
        }
    };

    Ok(value)
}

/// Create a RowDescription from Schema
pub fn schema_to_row_description(schema: &Schema) -> Vec<FieldDescription> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let type_oid = datatype_to_oid(&col.data_type);
            let type_size = match &col.data_type {
                DataType::Boolean => 1,
                DataType::Int4 => 4,
                DataType::Int8 => 8,
                DataType::Float4 => 4,
                DataType::Float8 => 8,
                DataType::Int2 => 2,
                DataType::Varchar(n) => n.unwrap_or(0) as i16,
                _ => -1, // Variable length
            };

            FieldDescription {
                name: col.name.clone(),
                table_oid: 0,
                column_attr: (idx + 1) as i16,
                type_oid,
                type_size,
                type_modifier: -1,
                format_code: 0, // Text format
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_authentication_ok() {
        let mut encoder = MessageEncoder::new();
        let msg = BackendMessage::Authentication(AuthenticationMessage::Ok);
        let encoded = encoder.encode(&msg).unwrap();

        assert_eq!(encoded[0], b'R');
        assert_eq!(encoded.len(), 9); // Type + Length(4) + Auth type(4)
    }

    #[test]
    fn test_encode_ready_for_query() {
        let mut encoder = MessageEncoder::new();
        let msg = BackendMessage::ReadyForQuery {
            status: TransactionStatus::Idle,
        };
        let encoded = encoder.encode(&msg).unwrap();

        assert_eq!(encoded[0], b'Z');
        assert_eq!(encoded[encoded.len() - 1], b'I');
    }

    #[test]
    fn test_decode_query() {
        let mut decoder = MessageDecoder::new();

        // Build a query message
        let mut buf = BytesMut::new();
        buf.put_u8(b'Q');
        buf.put_i32(4 + 8 + 1); // length field (4) + "SELECT 1" (8) + null (1)
        buf.put("SELECT 1".as_bytes());
        buf.put_u8(0);

        decoder.buffer_data(&buf);
        let msg = decoder.decode().unwrap();

        match msg {
            Some(FrontendMessage::Query { query }) => {
                assert_eq!(query, "SELECT 1");
            }
            _ => panic!("Expected Query message"),
        }
    }
}
