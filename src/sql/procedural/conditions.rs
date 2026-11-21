//! PL/pgSQL condition names and their SQLSTATEs (`RAISE division_by_zero`,
//! `EXCEPTION WHEN unique_violation THEN`).

/// The SQLSTATE of a condition name (lower case), or `None` for a name that
/// is not a known condition.
pub(crate) fn condition_sqlstate(name: &str) -> Option<&'static str> {
    Some(match name {
        "raise_exception" => "P0001",
        "no_data_found" => "P0002",
        "too_many_rows" => "P0003",
        "assert_failure" => "P0004",
        "plpgsql_error" => "P0000",
        "division_by_zero" => "22012",
        "numeric_value_out_of_range" => "22003",
        "invalid_text_representation" => "22P02",
        "string_data_right_truncation" => "22001",
        "null_value_not_allowed" => "22004",
        "datetime_field_overflow" => "22008",
        "invalid_datetime_format" => "22007",
        "data_exception" => "22000",
        "case_not_found" => "20000",
        "cardinality_violation" => "21000",
        "integrity_constraint_violation" => "23000",
        "not_null_violation" => "23502",
        "foreign_key_violation" => "23503",
        "unique_violation" => "23505",
        "check_violation" => "23514",
        "restrict_violation" => "23001",
        "exclusion_violation" => "23P01",
        "invalid_cursor_state" => "24000",
        "invalid_cursor_name" => "34000",
        "duplicate_cursor" => "42P03",
        "in_failed_sql_transaction" => "25P02",
        "no_active_sql_transaction" => "25P01",
        "serialization_failure" => "40001",
        "deadlock_detected" => "40P01",
        "transaction_rollback" => "40000",
        "syntax_error" => "42601",
        "syntax_error_or_access_rule_violation" => "42000",
        "insufficient_privilege" => "42501",
        "undefined_column" => "42703",
        "undefined_table" => "42P01",
        "undefined_function" => "42883",
        "undefined_object" => "42704",
        "duplicate_column" => "42701",
        "duplicate_table" => "42P07",
        "duplicate_object" => "42710",
        "duplicate_function" => "42723",
        "ambiguous_column" => "42702",
        "datatype_mismatch" => "42804",
        "wrong_object_type" => "42809",
        "invalid_parameter_value" => "22023",
        "feature_not_supported" => "0A000",
        "program_limit_exceeded" => "54000",
        "object_not_in_prerequisite_state" => "55000",
        "lock_not_available" => "55P03",
        "query_canceled" => "57014",
        "internal_error" => "XX000",
        _ => return None,
    })
}

/// Does a handler written for `condition` (a SQLSTATE; a code ending in
/// `000` names its whole class) take an error with `sqlstate`?
pub(crate) fn sqlstate_matches(condition: &str, sqlstate: &str) -> bool {
    if condition.eq_ignore_ascii_case(sqlstate) {
        return true;
    }
    match (condition.get(..2), condition.get(2..), sqlstate.get(..2)) {
        (Some(class), Some("000"), Some(actual)) => class.eq_ignore_ascii_case(actual),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_classes() {
        assert_eq!(condition_sqlstate("no_data_found"), Some("P0002"));
        assert_eq!(condition_sqlstate("nonsense"), None);
        assert!(sqlstate_matches("23505", "23505"));
        assert!(sqlstate_matches("23000", "23505"));
        assert!(!sqlstate_matches("23505", "23503"));
        assert!(!sqlstate_matches("22000", "23505"));
    }
}
