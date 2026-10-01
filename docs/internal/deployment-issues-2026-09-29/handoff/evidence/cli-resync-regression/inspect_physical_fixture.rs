//! EXTERNAL TEST EXAMPLE: inspect CLOSED owned fixtures through raw read-only
//! RocksDB only. Never constructs StorageEngine/EmbeddedDatabase or a writable
//! StagedPhysicalReplica. The driver verifies a full file census before/after.
use clap::Parser;
use rocksdb::{Options, DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{error::Error, path::PathBuf};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    path: PathBuf,
    #[arg(long)]
    history: Uuid,
    #[arg(long)]
    receiver: bool,
    /// Existing private log directory, outside the inspected database.
    #[arg(long)]
    log_dir: PathBuf,
}

// Mirrors the frozen physical cursor format for independent read-only evidence.
// Changes to the production cursor format require reviewing this fixture helper.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    format: u32,
    history: Uuid,
    snapshot_sequence: u64,
    source_sequence: u64,
    local_sequence: u64,
    last_first_sequence: Option<u64>,
    last_digest: Option<[u8; 32]>,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn ensure(valid: bool, reason: &str) -> Result<(), Box<dyn Error>> {
    if !valid {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, reason).into());
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    ensure(!args.history.is_nil(), "nil history")?;
    let metadata = std::fs::symlink_metadata(&args.path)?;
    ensure(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "fixture must be an existing real directory",
    )?;
    let identity = std::fs::read_to_string(args.path.join("IDENTITY"))?;
    let mut options = Options::default();
    options.create_if_missing(false);
    let log_metadata = std::fs::symlink_metadata(&args.log_dir)?;
    ensure(
        log_metadata.is_dir() && !log_metadata.file_type().is_symlink(),
        "inspector log directory must exist and be nonsymlink",
    )?;
    let db_path = args.path.canonicalize()?;
    let log_path = args.log_dir.canonicalize()?;
    ensure(
        !log_path.starts_with(&db_path) && !db_path.starts_with(&log_path),
        "inspector log directory must be disjoint from database",
    )?;
    options.set_db_log_dir(&log_path);
    let db = DB::open_for_read_only(&options, &args.path, false)?;
    let latest = db.latest_sequence_number();
    let cursor_bytes = db.get(b"__nano_physical_replica_cursor_v1")?;
    let cursor = if args.receiver {
        let bytes = cursor_bytes.ok_or("missing receiver cursor")?;
        ensure(bytes.len() <= 2048, "oversized cursor")?;
        let cursor: Cursor = serde_json::from_slice(&bytes)?;
        let max_sequence = (1_u64 << 56) - 1;
        ensure(
            cursor.format == 1
                && cursor.history == args.history
                && cursor.snapshot_sequence <= cursor.source_sequence
                && cursor.source_sequence <= max_sequence
                && cursor.local_sequence > cursor.source_sequence
                && cursor.local_sequence <= max_sequence
                && cursor.local_sequence == latest,
            "invalid or externally modified receiver cursor",
        )?;
        ensure(
            match (cursor.last_first_sequence, cursor.last_digest) {
                (None, None) => cursor.source_sequence == cursor.snapshot_sequence,
                (Some(first), Some(_)) => first > cursor.snapshot_sequence && first <= cursor.source_sequence,
                _ => false,
            },
            "inconsistent cursor batch identity",
        )?;
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(args.path.join("NANO-PHYSICAL-REPLICA.json"))?)?;
        let manifest_bytes = std::fs::read(args.path.join("NANO-PHYSICAL-MANIFEST.json"))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        let digest: [u8; 32] = Sha256::digest(&manifest_bytes).into();
        ensure(
            marker["history"].as_str() == Some(args.history.to_string().as_str())
                && marker["format"].as_u64() == Some(1)
                && marker["mode"].as_str() == Some("physical-staging-not-serving")
                && marker["storage_identity"].as_str() == Some(identity.trim())
                && manifest["storage_identity"] == marker["storage_identity"]
                && manifest["history"] == marker["history"]
                && manifest["snapshot_id"] == marker["snapshot_id"]
                && manifest["checkpoint_sequence"].as_u64() == Some(cursor.snapshot_sequence)
                && marker["checkpoint_sequence"].as_u64() == Some(cursor.snapshot_sequence)
                && marker["manifest_sha256"] == serde_json::to_value(digest)?,
            "receiver marker/manifest identity mismatch",
        )?;
        Some(cursor)
    } else {
        ensure(cursor_bytes.is_none(), "source unexpectedly contains receiver cursor")?;
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(args.path.join("NANO-PRIMARY-HISTORY.json"))?)?;
        ensure(
            marker["format"].as_u64() == Some(1)
                && marker["history_id"].as_str() == Some(args.history.to_string().as_str())
                && marker["storage_identity"].as_str() == Some(identity.trim()),
            "source history identity mismatch",
        )?;
        None
    };
    let prefix = b"data:resync_cli_probe:";
    let mut records = Vec::new();
    let mut digest = Sha256::new();
    for entry in db.prefix_iterator(prefix) {
        let (key, value) = entry?;
        if !key.starts_with(prefix) {
            break;
        }
        ensure(
            records.len() < 64 && value.len() <= 2 * 1024 * 1024,
            "fixture record limits exceeded",
        )?;
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(&key);
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(&value);
        records.push(serde_json::json!({"key_hex":hex(&key), "value_hex":hex(&value)}));
    }
    ensure(
        db.latest_sequence_number() == latest,
        "fixture changed during read-only inspection",
    )?;
    drop(db);
    println!(
        "{}",
        serde_json::json!({
            "path":args.path,"history":args.history,"latest_sequence":latest,"cursor":cursor,
            "records":records,"record_digest":hex(&digest.finalize()),
            "scope":"closed raw row-prefix/cursor evidence only; no SQL hydration or serving readiness"
        })
    );
    Ok(())
}
