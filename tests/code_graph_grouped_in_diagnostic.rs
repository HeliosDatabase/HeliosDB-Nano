//! Source-bound diagnostic for the resolver's transaction-owned grouped UPDATE.
//! Timings are observations, not a release performance threshold or substitute
//! for the unchanged 443-file field benchmark.
#![cfg(feature = "code-graph")]

use heliosdb_nano::{code_graph::CodeIndexOptions, EmbeddedDatabase, Error, Result, Tuple, Value};
use std::time::Instant;

const TOUCHED: usize = 256;
const SEED_BATCH: usize = 256;
const REFS: &str = "_hdb_code_symbol_refs";

fn integer(value: Option<&Value>) -> Result<i64> {
    match value {
        Some(Value::Int4(n)) => Ok(i64::from(*n)),
        Some(Value::Int8(n)) => Ok(*n),
        other => Err(Error::storage(format!("diagnostic expected integer, got {other:?}"))),
    }
}

fn image(db: &EmbeddedDatabase) -> Result<Vec<Tuple>> {
    db.query(
        "SELECT edge_id, file_id, from_symbol, to_symbol, to_name, kind, line, resolution \
         FROM _hdb_code_symbol_refs ORDER BY edge_id",
        &[],
    )
}

fn fixture(rows: usize) -> Result<EmbeddedDatabase> {
    // Use the same public default in-memory constructor as the field test.
    // The real indexer's empty source installs its actual graph/FK schema.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE cg_diag_src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    assert_eq!(
        db.code_index(CodeIndexOptions::for_table("cg_diag_src"))?.files_parsed,
        0
    );
    db.begin()?;
    db.execute("INSERT INTO _hdb_code_files (node_id, source_table, path, lang, sha256) VALUES (1, 'cg_diag_src', 'fixture.rs', 'rust', 'diagnostic')")?;
    for id in 1..=64 {
        db.execute(&format!(
            "INSERT INTO _hdb_code_symbols (node_id, file_id, name, qualified, kind) \
             VALUES ({id}, 1, 'target_{id}', 'target_{id}', 'function')"
        ))?;
    }
    for first in (1..=rows).step_by(SEED_BATCH) {
        let last = rows.min(first + SEED_BATCH - 1);
        let mut params = Vec::with_capacity((last - first + 1) * 2);
        let mut values = Vec::with_capacity(last - first + 1);
        for id in first..=last {
            let base = params.len();
            params.push(Value::Int8(
                i64::try_from(id).map_err(|_| Error::storage("row id overflow"))?,
            ));
            params.push(Value::String(format!("target_{}", (id - 1) % 64 + 1)));
            values.push(format!(
                "(${}, 1, 1, NULL, ${}, 'call', 1, 'unresolved')",
                base + 1,
                base + 2
            ));
        }
        let (affected, returned) = db.execute_params_returning(
            &format!(
                "INSERT INTO {REFS} (edge_id, file_id, from_symbol, to_symbol, to_name, kind, line, resolution) \
                 VALUES {} RETURNING edge_id",
                values.join(",")
            ),
            &params,
        )?;
        assert_eq!(
            affected,
            u64::try_from(last - first + 1).map_err(|_| Error::storage("batch overflow"))?
        );
        assert_eq!(returned.len(), last - first + 1);
        for (offset, row) in returned.iter().enumerate() {
            assert_eq!(row.values.len(), 1);
            assert_eq!(
                integer(row.values.first())?,
                i64::try_from(first + offset).map_err(|_| Error::storage("id overflow"))?
            );
        }
    }
    db.commit()?;
    assert!(!db.in_transaction());
    let seeded = image(&db)?;
    assert_eq!(seeded.len(), rows);
    check_image(&seeded, rows, None)?;
    Ok(db)
}

fn check_image(rows: &[Tuple], total: usize, groups: Option<usize>) -> Result<()> {
    assert_eq!(rows.len(), total);
    for (offset, row) in rows.iter().enumerate() {
        let id = offset + 1;
        assert_eq!(row.values.len(), 8);
        assert_eq!(
            integer(row.values.first())?,
            i64::try_from(id).map_err(|_| Error::storage("id overflow"))?
        );
        assert_eq!(integer(row.values.get(1))?, 1);
        assert_eq!(integer(row.values.get(2))?, 1);
        assert_eq!(
            row.values.get(4),
            Some(&Value::String(format!("target_{}", (id - 1) % 64 + 1)))
        );
        assert_eq!(row.values.get(5), Some(&Value::String("call".into())));
        assert_eq!(integer(row.values.get(6))?, 1);
        if let Some(groups) = groups.filter(|_| id <= TOUCHED) {
            assert_eq!(
                integer(row.values.get(3))?,
                i64::try_from((id - 1) % groups + 1).map_err(|_| Error::storage("target overflow"))?
            );
            assert_eq!(row.values.get(7), Some(&Value::String("exact".into())));
        } else {
            assert_eq!(row.values.get(3), Some(&Value::Null));
            assert_eq!(row.values.get(7), Some(&Value::String("unresolved".into())));
        }
    }
    Ok(())
}

#[test]
#[ignore = "source-bound native scaling diagnostic; original field benchmark remains mandatory"]
fn grouped_pk_in_transaction_update_scaling() -> Result<()> {
    for rows in [4_096, 16_384] {
        for groups in [1, 16, 64] {
            for commit in [false, true] {
                let setup = Instant::now();
                let db = fixture(rows)?;
                let before = image(&db)?;
                let setup_ns = setup.elapsed().as_nanos();
                db.begin()?;
                let started = Instant::now();
                let mut affected_total = 0;
                let mut group_ns = Vec::with_capacity(groups);
                for group in 0..groups {
                    let ids: Vec<usize> = (1..=TOUCHED).filter(|id| (id - 1) % groups == group).collect();
                    let csv = ids.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
                    // This is the resolver's exact text UPDATE shape. No raw
                    // row API, transaction detach, or disabled constraint.
                    let sql = format!(
                        "UPDATE {REFS} SET to_symbol = {}, resolution = 'exact' WHERE edge_id IN ({csv})",
                        group + 1
                    );
                    let group_started = Instant::now();
                    let affected = db.execute(&sql)?;
                    let elapsed_ns = group_started.elapsed().as_nanos();
                    assert_eq!(
                        affected,
                        u64::try_from(ids.len()).map_err(|_| Error::storage("count overflow"))?
                    );
                    assert!(db.in_transaction(), "group UPDATE consumed the caller owner");
                    affected_total += affected;
                    group_ns.push(elapsed_ns);
                    eprintln!("codegraph-grouped-in-operation rows={rows} groups={groups} commit={commit} ordinal={group} selected={} affected={affected} elapsed_ns={elapsed_ns}", ids.len());
                }
                let updates_ns = started.elapsed().as_nanos();
                assert_eq!(
                    affected_total,
                    u64::try_from(TOUCHED).map_err(|_| Error::storage("count overflow"))?
                );
                let staged = image(&db)?;
                check_image(&staged, rows, Some(groups))?;
                let settle = Instant::now();
                if commit {
                    db.commit()?;
                } else {
                    db.rollback()?;
                }
                let settle_ns = settle.elapsed().as_nanos();
                assert!(!db.in_transaction());
                let after = image(&db)?;
                if commit {
                    assert_eq!(after, staged);
                    check_image(&after, rows, Some(groups))?;
                } else {
                    assert_eq!(after, before, "caller rollback lost or leaked graph rows");
                    check_image(&after, rows, None)?;
                }
                eprintln!("codegraph-grouped-in-cell rows={rows} groups={groups} touched={TOUCHED} commit={commit} setup_ns={setup_ns} updates_ns={updates_ns} settle_ns={settle_ns} group_ns={group_ns:?}");
            }
        }
    }
    Ok(())
}
