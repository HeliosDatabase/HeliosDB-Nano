//! Rows of the trigger catalogs: `information_schema.triggers` and
//! `pg_trigger`, built from the stored definitions and the stored record of
//! what each trigger runs. One builder for every route.

use super::logical_plan::{TriggerEvent, TriggerFor, TriggerTiming};
use super::trigger_exec::TriggerExecInfo;
use super::triggers::TriggerDefinition;
use crate::storage::Catalog;
use crate::{Column, DataType, Result, Tuple, Value};
use std::collections::HashMap;

const CATALOG_NAME: &str = "heliosdb";

fn column(name: &str, data_type: DataType) -> Column {
    Column::new(name, data_type)
}

/// The columns of `information_schema.triggers`.
pub fn information_schema_triggers_columns() -> Vec<Column> {
    [
        "trigger_catalog",
        "trigger_schema",
        "trigger_name",
        "event_manipulation",
        "event_object_catalog",
        "event_object_schema",
        "event_object_table",
        "action_order",
        "action_condition",
        "action_statement",
        "action_orientation",
        "action_timing",
        "action_reference_old_table",
        "action_reference_new_table",
        "action_reference_old_row",
        "action_reference_new_row",
        "created",
    ]
    .into_iter()
    .map(|name| {
        column(
            name,
            if name == "action_order" {
                DataType::Int4
            } else {
                DataType::Text
            },
        )
    })
    .collect()
}

/// The columns of `pg_trigger`.
pub fn pg_trigger_columns() -> Vec<Column> {
    vec![
        column("oid", DataType::Int4),
        column("tgrelid", DataType::Int4),
        column("tgname", DataType::Text),
        column("tgfoid", DataType::Int4),
        column("tgtype", DataType::Int2),
        column("tgenabled", DataType::Text),
        column("tgisinternal", DataType::Boolean),
        column("tgnargs", DataType::Int2),
        column("tgargs", DataType::Text),
        column("tgqual", DataType::Text),
        column("tgrelname", DataType::Text),
        column("tgfname", DataType::Text),
    ]
}

/// Every trigger with what it runs (`None` for a trigger created by a build
/// that did not record it), in table and name order.
fn triggers(catalog: &Catalog<'_>) -> Result<Vec<(TriggerDefinition, Option<TriggerExecInfo>)>> {
    let mut exec: HashMap<(String, String), TriggerExecInfo> = catalog
        .load_all_trigger_exec()?
        .into_iter()
        .map(|(table, trigger, info)| ((table, trigger), info))
        .collect();
    let mut out: Vec<(TriggerDefinition, Option<TriggerExecInfo>)> = catalog
        .load_all_triggers()?
        .into_iter()
        .map(|definition| {
            let info = exec.remove(&(definition.table_name.clone(), definition.name.clone()));
            (definition, info)
        })
        .collect();
    out.sort_by(|a, b| (&a.0.table_name, &a.0.name).cmp(&(&b.0.table_name, &b.0.name)));
    Ok(out)
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `EXECUTE FUNCTION f('a', 'b')`, or the inline body as it was written.
fn action_statement(info: Option<&TriggerExecInfo>) -> Value {
    match info {
        Some(TriggerExecInfo {
            inline: Some(inline), ..
        }) => Value::String(inline.source.clone()),
        Some(TriggerExecInfo {
            function: Some(function),
            args,
            ..
        }) => Value::String(format!(
            "EXECUTE FUNCTION {function}({})",
            args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(", ")
        )),
        _ => Value::Null,
    }
}

fn condition(definition: &TriggerDefinition) -> Value {
    match &definition.when_condition {
        None => Value::Null,
        Some(expr) => {
            let rendered = serde_json::to_string(expr.as_ref())
                .ok()
                .and_then(|json| super::logical_plan::default_expr_json_to_sql(&json));
            Value::String(rendered.unwrap_or_else(|| format!("{expr:?}")))
        }
    }
}

fn timing(definition: &TriggerDefinition) -> &'static str {
    match definition.timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    }
}

fn split(table: &str) -> (String, String) {
    super::Planner::split_schema_key(table)
}

/// A stable positive number for a name (the catalogs here have no object ids).
fn name_oid(text: &str) -> i32 {
    let hash = text
        .bytes()
        .fold(2_166_136_261_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16_777_619));
    i32::try_from(hash % 1_000_000_000).unwrap_or(0) + 16_384
}

/// `information_schema.triggers`: one row per trigger and event.
pub fn information_schema_triggers_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    let all = triggers(catalog)?;
    let mut rows = Vec::new();
    for (definition, info) in &all {
        let (schema, table) = split(&definition.table_name);
        // Position among the table's triggers of the same timing and level,
        // which is the order they fire in.
        let order = all
            .iter()
            .filter(|(other, _)| {
                other.table_name == definition.table_name
                    && other.timing == definition.timing
                    && other.for_each == definition.for_each
                    && other.name <= definition.name
            })
            .count();
        for event in &definition.events {
            let manipulation = match event {
                TriggerEvent::Insert => "INSERT",
                TriggerEvent::Update(_) => "UPDATE",
                TriggerEvent::Delete => "DELETE",
                TriggerEvent::Truncate => "TRUNCATE",
            };
            rows.push(Tuple::new(vec![
                Value::String(CATALOG_NAME.into()),
                Value::String(schema.clone()),
                Value::String(definition.name.clone()),
                Value::String(manipulation.into()),
                Value::String(CATALOG_NAME.into()),
                Value::String(schema.clone()),
                Value::String(table.clone()),
                Value::Int4(i32::try_from(order).unwrap_or(i32::MAX)),
                condition(definition),
                action_statement(info.as_ref()),
                Value::String(
                    if definition.for_each == TriggerFor::Row {
                        "ROW"
                    } else {
                        "STATEMENT"
                    }
                    .into(),
                ),
                Value::String(timing(definition).into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::String(definition.created_at.to_string()),
            ]));
        }
    }
    Ok(rows)
}

/// `pg_trigger`: one row per trigger. `tgtype` uses PostgreSQL's bits (row 1,
/// before 2, insert 4, delete 8, update 16, truncate 32, instead 64);
/// `tgenabled` is `O` (fires) or `D` (disabled). `tgrelname` and `tgfname`
/// name the table and the function directly.
pub fn pg_trigger_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    let mut rows = Vec::new();
    for (definition, info) in triggers(catalog)? {
        let mut tgtype = 0_i16;
        if definition.for_each == TriggerFor::Row {
            tgtype |= 1;
        }
        match definition.timing {
            TriggerTiming::Before => tgtype |= 2,
            TriggerTiming::InsteadOf => tgtype |= 64,
            TriggerTiming::After => {}
        }
        for event in &definition.events {
            tgtype |= match event {
                TriggerEvent::Insert => 4,
                TriggerEvent::Delete => 8,
                TriggerEvent::Update(_) => 16,
                TriggerEvent::Truncate => 32,
            };
        }
        let function = info.as_ref().and_then(|i| i.function.clone());
        let args = info.as_ref().map(|i| i.args.clone()).unwrap_or_default();
        rows.push(Tuple::new(vec![
            Value::Int4(name_oid(&format!("{}:{}", definition.table_name, definition.name))),
            Value::Int4(name_oid(&definition.table_name)),
            Value::String(definition.name.clone()),
            function.as_deref().map_or(Value::Int4(0), |f| Value::Int4(name_oid(f))),
            Value::Int2(tgtype),
            Value::String(if definition.enabled { "O" } else { "D" }.into()),
            Value::Boolean(false),
            Value::Int2(i16::try_from(args.len()).unwrap_or(i16::MAX)),
            Value::String(args.join("\u{0}")),
            condition(&definition),
            Value::String(split(&definition.table_name).1),
            function.map_or(Value::Null, Value::String),
        ]));
    }
    Ok(rows)
}
