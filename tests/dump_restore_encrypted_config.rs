//! GH#45: `dump` and `restore` take `-c/--config`, so a data directory written
//! with encryption at rest can be dumped and restored offline; without the
//! option the error names `--config`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "encryption")]

use heliosdb_nano::cli::{DumpCommand, RestoreCommand};
use heliosdb_nano::{Config, EmbeddedDatabase, Tuple, Value};
use std::path::Path;

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn encrypted_config(dir: &Path, var: &str) -> Config {
    std::env::set_var(var, KEY);
    let mut config = Config::default();
    config.storage.memory_only = false;
    config.storage.path = Some(dir.to_path_buf());
    config.encryption.enabled = true;
    config.encryption.key_source = heliosdb_nano::KeySource::Environment(var.to_string());
    config
}

fn write_toml(path: &Path, var: &str) {
    std::fs::write(
        path,
        format!("[encryption]\nenabled = true\n\n[encryption.key_source]\nEnvironment = \"{var}\"\n"),
    )
    .unwrap();
}

fn render(rows: &[Tuple]) -> Vec<String> {
    rows.iter().map(|t| format!("{:?}", t.values)).collect()
}

fn dump_cmd(data: &Path, out: &Path, config: Option<&Path>) -> DumpCommand {
    DumpCommand {
        output: out.to_path_buf(),
        append: false,
        compression: "zstd".into(),
        connection: None,
        verbose: false,
        data_dir: Some(data.to_path_buf()),
        memory: false,
        config: config.map(Path::to_path_buf),
    }
}

fn restore_cmd(input: &Path, target: &Path, config: Option<&Path>) -> RestoreCommand {
    RestoreCommand {
        input: input.to_path_buf(),
        target: Some(target.to_path_buf()),
        verify: false,
        connection: None,
        verbose: false,
        no_validate: false,
        config: config.map(Path::to_path_buf),
    }
}

#[test]
fn dump_and_restore_an_encrypted_directory_with_config() {
    let var = "GH45_DUMP_KEY_ROUNDTRIP";
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let toml = root.path().join("enc.toml");
    let dump = root.path().join("t.heliodump");
    let restored = root.path().join("restored");
    write_toml(&toml, var);

    {
        let db = EmbeddedDatabase::with_config(encrypted_config(&data, var)).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, note TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'plain-statement')").unwrap();
        db.execute_params(
            "INSERT INTO t VALUES ($1, $2)",
            &[Value::Int4(2), Value::String("params-statement".into())],
        )
        .unwrap();
    }

    // Without the config the error names the option.
    let err = dump_cmd(&data, &dump, None).execute().unwrap_err().to_string();
    assert!(err.contains("--config"), "error must name --config: {err}");
    let err = restore_cmd(&dump, &data, None).execute().unwrap_err().to_string();
    assert!(!err.is_empty());

    dump_cmd(&data, &dump, Some(&toml)).execute().expect("dump with config");
    restore_cmd(&dump, &restored, Some(&toml))
        .execute()
        .expect("restore with config");

    let db = EmbeddedDatabase::with_config(encrypted_config(&restored, var)).unwrap();
    let expected = vec![
        "[Int4(1), String(\"plain-statement\")]".to_string(),
        "[Int4(2), String(\"params-statement\")]".to_string(),
    ];
    assert_eq!(
        render(&db.query("SELECT id, note FROM t ORDER BY id", &[]).unwrap()),
        expected
    );
    assert_eq!(
        render(
            &db.query_params("SELECT id, note FROM t WHERE id >= $1 ORDER BY id", &[Value::Int4(1)])
                .unwrap()
        ),
        expected
    );
    drop(db);

    // The restored directory is itself encrypted at rest.
    let err = EmbeddedDatabase::new(&restored)
        .err()
        .expect("must refuse without key")
        .to_string();
    assert!(err.contains("written with encryption enabled"), "{err}");
}

#[test]
fn plaintext_directory_still_dumps_without_config() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let dump = root.path().join("p.heliodump");
    let restored = root.path().join("restored");
    {
        let mut config = Config::default();
        config.storage.memory_only = false;
        config.storage.path = Some(data.clone());
        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (7)").unwrap();
    }
    dump_cmd(&data, &dump, None).execute().unwrap();
    restore_cmd(&dump, &restored, None).execute().unwrap();
    let db = EmbeddedDatabase::new(&restored).unwrap();
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 1);
}
