//! `pg_proc`: user functions and procedures.
//!
//! The first view that needs more than the storage engine: routines live in
//! the database's routine registry, which a producer reaches through
//! [`DictContext::functions`].

use super::context::DictContext;
use super::oid::{schema_oid, DictOids};
use crate::sql::logical_plan::{FunctionParam, ParamMode};
use crate::sql::phase3::SystemViewRegistry;
use crate::{Result, Tuple, Value};

/// `pg_language.oid` of `internal`, `c` and `sql`: fixed in PostgreSQL.
const INTERNAL_LANGUAGE_OID: i32 = 12;
const C_LANGUAGE_OID: i32 = 13;
const SQL_LANGUAGE_OID: i32 = 14;
/// PostgreSQL assigns `plpgsql` its identifier when a cluster is created, so
/// there is no fixed number to copy; this is this engine's fixed choice.
const PLPGSQL_LANGUAGE_OID: i32 = 13_577;
/// `pg_type.oid` of `void`: what a procedure, or a function declared without a
/// return type, returns.
const VOID_TYPE_OID: i32 = 2278;
/// The bootstrap superuser, the owner every catalog view reports.
const OWNER_OID: i32 = 10;

fn language_oid(language: &str) -> i32 {
    match language.to_ascii_lowercase().as_str() {
        "sql" => SQL_LANGUAGE_OID,
        "plpgsql" => PLPGSQL_LANGUAGE_OID,
        "internal" => INTERNAL_LANGUAGE_OID,
        "c" => C_LANGUAGE_OID,
        _ => 0,
    }
}

/// The input arguments (`IN` and `INOUT`), as PostgreSQL's `proargtypes`
/// prints them: type identifiers separated by one space.
fn input_argument_types(params: &[FunctionParam]) -> (String, i32) {
    let types: Vec<String> = params
        .iter()
        .filter(|param| !matches!(param.mode, ParamMode::Out))
        .map(|param| SystemViewRegistry::get_type_oid(&param.data_type).to_string())
        .collect();
    let count = i32::try_from(types.len()).unwrap_or(i32::MAX);
    (types.join(" "), count)
}

struct Routine {
    kind: &'static str,
    name: String,
    language: String,
    return_type: i32,
    params: Vec<FunctionParam>,
    body: String,
}

/// The rows of `pg_proc`: one per user function (`prokind` `f`) and procedure
/// (`p`), ordered by kind and name. Empty when the context has no routine
/// registry, which is the case only for a storage engine no database owns.
pub(crate) fn pg_proc_rows(ctx: &DictContext<'_>) -> Result<Vec<Tuple>> {
    let Some(registry) = ctx.functions() else {
        return Ok(Vec::new());
    };
    let mut routines = Vec::new();
    for name in registry.list_functions() {
        // A routine dropped between the listing and this read is not a row.
        if let Some(function) = registry.get_function(&name) {
            routines.push(Routine {
                kind: "f",
                name: function.name,
                language: function.language,
                return_type: function
                    .return_type
                    .as_ref()
                    .map_or(VOID_TYPE_OID, SystemViewRegistry::get_type_oid),
                params: function.params,
                body: function.body,
            });
        }
    }
    for name in registry.list_procedures() {
        if let Some(procedure) = registry.get_procedure(&name) {
            routines.push(Routine {
                kind: "p",
                name: procedure.name,
                language: procedure.language,
                return_type: VOID_TYPE_OID,
                params: procedure.params,
                body: procedure.body,
            });
        }
    }
    routines.sort_by_key(|routine| (routine.kind, routine.name.clone()));

    let mut oids = DictOids::empty();
    let namespace = schema_oid("public");
    Ok(routines
        .into_iter()
        .map(|routine| {
            let (argument_types, argument_count) = input_argument_types(&routine.params);
            Tuple::new(vec![
                Value::Int4(oids.claim_routine(routine.kind, &routine.name.to_lowercase())), // oid
                Value::String(routine.name),                                                 // proname
                Value::Int4(namespace),                                                      // pronamespace
                Value::Int4(OWNER_OID),                                                      // proowner
                Value::Int4(language_oid(&routine.language)),                                // prolang
                Value::Int4(routine.return_type),                                            // prorettype
                Value::String(argument_types),                                               // proargtypes
                Value::String(routine.body),                                                 // prosrc
                Value::String(routine.kind.to_string()),                                     // prokind
                Value::Int4(argument_count),                                                 // pronargs
            ])
        })
        .collect())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty
)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;

    #[test]
    fn languages() {
        assert_eq!(language_oid("SQL"), 14);
        assert_eq!(language_oid("plpgsql"), PLPGSQL_LANGUAGE_OID);
        assert_eq!(language_oid("lua"), 0);
    }

    #[test]
    fn a_context_without_a_registry_has_no_routines_and_one_with_it_lists_them() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE FUNCTION rt_f(a INTEGER, b TEXT) RETURNS BIGINT AS $$ SELECT 1 $$ LANGUAGE sql")
            .unwrap();
        assert!(pg_proc_rows(&DictContext::new(&db.storage)).unwrap().is_empty());
        let rows = pg_proc_rows(&DictContext::for_engine(&db.storage)).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0].values;
        assert_eq!(row.len(), 10, "one value per registered column");
        assert_eq!(row[1], Value::String("rt_f".to_string()));
        assert_eq!(row[5], Value::Int4(20));
        assert_eq!(row[6], Value::String("23 25".to_string()));
        assert_eq!(row[8], Value::String("f".to_string()));
        assert_eq!(row[9], Value::Int4(2));
        // The registered schema has the same width.
        let schema = SystemViewRegistry::shared().get_schema("pg_proc").unwrap();
        assert_eq!(schema.columns.len(), row.len());
    }
}
