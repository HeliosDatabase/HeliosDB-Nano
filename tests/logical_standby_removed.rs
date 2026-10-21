//! Spec P2.1 §3: the logical standby path and the native v3 replication
//! channel are REMOVED, with no compatibility shim — nothing in the code, the
//! tests, the fixture, the shipped example configuration, the guides or the
//! agent skills may still name them.
//!
//! A source scan, like `tests/write_gate_funnel.rs`: a reintroduced symbol (a
//! revert, a copy-pasted test, a guide that still documents the old flag) is a
//! test failure that names the file and line. Plans and internal reports
//! (`docs/plans/`, `docs/internal/`) and the CHANGELOG are history and are not
//! scanned; neither is `tests/docker/`, the multi-container lab that is out of
//! service until remote replication (spec P2.2).
//!
//! The needles are assembled from pieces so this file does not match itself.

use std::path::{Path, PathBuf};

/// What P2.1 removed, and why each must stay gone.
fn removed() -> Vec<(String, &'static str)> {
    vec![
        (
            concat!("WalApp", "licator").to_string(),
            "the logical standby's WAL applicator",
        ),
        (
            concat!("StorageCatchup", "Source").to_string(),
            "the logical WAL as a standby's catch-up source",
        ),
        (
            concat!("FULL_RESYNC_", "REQUIRED_MESSAGE").to_string(),
            "the native handshake's refusal text (now a typed Refuse code)",
        ),
        (
            concat!("HELIOSDB_PRIMARY_", "PG_PORT").to_string(),
            "the standby's write forwarding to the primary's PostgreSQL port",
        ),
        (
            concat!("physical-replication-", "listen").to_string(),
            "the separate physical endpoint flag (the replication port is the one port)",
        ),
        (
            concat!("physical_replication_", "listen").to_string(),
            "the separate physical endpoint argument",
        ),
        (
            concat!("Streaming", "Server").to_string(),
            "the native v3 replication listener",
        ),
        (
            concat!("Streaming", "Client").to_string(),
            "the native v3 standby client",
        ),
        (
            concat!("query_", "forwarder").to_string(),
            "transparent write forwarding from a standby",
        ),
        (
            concat!("observe_native_", "handshake").to_string(),
            "the posture's view of the native channel",
        ),
    ]
}

/// The modules P2.1 deleted.
const DELETED_MODULES: [&str; 10] = [
    "streaming",
    "transport",
    "wal_applicator",
    "wal_store",
    "wal_replicator",
    "logical_replication",
    "lsn_manager",
    "failover_watcher",
    "split_brain",
    "query_forwarder",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scanned(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("rs" | "md" | "sh" | "toml" | "py")
    )
}

fn walk(dir: &Path, skip: &[PathBuf], files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if skip.iter().any(|skipped| path.starts_with(skipped)) {
            continue;
        }
        if path.is_dir() {
            walk(&path, skip, files);
        } else if scanned(&path) {
            files.push(path);
        }
    }
}

#[test]
fn no_removed_replication_symbol_is_named_anywhere_that_ships() {
    let root = root();
    let this = root.join("tests").join("logical_standby_removed.rs");
    let skip = vec![
        this.clone(),
        root.join("tests").join("docker"),
        root.join("tests").join("protocol_tests").join("venv"),
    ];
    let mut files = Vec::new();
    for dir in ["src", "tests", "scripts/regress", "docs/guides", ".claude/skills"] {
        walk(&root.join(dir), &skip, &mut files);
    }
    for file in ["README.md", "config.example.toml", "AGENTS.md"] {
        let path = root.join(file);
        if path.is_file() {
            files.push(path);
        }
    }
    assert!(
        files.iter().any(|file| file.ends_with("src/main.rs")) && files.len() > 100,
        "the scan reached the sources ({} files)",
        files.len()
    );
    let mut hits = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for (needle, what) in removed() {
                if line.contains(&needle) {
                    hits.push(format!(
                        "{}:{}: {needle} ({what})",
                        file.strip_prefix(&root).unwrap_or(file).display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "removed replication symbols are back:\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_removed_modules_are_gone_and_undeclared() {
    let replication = root().join("src").join("replication");
    let declarations = std::fs::read_to_string(replication.join("mod.rs")).expect("replication/mod.rs");
    for module in DELETED_MODULES {
        assert!(
            !replication.join(format!("{module}.rs")).exists(),
            "src/replication/{module}.rs is back"
        );
        assert!(
            !declarations.contains(&format!("mod {module};")),
            "replication/mod.rs declares {module} again"
        );
    }
    for test in [
        "replication_catchup_transport",
        "replication_history_transport",
        "replication_listener_startup",
        "native_replication_auth",
        "wal_applicator_fail_closed",
        "wal_segment_recovery_tests",
    ] {
        assert!(
            !root().join("tests").join(format!("{test}.rs")).exists(),
            "tests/{test}.rs (native channel only) is back"
        );
    }
    assert!(
        !root().join("tests").join("ha_tests").exists(),
        "tests/ha_tests (the native v3 suite) is back; ha_integration runs the physical pair"
    );
}
