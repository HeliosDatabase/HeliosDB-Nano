//! Inspection helper — introspects a `.helios-index` produced by the
//! `code_graph_pilot` example. Dumps sample rows + runs a few
//! additional LSP queries with timings so we can see what's really
//! in the index.

#![cfg(feature = "code-graph")]

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use heliosdb_nano::{
    code_graph::{lsp::CallDirection, DefinitionHint},
    EmbeddedDatabase, Result, Value,
};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let idx = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| ".helios-index".into());
    let data = PathBuf::from(&idx).join("heliosdb-data");
    let db = EmbeddedDatabase::new(data)?;

    println!("=== inspect {idx} ===");

    // Distribution of symbol kinds
    let rows = db.query(
        "SELECT kind, count(*) FROM _hdb_code_symbols GROUP BY kind",
        &[],
    )?;
    println!();
    println!("-- symbols by kind --");
    for row in rows {
        let kind = as_str(row.values.first()).unwrap_or_default();
        let n = as_int(row.values.get(1)).unwrap_or(0);
        println!("  {kind:<10}  {n}");
    }

    // Resolution distribution
    let rows = db.query(
        "SELECT resolution, count(*) FROM _hdb_code_symbol_refs GROUP BY resolution",
        &[],
    )?;
    println!();
    println!("-- refs by resolution --");
    for row in rows {
        let r = as_str(row.values.first()).unwrap_or_default();
        let n = as_int(row.values.get(1)).unwrap_or(0);
        println!("  {r:<12}  {n}");
    }

    // Top 10 most-referenced names
    let rows = db.query(
        "SELECT to_name, count(*) FROM _hdb_code_symbol_refs \
         WHERE to_name IS NOT NULL \
         GROUP BY to_name ORDER BY count(*) DESC LIMIT 10",
        &[],
    )?;
    println!();
    println!("-- top referenced names (by textual target) --");
    for row in rows {
        let n = as_str(row.values.first()).unwrap_or_default();
        let c = as_int(row.values.get(1)).unwrap_or(0);
        println!("  {c:>6}  {n}");
    }

    // Probe specific symbols relevant to the pilot's narrative
    println!();
    println!("-- targeted probes --");
    for name in &["new_in_memory", "execute", "query", "code_index", "lsp_definition"] {
        let t = Instant::now();
        let defs = db.lsp_definition(name, &DefinitionHint::default())?;
        let ms = t.elapsed().as_millis();
        let def = defs
            .first()
            .map(|d| format!("{}@{}", d.path, d.line))
            .unwrap_or_else(|| "<none>".into());
        println!("def {name:<18} {defs_len:>3} hits {ms:>4} ms  {def}",
            defs_len = defs.len());
        if let Some(d) = defs.first() {
            let t = Instant::now();
            let refs = db.lsp_references(d.symbol_id)?;
            let ms = t.elapsed().as_millis();
            println!(
                "    refs(symbol_id={sid:>5})         {nr:>4} refs {ms:>4} ms",
                sid = d.symbol_id,
                nr = refs.len()
            );
            let t = Instant::now();
            let ch = db.lsp_call_hierarchy(d.symbol_id, CallDirection::Incoming, 2)?;
            let ms = t.elapsed().as_millis();
            println!(
                "    call_hierarchy(in, 2)           {n:>4} nodes {ms:>4} ms",
                n = ch.len()
            );
        }
    }

    // Example: refs pointing at methods called `.code_index(` in the source
    let rows = db.query(
        "SELECT kind, count(*) FROM _hdb_code_symbol_refs \
         WHERE to_name LIKE '%code_index%' GROUP BY kind",
        &[],
    )?;
    println!();
    println!("-- raw refs by kind (to_name LIKE '%code_index%') --");
    for row in rows {
        let k = as_str(row.values.first()).unwrap_or_default();
        let n = as_int(row.values.get(1)).unwrap_or(0);
        println!("  {k:<12}  {n}");
    }

    Ok(())
}

fn as_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}
fn as_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int2(n)) => Some(*n as i64),
        Some(Value::Int4(n)) => Some(*n as i64),
        Some(Value::Int8(n)) => Some(*n),
        _ => None,
    }
}
