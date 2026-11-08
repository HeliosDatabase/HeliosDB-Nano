//! One encoded physical group ceiling for every point put/delete owner.
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::EmbeddedDatabase;

#[derive(Clone, Copy, Debug)]
enum Op {
    Put,
    Delete,
}
impl Op {
    fn batch(self, key: &[u8], value: &[u8]) -> WriteBatch {
        let mut batch = WriteBatch::default();
        match self {
            Self::Put => batch.put(key, value),
            Self::Delete => batch.delete(key),
        }
        batch
    }
    fn run(self, gate: &WriteGate, db: &DB, key: &[u8], value: &[u8], opts: Option<&WriteOptions>) -> Result<()> {
        match self {
            Self::Put => gate.put(db, key, value, opts),
            Self::Delete => gate.delete(db, key, opts),
        }
    }
}
fn scratch() -> (tempfile::TempDir, DB) {
    let dir = tempfile::tempdir().unwrap();
    let mut options = rocksdb::Options::default();
    options.create_if_missing(true);
    let db = DB::open(&options, dir.path()).unwrap();
    (dir, db)
}
fn image(db: &DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    db.iterator_opt(rocksdb::IteratorMode::Start, read)
        .map(|record| {
            let (key, value) = record.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}
fn cap(error: Error) {
    assert!(
        matches!(error, Error::QueryExecution(_)),
        "typed prewrite group ceiling: {error}"
    );
    assert!(
        error
            .to_string()
            .contains(crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
        "exact prewrite cap: {error}"
    );
}
fn one_group_bytes(db: &DB, before: u64) -> usize {
    let mut groups = db.get_updates_since(before).unwrap();
    let (start, batch) = groups.next().unwrap().unwrap();
    assert_eq!(start, before + 1);
    let bytes = batch.data().len();
    assert!(!batch.is_empty());
    assert_eq!(
        start + u64::try_from(batch.len()).unwrap() - 1,
        db.latest_sequence_number()
    );
    assert!(groups.next().is_none(), "exactly one native group");
    groups.status().unwrap();
    bytes
}

#[test]
fn point_put_delete_obey_the_actual_encoder_ceiling_for_keyclasses_and_options() {
    let classes: &[&[u8]] = &[
        b"meta:seqstate:",
        b"counter:",
        b"dict:cap_probe:",
        b"meta:table:",
        b"__vidx:999:vec:",
        b"scn_map:@s",
    ];
    for prefix in classes {
        for op in [Op::Put, Op::Delete] {
            for sync in [false, true] {
                for boundary in [127, 128] {
                    let (_dir, db) = scratch();
                    let gate = WriteGate::open();
                    let mut key = prefix.to_vec();
                    key.resize(boundary, b'x');
                    let value = vec![7; boundary];
                    if matches!(op, Op::Delete) {
                        gate.put(&db, &key, b"old", None).unwrap();
                    }
                    let encoded = op.batch(&key, &value).size_in_bytes();
                    let before = image(&db);
                    let sequence = db.latest_sequence_number();
                    let mut options = WriteOptions::default();
                    options.set_sync(true);
                    let opts = sync.then_some(&options);
                    for ceiling in [encoded - 1, 0] {
                        gate.debug_set_write_group_ceiling(Some(ceiling));
                        cap(op.run(&gate, &db, &key, &value, opts).unwrap_err());
                        assert_eq!(image(&db), before);
                        assert_eq!(db.latest_sequence_number(), sequence);
                        assert!(gate.wait_for_writes(Duration::ZERO));
                    }
                    assert_eq!(gate.refusals(), 2);
                    gate.debug_set_write_group_ceiling(Some(encoded));
                    op.run(&gate, &db, &key, &value, opts).unwrap();
                    assert_eq!(db.latest_sequence_number(), sequence + 1);
                    assert_eq!(
                        one_group_bytes(&db, sequence),
                        encoded,
                        "actual native encoder boundary"
                    );
                    assert_eq!(db.get(&key).unwrap(), matches!(op, Op::Put).then_some(value));
                    assert_eq!(gate.refusals(), 2);
                    assert!(gate.wait_for_writes(Duration::ZERO));
                    eprintln!(
                        "POINT_CAP_ENCODER_CELL {op:?} prefix={} sync={sync} boundary={boundary}",
                        String::from_utf8_lossy(prefix)
                    );
                }
            }
        }
    }
}

#[test]
fn put_cap_refusal_preserves_the_next_actual_native_fault_and_retry() {
    for after in [false, true] {
        for sync in [false, true] {
            let (_dir, db) = scratch();
            let gate = WriteGate::open();
            let key = b"meta:seqstate:native_cap";
            let value = [9; 128];
            let before = image(&db);
            let sequence = db.latest_sequence_number();
            let mut options = WriteOptions::default();
            options.set_sync(true);
            let opts = sync.then_some(&options);
            if after {
                gate.debug_fail_next_native_write_after_apply();
            } else {
                gate.debug_fail_next_native_write_before_apply();
            }
            gate.debug_set_write_group_ceiling(Some(0));
            cap(gate.put(&db, key, value, opts).unwrap_err());
            assert_eq!(image(&db), before);
            assert_eq!(db.latest_sequence_number(), sequence);
            gate.debug_set_write_group_ceiling(None);
            let error = gate.put(&db, key, value, opts).unwrap_err();
            let marker = if after {
                "injected native write error after apply; outcome is unknown"
            } else {
                "injected native write error before apply; outcome is unknown"
            };
            assert!(
                error.to_string().contains(marker),
                "native flag retained through prewrite cap: {error}"
            );
            assert_eq!(db.get(key).unwrap(), after.then_some(value.to_vec()));
            assert_eq!(db.latest_sequence_number(), sequence + u64::from(after));
            if after {
                assert_eq!(
                    one_group_bytes(&db, sequence),
                    Op::Put.batch(key, &value).size_in_bytes()
                );
            }
            gate.put(&db, b"proof:native_retry", b"ok", opts).unwrap();
            assert_eq!(gate.refusals(), 1);
            assert!(gate.wait_for_writes(Duration::ZERO));
            eprintln!("POINT_CAP_NATIVE_CELL after={after} sync={sync}");
        }
    }
}

#[test]
fn point_cap_preserves_role_precedence_and_closes_layout_changes_on_refusal() {
    for fenced in [false, true] {
        for op in [Op::Put, Op::Delete] {
            let (_dir, db) = scratch();
            WriteGate::open().put(&db, b"proof:role", b"old", None).unwrap();
            let gate = if fenced {
                WriteGate::open()
            } else {
                WriteGate::closed("physical snapshot")
            };
            if fenced {
                gate.fence(format!(
                    "{}: cap role proof",
                    crate::error::PRIMARY_FENCED_WRITE_REFUSED
                ));
            }
            gate.debug_set_write_group_ceiling(Some(0));
            let before = image(&db);
            let sequence = db.latest_sequence_number();
            let error = op
                .run(&gate, &db, b"proof:role", &[1; 128], None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(if fenced {
                    crate::error::PRIMARY_FENCED_WRITE_REFUSED
                } else {
                    "write refused on a physical snapshot"
                }),
                "{error}"
            );
            assert!(!error.contains(crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER));
            assert_eq!(gate.refusals(), 1);
            assert_eq!(image(&db), before);
            assert_eq!(db.latest_sequence_number(), sequence);
            assert!(gate.wait_for_writes(Duration::ZERO));
            eprintln!("POINT_CAP_ROLE_CELL fenced={fenced} {op:?}");
        }
    }
    for op in [Op::Put, Op::Delete] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE point_cap_layout (v INT)").unwrap();
        let raw = db.storage.db();
        let gate = db.storage.write_gate();
        let key = b"meta:table:point_cap_layout".to_vec();
        let value = db.storage.get(&key).unwrap().unwrap();
        let before = image(&raw);
        let sequence = raw.latest_sequence_number();
        gate.debug_set_write_group_ceiling(Some(0));
        cap(op.run(gate, &raw, &key, &value, None).unwrap_err());
        assert_eq!(image(&raw), before);
        assert_eq!(raw.latest_sequence_number(), sequence);
        assert!(!db.storage.layout_epochs().in_flight("point_cap_layout"));
        assert!(gate.wait_for_writes(Duration::ZERO));
        gate.debug_set_write_group_ceiling(None);
        gate.put(&raw, &key, &value, None).unwrap();
        assert!(!db.storage.layout_epochs().in_flight("point_cap_layout"));
        assert_eq!(
            db.storage
                .catalog()
                .get_table_schema("point_cap_layout")
                .unwrap()
                .columns
                .len(),
            1
        );
        eprintln!("POINT_CAP_LAYOUT_CELL {op:?}");
    }
}

#[test]
fn real_b4_inline_copies_are_included_in_the_point_group_ceiling() {
    for history in [false, true] {
        let mut config = crate::Config::default();
        config.storage.memory_only = true;
        config.storage.path = None;
        config.storage.time_travel_enabled = history;
        config.storage.version_gc_interval_secs = Some(0);
        config.storage.wal_checkpoint_interval_entries = 0;
        config.storage.wal_checkpoint_interval_secs = 0;
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE point_cap_rows (id INT PRIMARY KEY, v INT)")
            .unwrap();
        db.execute("INSERT INTO point_cap_rows VALUES (1, 7)").unwrap();
        db.execute("CREATE DATABASE BRANCH cap_child FROM main AS OF NOW")
            .unwrap();
        let table_scope = db.storage.lock_main_table_for_write("point_cap_rows").unwrap();
        let raw = db.storage.db();
        let gate = db.storage.write_gate();
        let rows: Vec<_> = image(&raw)
            .into_iter()
            .filter(|(key, _)| key.starts_with(b"data:point_cap_rows:"))
            .collect();
        assert_eq!(rows.len(), 1);
        let (key, value) = rows.first().unwrap();
        let single = Op::Put.batch(key, value).size_in_bytes();
        let before = image(&raw);
        let sequence = raw.latest_sequence_number();
        let hooks = gate.debug_freeze_runs();
        gate.debug_set_write_group_ceiling(Some(single));
        cap(gate.put(&raw, key, value, None).unwrap_err());
        assert!(
            gate.debug_freeze_runs() > hooks,
            "actual live-child freeze hook must run"
        );
        assert_eq!(image(&raw), before);
        assert_eq!(raw.latest_sequence_number(), sequence);
        assert_eq!(gate.refusals(), 1);
        assert!(gate.wait_for_writes(Duration::ZERO));
        gate.debug_set_write_group_ceiling(None);
        gate.put(&raw, key, value, None).unwrap();
        assert!(
            one_group_bytes(&raw, sequence) > single,
            "owning group includes real B4 frozen copies"
        );
        assert!(
            image(&raw).iter().any(|(key, _)| key.starts_with(b"bdata:")),
            "actual child row copy must exist"
        );
        assert_eq!(raw.get(key).unwrap(), Some(value.clone()));
        drop(table_scope);
        db.execute("USE BRANCH cap_child").unwrap();
        let child = db.query("SELECT v FROM point_cap_rows WHERE id = 1", &[]).unwrap();
        assert_eq!(child.len(), 1);
        assert_eq!(child.first().unwrap().values, vec![crate::Value::Int4(7)]);
        db.execute("USE BRANCH main").unwrap();
        eprintln!("POINT_CAP_B4_CELL tt={history}");
    }
}

struct SwitchingBytes<'a> {
    calls: &'a std::cell::Cell<usize>,
    small_reads: usize,
    small: &'a [u8],
    large: &'a [u8],
}

impl AsRef<[u8]> for SwitchingBytes<'_> {
    fn as_ref(&self) -> &[u8] {
        let call = self.calls.get() + 1;
        self.calls.set(call);
        if call <= self.small_reads {
            self.small
        } else {
            self.large
        }
    }
}

#[test]
fn point_owners_measure_admit_and_write_the_same_single_asref_snapshot() {
    for shape in ["put_key", "put_value", "delete_key"] {
        let (_dir, db) = scratch();
        let gate = WriteGate::open();
        let small_key = b"proof:asref_small";
        let small_value = b"small";
        let large = [b'x'; 512];
        let op = if shape == "delete_key" { Op::Delete } else { Op::Put };
        if matches!(op, Op::Delete) {
            gate.put(&db, small_key, small_value, None).unwrap();
        }
        let calls = std::cell::Cell::new(0);
        // V1 converts key for availability/layout/cap then AGAIN at native,
        // or value for layout/cap then AGAIN at native. All first lookups fit
        // the cap; the final independent conversion would write a larger group.
        let changing = SwitchingBytes {
            calls: &calls,
            small_reads: if shape == "put_value" { 2 } else { 3 },
            small: if shape == "put_value" {
                small_value.as_slice()
            } else {
                small_key.as_slice()
            },
            large: &large,
        };
        let encoded = op.batch(small_key, small_value).size_in_bytes();
        let sequence = db.latest_sequence_number();
        gate.debug_set_write_group_ceiling(Some(encoded));
        match shape {
            "put_key" => gate.put(&db, changing, small_value, None).unwrap(),
            "put_value" => gate.put(&db, small_key, changing, None).unwrap(),
            "delete_key" => gate.delete(&db, changing, None).unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(
            one_group_bytes(&db, sequence),
            encoded,
            "actual native group must fit measured bytes"
        );
        assert_eq!(
            db.get(small_key).unwrap(),
            matches!(op, Op::Put).then_some(small_value.to_vec())
        );
        assert!(
            db.get(large).unwrap().is_none(),
            "never write an independently converted larger key"
        );
        assert_eq!(
            calls.get(),
            1,
            "freeze the generic view once before every admission/measurement/write"
        );
        assert_eq!(gate.refusals(), 0);
        assert!(gate.wait_for_writes(Duration::ZERO));
        eprintln!("POINT_ASREF_SNAPSHOT_CELL {shape}");
    }
}
