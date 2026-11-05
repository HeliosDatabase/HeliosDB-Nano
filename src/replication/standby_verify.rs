//! `heliosdb-nano standby verify` (spec R1 §1; plan
//! `PHYSICAL_STANDBY_REPLICATION.md` "Verification" §3): prove that a STOPPED
//! physical standby's replicated keyspace — its whole default column family —
//! is byte-identical to its primary's at one source sequence, or name the key
//! ranges where it is not.
//!
//! # The protocol
//!
//! 1. **The standby, held.** The directory must be an installed physical
//!    standby that no process runs — its RocksDB `LOCK` is asked with
//!    `F_GETLK`, as `promote` asks it — carrying no interrupted fast-forward
//!    and no interrupted promotion. Its stage is reopened exclusively and the
//!    lock is held for the whole verification, so no `start` can race it; its
//!    cursor `S`, the last write group it applied and its epoch are read.
//! 2. **The tail is served.** A resume session after `S`, naming that last
//!    group (NPHYS004 verifies it by content), proves the primary still serves
//!    the standby's next write group — from its WAL or its WAL archive —
//!    before the primary is asked for anything costly. A primary that does
//!    not (WAL pruned, a gap, a group too large, a diverged last group, a
//!    promotion) is refused with the command that recovers the standby: the
//!    comparison needs the standby at the primary's cut, and only the tail a
//!    `start` would apply may bring it there.
//! 3. **The primary's cut.** A fast-forward session (spec P4.1 §3.1) has the
//!    primary pin a checkpoint export at its sequence `P >= S` and send the
//!    leaf plan of that export: per leaf of the default family its first key,
//!    its size and the SHA-256 of its pairs in `physical::range_digest`'s
//!    framing. Nothing else is asked of it: no leaf file is built, nothing is
//!    shipped. (Spec R1 §1: a receiver AT the cut is served such a session
//!    too — an idle primary's cut is the caught-up standby's cursor — when
//!    both sides offer `CAP_FAST_FORWARD_AT_CUT`; a primary that does not is
//!    refused plainly, before anything is asked of it.)
//! 4. **The standby at `P`.** The groups `S+1..=P` are tailed through a resume
//!    session and applied through the stage's `LocalWriter` exactly as a
//!    receiver applies them — the same bytes, the cursor in the same write
//!    group — and nothing past `P`. `P` is the end of a write group: the
//!    export is a checkpoint, and a checkpoint holds whole groups. These are
//!    the writes a `start` would make with the same groups; the standby gets
//!    no other write.
//! 5. **The comparison.** One ordered scan of the standby's default family
//!    digests it against the primary's leaf boundaries
//!    (`fast_forward::local_leaf_digests`). The node-local family is never
//!    read, so the standby's own state (its cursor, its archive catalog) is
//!    excluded by construction. Equal digests in every leaf: identical.
//!    Otherwise each differing leaf is named by its key range, both digests,
//!    and the keys the standby holds in it.
//!
//! The primary is never modified: it serves one resume probe, one
//! fast-forward session and one resume session, as it does for `recover
//! --from-primary`, all with the offline identity (role `Resync`: listed as
//! bootstrapping, never counted for synchronous commits). The export it pins
//! is leased like any other and expires on its own.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rocksdb::{Direction, IteratorMode, ReadOptions, DB};
use tokio::sync::watch;
use uuid::Uuid;

use super::checkpoint_transfer::{
    interrupted_fast_forward, physical_stage_history, ReopenedPhysicalStage, TransferLimits, PHYSICAL_MARKER_FILE,
};
use super::epoch::read_epoch;
use super::fast_forward::local_leaf_digests;
use super::fence::FENCED_FILE;
use super::history::PRIMARY_HISTORY_FILE;
use super::physical::{fast_forward_remedy, Durability, PhysicalBatch};
use super::physical_tls::{ReplicationStream, ReplicationTransport};
use super::physical_wire::{
    valid_leaf_bytes, BatchSink, LastGroup, LeafPlan, PeerRole, PhysicalClient, ReceiverIdentity, Refusal, RefuseCode,
    SourceSessionInfo, TransferToken, WireError, WireLimits, WireSyncMode, LEAF_END,
};
use super::promote::{lock_holder, refuse_promoting_directory};
use super::recover_primary::{format_bytes, format_count};
use crate::Config;

/// The byte limit of the tail's polls: a coalescing target (spec P4.1c) — a
/// larger write group still travels, alone in its poll.
const TAIL_POLL_BYTES: usize = 8 * 1024 * 1024;
/// `TooManySessions` / `SourceNotServing` are retried this many times, with
/// backoff, before the verification gives up.
const REFUSAL_RETRIES: u32 = 5;
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);
/// A poll that brought nothing new is asked again after this long.
const IDLE_POLL_WAIT: Duration = Duration::from_millis(100);
/// The tail gives up when no write group arrived for this many operation
/// deadlines (an idle deadline: each poll that brings groups resets it).
const TAIL_DEADLINES: u32 = 10;
/// At most this many differing leaves are described one by one; every one
/// is counted.
pub const MAX_LISTED_LEAVES: usize = 64;
/// A key is printed up to this many bytes.
const MAX_RENDERED_KEY_BYTES: usize = 96;

/// What [`verify_standby`] verifies, against what.
pub struct VerifyOptions {
    /// The STOPPED standby's data directory.
    pub data_dir: PathBuf,
    /// The node's configuration: the storage options every open of the
    /// standby's store uses.
    pub config: Config,
    /// `--against` as given (the primary's replication port), and its
    /// addresses.
    pub primary_host: String,
    pub sources: Vec<SocketAddr>,
    pub transport: ReplicationTransport,
    pub token: TransferToken,
    pub wire: WireLimits,
    /// The leaf size both sides digest the keyspace in.
    pub leaf_bytes: u32,
}

/// Why a standby was not verified: nothing was compared. `standby verify`
/// exits 2 on either.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A precondition, or the primary's refusal: the standby is as it was.
    #[error("standby verify refused: {0}")]
    Refused(String),
    /// A failure part-way (transport, storage). Groups of the tail applied
    /// before it are durable, as a `start` would have applied them.
    #[error("standby verify failed: {0}")]
    Failed(String),
}

fn refused(reason: impl Into<String>) -> VerifyError {
    VerifyError::Refused(reason.into())
}

fn failed(reason: impl Into<String>) -> VerifyError {
    VerifyError::Failed(reason.into())
}

/// One side of a differing leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafSide {
    pub digest: [u8; 32],
    pub keys: u64,
    pub bytes: u64,
}

/// A leaf whose digests differ: the key range `[start, end)` (`end` is
/// [`LEAF_END`] for the last leaf), what the primary's plan says of it, what
/// the standby holds in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafDifference {
    pub index: u32,
    pub start: Vec<u8>,
    pub end: Vec<u8>,
    pub primary: LeafSide,
    pub standby: LeafSide,
    /// The first and last key the standby holds in the range (`None`: none).
    pub standby_first_key: Option<Vec<u8>>,
    pub standby_last_key: Option<Vec<u8>>,
}

/// What a verification found.
#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub data_dir: PathBuf,
    pub primary_host: String,
    pub history: Uuid,
    /// The standby's source sequence before the tail.
    pub from: u64,
    /// The primary's cut `P`: both sides were compared at it.
    pub sequence: u64,
    /// Write groups of the tail applied to bring the standby to `P`.
    pub tailed_groups: u64,
    pub leaf_bytes: u32,
    pub leaves: u64,
    /// The primary's key and value bytes at `P` (what the leaves cover).
    pub bytes_compared: u64,
    /// How many leaves differ; [`Self::differing`] describes the first
    /// [`MAX_LISTED_LEAVES`] of them.
    pub differing_leaves: u64,
    pub differing: Vec<LeafDifference>,
    pub duration: Duration,
}

impl VerifyReport {
    pub fn identical(&self) -> bool {
        self.differing_leaves == 0
    }

    /// The one-line verdict.
    pub fn summary(&self) -> String {
        if self.identical() {
            format!(
                "standby verify: identical at sequence {} ({} leaves, {} bytes compared; {:.1} s)",
                format_count(self.sequence),
                format_count(self.leaves),
                format_count(self.bytes_compared),
                self.duration.as_secs_f64()
            )
        } else {
            format!(
                "standby verify: DIFFERENT at sequence {}: {} of {} leaves differ ({} bytes on the primary); the \
                 standby is not a copy of its primary — rebuild it with `heliosdb-nano recover --from-primary --full \
                 --replace --data-dir {} --primary-host {}` (the old directory is kept)",
                format_count(self.sequence),
                format_count(self.differing_leaves),
                format_count(self.leaves),
                format_count(self.bytes_compared),
                self.data_dir.display(),
                self.primary_host
            )
        }
    }

    /// One line per described differing leaf, and one more when there are
    /// more than were described.
    pub fn leaf_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .differing
            .iter()
            .map(|leaf| {
                format!(
                    "standby verify: leaf {} differs: keys [{}, {}); primary: {} keys, {}, sha256 {}; standby: {} \
                     keys, {}, first {}, last {}, sha256 {}",
                    leaf.index,
                    render_key(&leaf.start),
                    render_key(&leaf.end),
                    format_count(leaf.primary.keys),
                    format_bytes(leaf.primary.bytes),
                    hex::encode(leaf.primary.digest),
                    format_count(leaf.standby.keys),
                    format_bytes(leaf.standby.bytes),
                    leaf.standby_first_key
                        .as_deref()
                        .map_or_else(|| "-".to_string(), render_key),
                    leaf.standby_last_key
                        .as_deref()
                        .map_or_else(|| "-".to_string(), render_key),
                    hex::encode(leaf.standby.digest)
                )
            })
            .collect();
        let unlisted = self.differing_leaves.saturating_sub(self.differing.len() as u64);
        if unlisted > 0 {
            lines.push(format!(
                "standby verify: … and {} more differing leaves",
                format_count(unlisted)
            ));
        }
        lines
    }

    /// `--json`: the report as one object. Keys are hex; `end` of the last
    /// leaf is the 64 `ff` bytes the leaves end at.
    pub fn to_json(&self) -> serde_json::Value {
        let differing: Vec<serde_json::Value> = self
            .differing
            .iter()
            .map(|leaf| {
                serde_json::json!({
                    "index": leaf.index,
                    "start": hex::encode(&leaf.start),
                    "end": hex::encode(&leaf.end),
                    "primary": {
                        "sha256": hex::encode(leaf.primary.digest),
                        "keys": leaf.primary.keys,
                        "bytes": leaf.primary.bytes,
                    },
                    "standby": {
                        "sha256": hex::encode(leaf.standby.digest),
                        "keys": leaf.standby.keys,
                        "bytes": leaf.standby.bytes,
                        "first_key": leaf.standby_first_key.as_ref().map(hex::encode),
                        "last_key": leaf.standby_last_key.as_ref().map(hex::encode),
                    },
                })
            })
            .collect();
        serde_json::json!({
            "result": if self.identical() { "identical" } else { "different" },
            "data_dir": self.data_dir.display().to_string(),
            "primary": self.primary_host,
            "history": self.history.to_string(),
            "from": self.from,
            "sequence": self.sequence,
            "tailed_groups": self.tailed_groups,
            "leaf_bytes": self.leaf_bytes,
            "leaves": self.leaves,
            "bytes_compared": self.bytes_compared,
            "differing_leaves": self.differing_leaves,
            "differing": differing,
            "duration_secs": self.duration.as_secs_f64(),
        })
    }
}

/// A key as the report prints it: printable ASCII as is (a backslash
/// doubled), every other byte as `\xNN`, cut after 96 bytes; the empty key
/// and [`LEAF_END`] by name.
pub fn render_key(key: &[u8]) -> String {
    if key.is_empty() {
        return "<start of keyspace>".to_string();
    }
    if key == LEAF_END.as_slice() {
        return "<end of keyspace>".to_string();
    }
    let mut text = String::with_capacity(key.len().min(MAX_RENDERED_KEY_BYTES) + 4);
    for &byte in key.iter().take(MAX_RENDERED_KEY_BYTES) {
        if byte.is_ascii_graphic() || byte == b' ' {
            if byte == b'\\' {
                text.push('\\');
            }
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("\\x{byte:02x}"));
        }
    }
    if key.len() > MAX_RENDERED_KEY_BYTES {
        text.push('…');
    }
    text
}

/// The standby, held for the verification: its reopened stage (the only
/// handle, holding RocksDB's lock) and what its cursor says.
struct Held {
    stage: ReopenedPhysicalStage,
    history: Uuid,
    epoch: u64,
    /// The cursor's source sequence when the stage was opened.
    from: u64,
    /// The last write group the cursor applied (spec P5.1 §11.F).
    last: Option<LastGroup>,
}

fn running_refusal(dir: &Path, pid: Option<i32>) -> VerifyError {
    refused(format!(
        "the standby at {} is running{}: stop it first (`heliosdb-nano stop`); `standby verify` works on a stopped \
         standby, whose store it holds while it compares",
        dir.display(),
        pid.map(|pid| format!(" (process {pid} holds its LOCK)"))
            .unwrap_or_default()
    ))
}

/// Step 1 (blocking): classify `dir` and hold its stage.
fn hold_standby(dir: &Path, config: &Config, limits: TransferLimits, primary_host: &str) -> Result<Held, VerifyError> {
    let present = |name: &str| std::fs::symlink_metadata(dir.join(name)).is_ok();
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(refused(format!("{} is not a directory", dir.display()))),
        Err(error) => {
            return Err(refused(format!(
                "{} is not an installed physical standby ({error})",
                dir.display()
            )))
        }
    }
    if let Some(pid) = lock_holder(dir) {
        return Err(running_refusal(dir, Some(pid)));
    }
    if let Some(reason) = refuse_promoting_directory(dir) {
        return Err(refused(reason));
    }
    if present(FENCED_FILE) {
        return Err(refused(format!(
            "{} is a fenced primary's directory ({FENCED_FILE}), not a standby's: there is nothing to verify; \
             rebuild it as a standby with `heliosdb-nano recover --from-primary --replace`",
            dir.display()
        )));
    }
    if !present(PHYSICAL_MARKER_FILE) {
        return Err(refused(if present(PRIMARY_HISTORY_FILE) {
            format!(
                "{} is a primary's directory (it carries {PRIMARY_HISTORY_FILE}), not a standby's: only a physical \
                 standby is verified against its primary",
                dir.display()
            )
        } else {
            format!(
                "{} is not an installed physical standby (no {PHYSICAL_MARKER_FILE})",
                dir.display()
            )
        }));
    }
    let history = physical_stage_history(dir).map_err(|error| {
        refused(format!(
            "the physical standby marker of {} cannot be read: {error}",
            dir.display()
        ))
    })?;
    if let Some(record) = interrupted_fast_forward(dir, config) {
        return Err(refused(format!(
            "{}; nothing was verified",
            fast_forward_remedy(record.from, record.to, &dir.display().to_string(), primary_host)
        )));
    }
    let stage = ReopenedPhysicalStage::open(dir, history, limits, config).map_err(|error| {
        let text = error.to_string();
        if text.contains("lock") || text.contains("Resource temporarily unavailable") {
            running_refusal(dir, None)
        } else {
            // Reported as it is: an I/O error, a file limit or a permission
            // is not a reason to rebuild the standby.
            failed(format!(
                "{} cannot be opened as the physical standby it is: {text}",
                dir.display()
            ))
        }
    })?;
    // Spec P5.1 §11.G: a store that cannot say which epoch it follows does
    // not announce one.
    let epoch = match read_epoch(stage.writer().db()) {
        Ok(epoch) => epoch,
        Err(error) => {
            let _ = stage.close();
            return Err(refused(format!(
                "the replication epoch record of {} cannot be read ({error}): its epoch is unknown, so it is not \
                 compared",
                dir.display()
            )));
        }
    };
    let cursor = stage.cursor();
    Ok(Held {
        stage,
        history,
        epoch,
        from: cursor.source_sequence(),
        last: cursor
            .last_group()
            .map(|(first_sequence, digest)| LastGroup { first_sequence, digest }),
    })
}

/// Close the held stage (the verification's last step); returns its cursor.
fn close_held(held: Arc<Held>) -> Result<u64, VerifyError> {
    let held = Arc::try_unwrap(held).map_err(|_| failed("the standby's store is still shared; it cannot be closed"))?;
    let closed = held
        .stage
        .close()
        .map_err(|error| failed(format!("the standby's store could not be closed cleanly: {error}")))?;
    Ok(closed.cursor.source_sequence())
}

/// Run blocking storage work off the async executor.
async fn blocking<T, F>(job: F) -> Result<T, VerifyError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, VerifyError> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| failed(format!("the verification's storage worker failed: {error}")))?
}

/// The groups of one poll that end at or before the cut; one that starts at
/// or before it but ends past it is noted (it cannot happen: the cut is the
/// end of a group) and refused after the poll.
struct TailSink {
    through: u64,
    batches: Vec<PhysicalBatch>,
    straddling: Option<(u64, u64)>,
}

#[async_trait]
impl BatchSink for TailSink {
    async fn apply(&mut self, batch: PhysicalBatch) -> super::physical_wire::Result<()> {
        if batch.last_sequence() <= self.through {
            self.batches.push(batch);
        } else if batch.first_sequence() <= self.through && self.straddling.is_none() {
            self.straddling = Some((batch.first_sequence(), batch.last_sequence()));
        }
        Ok(())
    }
}

/// Step 4's apply (blocking): `batches`, in order, through the stage's
/// writer — the receiver's own apply, deferred, then one fsync. Returns the
/// cursor reached.
fn apply_tail(held: &Held, batches: &[PhysicalBatch]) -> Result<u64, VerifyError> {
    held.stage
        .verify()
        .map_err(|error| failed(format!("the standby's stage changed under the verification: {error}")))?;
    let writer = held.stage.writer();
    for batch in batches {
        writer.apply_source_with(batch, Durability::Deferred).map_err(|error| {
            failed(format!(
                "applying the primary's write group {}..{} failed: {error}",
                batch.first_sequence(),
                batch.last_sequence()
            ))
        })?;
    }
    writer
        .sync()
        .map_err(|error| failed(format!("the standby's WAL could not be synced: {error}")))
}

/// What the standby holds in one leaf's range.
#[derive(Default)]
struct LeafScan {
    keys: u64,
    bytes: u64,
    first: Option<Vec<u8>>,
    last: Vec<u8>,
}

/// The standby's pairs in `[start, end)` of its default family, counted.
fn scan_leaf(db: &DB, start: &[u8], end: &[u8]) -> Result<LeafScan, rocksdb::Error> {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    read.fill_cache(false);
    read.set_iterate_upper_bound(end.to_vec());
    let mut scan = LeafScan::default();
    for item in db.iterator_opt(IteratorMode::From(start, Direction::Forward), read) {
        let (key, value) = item?;
        scan.keys += 1;
        scan.bytes = scan.bytes.saturating_add((key.len() + value.len()) as u64);
        if scan.first.is_none() {
            scan.first = Some(key.to_vec());
        }
        scan.last.clear();
        scan.last.extend_from_slice(&key);
    }
    Ok(scan)
}

/// Where leaf `index` of `plan` ends: the next leaf's start, or
/// [`LEAF_END`].
fn leaf_end(plan: &LeafPlan, index: usize) -> Vec<u8> {
    plan.leaves
        .get(index + 1)
        .map_or_else(|| LEAF_END.to_vec(), |next| next.start.clone())
}

/// Step 5 (blocking): the standby's digests against the primary's plan.
/// Returns how many leaves differ and the first [`MAX_LISTED_LEAVES`] of them
/// described.
fn compare(held: &Held, plan: &LeafPlan) -> Result<(u64, Vec<LeafDifference>), VerifyError> {
    let starts: Vec<Vec<u8>> = plan.leaves.iter().map(|leaf| leaf.start.clone()).collect();
    let db: &DB = held.stage.writer().db();
    let local = local_leaf_digests(db, &starts)
        .map_err(|error| failed(format!("the standby's digest pass failed: {error}")))?;
    if local.len() != plan.leaves.len() {
        return Err(failed(
            "the standby's digest pass disagrees with the primary's leaf count",
        ));
    }
    let mut differing = 0_u64;
    let mut listed = Vec::new();
    for (index, (leaf, mine)) in plan.leaves.iter().zip(&local).enumerate() {
        if leaf.digest == *mine {
            continue;
        }
        differing += 1;
        if listed.len() >= MAX_LISTED_LEAVES {
            continue;
        }
        let end = leaf_end(plan, index);
        let scan = scan_leaf(db, &leaf.start, &end)
            .map_err(|error| failed(format!("the standby's leaf {index} could not be read: {error}")))?;
        listed.push(LeafDifference {
            index: u32::try_from(index).map_err(|_| failed("too many leaves"))?,
            start: leaf.start.clone(),
            end,
            primary: LeafSide {
                digest: leaf.digest,
                keys: u64::from(leaf.keys),
                bytes: leaf.bytes,
            },
            standby: LeafSide {
                digest: *mine,
                keys: scan.keys,
                bytes: scan.bytes,
            },
            standby_first_key: scan.first,
            standby_last_key: (scan.keys > 0).then_some(scan.last),
        });
    }
    Ok((differing, listed))
}

/// The verification's sessions with the primary.
struct Verifier<'a> {
    options: &'a VerifyOptions,
    data_dir: PathBuf,
    held: Arc<Held>,
    identity: ReceiverIdentity,
    /// Never sent: held so the sessions' shutdown watch stays open.
    _stop: watch::Sender<bool>,
    shutdown: watch::Receiver<bool>,
}

impl Verifier<'_> {
    /// An authenticated session announcing the standby's history and
    /// `cursor`: the first address that answers; a refusal is final.
    async fn connect(&self, cursor: u64) -> Result<PhysicalClient<ReplicationStream>, WireError> {
        let mut last = None;
        for source in &self.options.sources {
            match PhysicalClient::connect_replication(
                *source,
                &self.options.transport,
                &self.options.token,
                self.identity.hello(Some(self.held.history), Some(cursor)),
                self.options.wire,
                self.shutdown.clone(),
            )
            .await
            {
                Ok(client) => return Ok(client),
                Err(error @ WireError::Refused(_)) => return Err(error),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| WireError::Protocol("no address of the primary".into())))
    }

    /// Whether a refusal says "come back later"; one more attempt is then
    /// made after a backoff, up to [`REFUSAL_RETRIES`]. A superseded primary
    /// (`SourceStale`, retryable for a receiver) is not: the fix is
    /// `--against`.
    async fn retry_later(&self, error: &WireError, tries: &mut u32) -> bool {
        let later = matches!(
            error,
            WireError::Refused(refusal)
                if refusal.code.retryable() && !matches!(refusal.code, RefuseCode::SourceStale { .. })
        );
        if later && *tries < REFUSAL_RETRIES {
            *tries += 1;
            let delay = RETRY_BACKOFF
                .saturating_mul(2_u32.saturating_pow(tries.saturating_sub(1)))
                .min(MAX_RETRY_BACKOFF);
            tokio::time::sleep(delay).await;
            return true;
        }
        false
    }

    /// The command that recovers the standby.
    fn recover_hint(&self) -> String {
        format!(
            "heliosdb-nano recover --from-primary --data-dir {} --primary-host {}",
            self.data_dir.display(),
            self.options.primary_host
        )
    }

    /// Why the primary does not serve the standby's tail (or this
    /// verification at all), naming what to do.
    fn unserved(&self, error: &WireError) -> VerifyError {
        let host = &self.options.primary_host;
        let Some(refusal) = error.refusal() else {
            return failed(format!("the primary at {host} could not be reached or failed: {error}"));
        };
        match &refusal.code {
            RefuseCode::WalPruned { .. }
            | RefuseCode::Gap { .. }
            | RefuseCode::GroupTooLarge { .. }
            | RefuseCode::Diverged { .. } => refused(format!(
                "the primary at {host} no longer serves this standby's next write group after sequence {} \
                 ({refusal}); `standby verify` brings a standby to the primary's cut only through the tail a start \
                 would apply: recover it first with `{}`, then verify it",
                format_count(self.held.from),
                self.recover_hint()
            )),
            RefuseCode::EpochStale { have, want } if have > want => refused(format!(
                "the primary at {host} was promoted to epoch {have}; this standby follows epoch {want}: recover it \
                 from the new primary with `{}`",
                self.recover_hint()
            )),
            RefuseCode::SourceStale { have, want } => refused(format!(
                "the primary at {host} is a superseded primary (epoch {have} < this standby's {want}): point \
                 --against at the current primary"
            )),
            RefuseCode::HistoryMismatch => refused(format!(
                "the primary at {host} does not serve this standby's dataset at its position ({refusal}): it is \
                 another dataset, or the standby is ahead of it"
            )),
            _ => refused(format!("the primary at {host} refused: {refusal}")),
        }
    }

    /// Step 2: the primary still serves the standby's next group, and the
    /// standby's last group is the primary's.
    async fn probe_tail(&self) -> Result<(), VerifyError> {
        let (history, from, last) = (self.held.history, self.held.from, self.held.last);
        let mut tries = 0;
        loop {
            let outcome = async {
                let mut client = self.connect(from).await?;
                client.resume(history, from, last).await?;
                Ok::<_, WireError>(client)
            }
            .await;
            match outcome {
                Ok(client) => {
                    let _ = client.close().await;
                    return Ok(());
                }
                Err(error) => {
                    if self.retry_later(&error, &mut tries).await {
                        continue;
                    }
                    return Err(self.unserved(&error));
                }
            }
        }
    }

    /// Step 3: the primary's cut `P` and its leaf plan.
    async fn primary_plan(&self) -> Result<(Uuid, u64, LeafPlan), VerifyError> {
        let (history, from) = (self.held.history, self.held.from);
        let leaf_bytes = self.options.leaf_bytes;
        let mut tries = 0;
        loop {
            let outcome = async {
                let mut client = self.connect(from).await?;
                if !client.fast_forward() {
                    return Err(WireError::Refused(Refusal::new(
                        RefuseCode::Version,
                        "the primary's release serves no leaf digests (no fast-forward capability): upgrade it to \
                         verify a standby against it",
                    )));
                }
                // Spec R1 §1: a caught-up standby is compared AT the
                // primary's cut, which only a release offering
                // `CAP_FAST_FORWARD_AT_CUT` serves; an older one would refuse
                // it as another dataset.
                if !client.fast_forward_at_cut() {
                    return Err(WireError::Refused(Refusal::new(
                        RefuseCode::Version,
                        "the primary's release cannot compare a standby at its own sequence (it does not offer \
                         CAP_FAST_FORWARD_AT_CUT): upgrade the primary to verify a standby against it",
                    )));
                }
                let info = client.open_fast_forward(history, from, leaf_bytes, None).await?;
                let SourceSessionInfo::FastForward {
                    snapshot_id, sequence, ..
                } = info
                else {
                    return Err(WireError::Protocol(
                        "the primary opened another kind of session than a fast-forward".into(),
                    ));
                };
                let plan = client.fetch_leaf_digests().await?;
                let _ = client.close().await;
                Ok::<_, WireError>((snapshot_id, sequence, plan))
            }
            .await;
            match outcome {
                Ok((snapshot_id, sequence, plan)) => {
                    if plan.snapshot_id != snapshot_id {
                        return Err(failed(
                            "the primary's leaf plan names another snapshot than its session",
                        ));
                    }
                    if sequence < from || plan.leaves.is_empty() {
                        return Err(failed(format!(
                            "the primary's cut {sequence} is behind the standby's sequence {from}, or its plan is \
                             empty"
                        )));
                    }
                    return Ok((snapshot_id, sequence, plan));
                }
                Err(error) => {
                    if self.retry_later(&error, &mut tries).await {
                        continue;
                    }
                    return Err(self.unserved(&error));
                }
            }
        }
    }

    /// Step 4: the groups `S+1..=through` applied to the standby, and nothing
    /// past `through`. Returns how many groups were applied.
    async fn tail_to(&self, through: u64) -> Result<u64, VerifyError> {
        let (history, from, last) = (self.held.history, self.held.from, self.held.last);
        if from >= through {
            return Ok(0);
        }
        let mut tries = 0;
        let mut client = loop {
            let outcome = async {
                let mut client = self.connect(from).await?;
                client.resume(history, from, last).await?;
                Ok::<_, WireError>(client)
            }
            .await;
            match outcome {
                Ok(client) => break client,
                Err(error) => {
                    if self.retry_later(&error, &mut tries).await {
                        continue;
                    }
                    return Err(self.unserved(&error));
                }
            }
        };
        // An IDLE deadline: reset by every poll that brings groups.
        let budget = self.options.wire.operation_timeout.saturating_mul(TAIL_DEADLINES);
        let mut deadline = Instant::now() + budget;
        let mut cursor = from;
        let mut groups = 0_u64;
        while cursor < through {
            let mut sink = TailSink {
                through,
                batches: Vec::new(),
                straddling: None,
            };
            let summary = client.poll(cursor, TAIL_POLL_BYTES, &mut sink).await.map_err(|error| {
                failed(format!(
                    "the tail from the primary at {} stopped at sequence {} ({error}); the groups applied are \
                     durable: re-run the command",
                    self.options.primary_host,
                    format_count(cursor)
                ))
            })?;
            if let Some((first, last)) = sink.straddling {
                return Err(failed(format!(
                    "the primary's write group {first}..{last} crosses its own cut {through}"
                )));
            }
            if sink.batches.is_empty() {
                if summary.through > cursor {
                    return Err(failed(format!(
                        "the primary served sequences {}..{} past its cut {through} before reaching it",
                        cursor + 1,
                        summary.through
                    )));
                }
                if Instant::now() >= deadline {
                    return Err(failed(format!(
                        "the tail stopped at sequence {}, short of the primary's cut {through}: no write group \
                         arrived for {} s",
                        format_count(cursor),
                        budget.as_secs()
                    )));
                }
                tokio::time::sleep(IDLE_POLL_WAIT).await;
                continue;
            }
            let count = sink.batches.len() as u64;
            let held = Arc::clone(&self.held);
            let batches = sink.batches;
            cursor = blocking(move || apply_tail(&held, &batches)).await?;
            groups += count;
            deadline = Instant::now() + budget;
        }
        let _ = client.close().await;
        Ok(groups)
    }
}

/// The standby's own WAL archive directory, when it exists: the configured
/// `[replication.archive] dir`, else `<D>.archive` beside the data directory.
fn standby_archive(options: &VerifyOptions, data_dir: &Path) -> Option<PathBuf> {
    let archive = match &options.config.replication.archive.dir {
        Some(dir) => dir.clone(),
        None => {
            let mut name = data_dir.file_name()?.to_os_string();
            name.push(".archive");
            data_dir.with_file_name(name)
        }
    };
    archive.is_dir().then_some(archive)
}

/// Spec R1 §1: verify the STOPPED physical standby `options.data_dir` against
/// its primary (see the module documentation). `progress` receives the
/// `standby verify: …` lines. Storage work runs on blocking threads; the
/// primary is reached on its replication port, over TLS as
/// `options.transport` says. `Ok` whatever the verdict
/// ([`VerifyReport::identical`]); `Err` when nothing could be compared.
pub async fn verify_standby(
    options: &VerifyOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<VerifyReport, VerifyError> {
    if !valid_leaf_bytes(options.leaf_bytes) {
        return Err(refused(format!(
            "--leaf-bytes {} is outside 64KiB..=256MiB",
            options.leaf_bytes
        )));
    }
    if options.sources.is_empty() {
        return Err(refused("--against resolved to no address"));
    }
    let started = Instant::now();
    // The directory must exist (it is an installed standby), so the
    // filesystem resolves it: no symlink or `..` reaches the stage checks.
    let data_dir = std::fs::canonicalize(&options.data_dir).map_err(|error| {
        refused(format!(
            "{} is not an installed physical standby ({error})",
            options.data_dir.display()
        ))
    })?;
    let held = {
        let (dir, config) = (data_dir.clone(), options.config.clone());
        let (limits, host) = (options.wire.transfer, options.primary_host.clone());
        blocking(move || hold_standby(&dir, &config, limits, &host)).await?
    };
    let (history, epoch, from) = (held.history, held.epoch, held.from);
    let held = Arc::new(held);
    let (stop, shutdown) = watch::channel(false);
    let verifier = Verifier {
        options,
        data_dir: data_dir.clone(),
        held: Arc::clone(&held),
        identity: ReceiverIdentity {
            node_id: Uuid::new_v4(),
            role: PeerRole::Resync,
            epoch,
            pg_port: 0,
            sync_mode: WireSyncMode::Async,
        },
        _stop: stop,
        shutdown,
    };
    progress(&format!(
        "standby verify: the physical standby {} (history {history}, epoch {epoch}) is at sequence {}",
        data_dir.display(),
        format_count(from)
    ));
    let outcome = async {
        verifier.probe_tail().await?;
        let (snapshot_id, sequence, plan) = verifier.primary_plan().await?;
        progress(&format!(
            "standby verify: the primary at {} pinned its snapshot {snapshot_id} at sequence {}: {} leaves of {}, {}",
            options.primary_host,
            format_count(sequence),
            format_count(plan.leaves.len() as u64),
            format_bytes(u64::from(options.leaf_bytes)),
            format_bytes(plan.total_bytes)
        ));
        let groups = verifier.tail_to(sequence).await?;
        if groups > 0 {
            progress(&format!(
                "standby verify: applied the primary's write groups {}..{} ({} groups), as a start would apply them",
                format_count(from + 1),
                format_count(sequence),
                format_count(groups)
            ));
            if let Some(archive) = standby_archive(options, &data_dir) {
                progress(&format!(
                    "standby verify: the standby's own WAL archive {} will not hold sequences {}..{}, applied \
                     offline (as `recover` and `promote` apply theirs): its next start records them as a gap there. \
                     A recovery from that archive cannot cross the gap — recover across it from the primary's \
                     archive, which holds the range — and a standby cascaded from this one is served the range from \
                     this standby's WAL while that holds it, never from its archive",
                    archive.display(),
                    format_count(from + 1),
                    format_count(sequence)
                ));
            }
        } else {
            progress("standby verify: the standby is at the primary's cut already: nothing applied");
        }
        let compared = Arc::clone(&verifier.held);
        let (differing_leaves, differing) = {
            let plan = plan.clone();
            blocking(move || compare(&compared, &plan)).await?
        };
        Ok::<_, VerifyError>((sequence, groups, plan, differing_leaves, differing))
    }
    .await;
    drop(verifier);
    // The stage is the only handle: closed whatever happened.
    let closed = blocking(move || close_held(held)).await;
    let (sequence, tailed_groups, plan, differing_leaves, differing) = outcome?;
    let reached = closed?;
    if reached != sequence {
        return Err(failed(format!(
            "the standby's durable cursor is {reached}, not the cut {sequence} it was compared at"
        )));
    }
    Ok(VerifyReport {
        data_dir,
        primary_host: options.primary_host.clone(),
        history,
        from,
        sequence,
        tailed_groups,
        leaf_bytes: options.leaf_bytes,
        leaves: plan.leaves.len() as u64,
        bytes_compared: plan.total_bytes,
        differing_leaves,
        differing,
        duration: started.elapsed(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::replication::fast_forward::LeafPlanner;
    use crate::replication::physical_wire::LeafEntry;

    #[test]
    fn keys_render_printable_and_escape_the_rest() {
        assert_eq!(render_key(b""), "<start of keyspace>");
        assert_eq!(render_key(&LEAF_END), "<end of keyspace>");
        assert_eq!(render_key(b"data:t:17"), "data:t:17");
        assert_eq!(render_key(b"a\x00b\xff"), "a\\x00b\\xff");
        assert_eq!(render_key(b"back\\slash"), "back\\\\slash");
        let long = vec![b'k'; MAX_RENDERED_KEY_BYTES + 5];
        let rendered = render_key(&long);
        assert!(rendered.ends_with('…'), "{rendered}");
        assert_eq!(rendered.chars().filter(|c| *c == 'k').count(), MAX_RENDERED_KEY_BYTES);
    }

    /// A primary's plan over a scratch store, the standby a copy with one
    /// pair changed: exactly the leaf holding it differs, its range holds the
    /// key, and the standby's side names it.
    #[test]
    fn the_comparison_names_exactly_the_leaf_that_differs() {
        let root = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            let mut options = rocksdb::Options::default();
            options.create_if_missing(true);
            DB::open(&options, root.path().join(name)).unwrap()
        };
        let (primary, standby) = (open("primary"), open("standby"));
        for row in 0..400_u32 {
            let key = format!("data:t:{row:06}");
            let value = vec![b'v'; 300];
            primary.put(&key, &value).unwrap();
            standby.put(&key, &value).unwrap();
        }
        let changed = b"data:t:000250".to_vec();
        standby.put(&changed, b"tampered").unwrap();

        let mut planner = LeafPlanner::new(16 * 1024, 1 << 20);
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        for item in primary.iterator_opt(IteratorMode::Start, read) {
            let (key, value) = item.unwrap();
            planner.add(&key, &value).unwrap();
        }
        let leaves = planner.finish().unwrap();
        assert!(leaves.len() > 3, "the test needs several leaves: {}", leaves.len());
        let plan = LeafPlan {
            snapshot_id: Uuid::new_v4(),
            total_bytes: leaves.iter().map(|leaf| leaf.bytes).sum(),
            leaves: leaves.iter().map(|leaf| leaf.entry()).collect::<Vec<LeafEntry>>(),
        };
        let starts: Vec<Vec<u8>> = plan.leaves.iter().map(|leaf| leaf.start.clone()).collect();
        let local = local_leaf_digests(&standby, &starts).unwrap();
        let differing: Vec<usize> = plan
            .leaves
            .iter()
            .zip(&local)
            .enumerate()
            .filter(|(_, (leaf, mine))| leaf.digest != **mine)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(differing.len(), 1, "{differing:?}");
        let index = differing[0];
        let end = leaf_end(&plan, index);
        assert!(plan.leaves[index].start.as_slice() <= changed.as_slice() && changed < end);
        let scan = scan_leaf(&standby, &plan.leaves[index].start, &end).unwrap();
        assert_eq!(
            scan.keys,
            u64::from(plan.leaves[index].keys),
            "the same keys, one value changed"
        );
        assert!(scan.first.as_deref().is_some_and(|first| first <= changed.as_slice()));
        assert!(scan.last.as_slice() >= changed.as_slice());
    }

    /// The tail's sink keeps the groups at or before the cut, drops the ones
    /// past it, and notes one that crosses it.
    #[tokio::test]
    async fn the_tail_sink_keeps_only_what_ends_at_the_cut() {
        let history = Uuid::new_v4();
        let batch = |first: u64, ops: u32| {
            let mut write = rocksdb::WriteBatch::default();
            for op in 0..ops {
                write.put(format!("k{first}-{op}"), b"v");
            }
            let mut raw = write.data().to_vec();
            raw[..8].copy_from_slice(&first.to_le_bytes());
            let digest = {
                use sha2::{Digest, Sha256};
                let digest: [u8; 32] = Sha256::digest(&raw).into();
                digest
            };
            PhysicalBatch::decode(history, first, raw, digest).unwrap()
        };
        let mut sink = TailSink {
            through: 12,
            batches: Vec::new(),
            straddling: None,
        };
        sink.apply(batch(10, 1)).await.unwrap();
        sink.apply(batch(11, 2)).await.unwrap();
        sink.apply(batch(13, 1)).await.unwrap();
        assert_eq!(sink.batches.len(), 2);
        assert_eq!(sink.batches.last().unwrap().last_sequence(), 12);
        assert!(sink.straddling.is_none());
        let mut crossing = TailSink {
            through: 11,
            batches: Vec::new(),
            straddling: None,
        };
        crossing.apply(batch(11, 2)).await.unwrap();
        assert_eq!(crossing.straddling, Some((11, 12)));
        assert!(crossing.batches.is_empty());
    }
}
