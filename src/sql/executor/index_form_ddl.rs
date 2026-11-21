//! `CREATE INDEX` for the index forms beyond one plain column or a plain UNIQUE
//! key: an ordinary multi-column index, a partial index (`… WHERE predicate`)
//! and an expression index (`… (lower(email))`), UNIQUE or not.
//!
//! One statement, one write group: the tree is registered hidden, filled from
//! the committed rows, and revealed when its definition is written. A UNIQUE
//! index whose rows already repeat a key is not created (23505).

use std::sync::Arc;

use super::{Executor, PhysicalOperator};
use crate::sql::logical_plan::IndexOption;
use crate::storage::index_form::{is_tree_definition, IndexForm, IndexShape};
use crate::{Error, Result};

/// Build the index a `CREATE INDEX` plan describes when it is one of this
/// module's forms. `Ok(None)`: it is not (one plain column, a plain UNIQUE key,
/// a vector index, an index that is only recorded), and the caller builds it.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_create(
    executor: &Executor,
    storage: &crate::storage::StorageEngine,
    name: &str,
    table_name: &str,
    column_name: &str,
    index_type: Option<&str>,
    options: &[IndexOption],
    if_not_exists: bool,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    if !is_tree_definition(index_type, options) {
        return Ok(None);
    }
    let shape = IndexShape::of(column_name, index_type, options);
    if !shape.needs_form() && (shape.unique || shape.parts.len() == 1) {
        return Ok(None);
    }
    if storage.is_branch_active() {
        return Err(Error::query_execution(
            "CREATE INDEX for ART secondary indexes must run on the main branch",
        ));
    }

    let art = storage.art_indexes();
    let catalog = storage.catalog();
    let taken = art.index_exists(name)
        || storage.vector_indexes().index_exists(name)
        || catalog.index_definition_exists(name)?;
    if taken {
        if if_not_exists {
            return Ok(Some(super::ddl::empty_ddl_result(executor)));
        }
        return Err(Error::query_execution(format!("Index '{}' already exists", name)));
    }

    let schema = catalog.get_table_schema(table_name)?;
    let form = if shape.needs_form() {
        Some(Arc::new(IndexForm::compile(&catalog, table_name, &schema, &shape)?))
    } else {
        for column in &shape.parts {
            if schema.get_column(column).is_none() {
                return Err(Error::query_execution(format!(
                    "Column '{}' not found in table '{}'",
                    column, table_name
                )));
            }
        }
        None
    };

    // The rows the index is built from. An expression that cannot be evaluated
    // for one of them fails the statement before anything is registered.
    let tuples = storage.scan_table_with_schema(table_name, &schema)?;
    if let Some(form) = &form {
        for tuple in &tuples {
            form.strict_key_for_tuple(&schema, tuple).map_err(|e| {
                Error::query_execution(format!(
                    "could not create index \"{}\": evaluating its key for an existing row failed: {}",
                    name, e
                ))
            })?;
        }
    }

    // Spec R5.3: the definition and its WAL entry are ONE group; the index is
    // hidden until it is written, and goes again if it is not.
    let mut ddl = crate::storage::DdlBatch::new(storage);
    art.hide_index(name);
    {
        let revealed = Arc::clone(art);
        let hidden = name.to_string();
        ddl.on_commit(move || revealed.reveal_index(&hidden));
        let revealed = Arc::clone(art);
        let hidden = name.to_string();
        ddl.on_abort(move || revealed.reveal_index(&hidden));
    }
    let registered = match &form {
        Some(form) => art.create_form_index(name, table_name, shape.unique, Arc::clone(form)),
        None => art.create_manual_index(name, table_name, &shape.parts),
    };
    registered.map_err(|e| Error::query_execution(format!("Failed to create index: {}", e)))?;
    super::ddl::drop_index_unless_written(art, name, &mut ddl);

    let backfilled = if shape.unique {
        art.backfill_unique_index(name, &schema, &tuples).map_err(|e| {
            let _ = art.drop_index(name);
            Error::constraint_violation(format!("could not create unique index \"{}\": {}", name, e))
        })?
    } else {
        art.backfill_manual_index(name, &schema, &tuples).map_err(|e| {
            let _ = art.drop_index(name);
            Error::query_execution(format!("Failed to backfill index: {}", e))
        })?
    };
    tracing::info!(
        "Created {}index '{}' on table '{}' ({}){} with {} of {} existing rows",
        if shape.unique { "UNIQUE " } else { "" },
        name,
        table_name,
        shape.parts.join(", "),
        shape
            .predicate
            .as_ref()
            .map(|p| format!(" WHERE {p}"))
            .unwrap_or_default(),
        backfilled,
        tuples.len()
    );

    // The tag says how the definition reads back: an expression index keeps its
    // key as text, every other tree index is an `art` definition.
    let tag = if shape.expression { "expression" } else { "art" };
    super::ddl::stage_index_definition(storage, name, table_name, column_name, Some(tag), options, &mut ddl)?;
    ddl.commit()?.wait_replicated();
    Ok(Some(super::ddl::empty_ddl_result(executor)))
}
