//! Archived WAL segments (spec P3.1 §2, plan §D4): the files a node's WAL
//! archiver seals every write group into.
//!
//! # Why a file format of our own
//!
//! RocksDB's own WAL retention (`[replication.physical] wal_retention_secs`)
//! is a coarse per-file timer with no catalog: it cannot say what it holds,
//! cannot be copied to another host and checked there, and is gone when its
//! timer runs out. A segment is this database's archived redo log: a
//! self-describing, checksummed file of whole write groups, named by the
//! sequence interval it covers, so a directory of segments is its own catalog.
//!
//! # Layout (integers big-endian, like the replication wire)
//!
//! ```text
//! header, 132 bytes
//!   magic "HSEG0001" | format u16 = 1 | flags u16 (bit 0: frames sealed with the dataset key)
//!   history uuid | epoch u64 | first_sequence u64 | created_at i64 (unix seconds)
//!   node_id uuid | key_check [u8; 32] | SHA-256 of the 100 bytes before it
//! body: frames, each  length u32 | payload
//!   payload = the NPHYS003 `Batch` payload: history(16) | first u64 | digest(32) | raw
//!   (sealed segment: the payload sealed as one value with the dataset key)
//! footer, 112 bytes
//!   magic "HSEGEND1" | last_sequence u64 | batch_count u64
//!   first_observed_at i64 | last_observed_at i64 | body_bytes u64
//!   file SHA-256 over header + body | SHA-256 of the 80 footer bytes before it
//! ```
//!
//! One write group is one frame and a frame is never split across segments.
//! A frame decodes with the replication wire's own validation
//! ([`PhysicalBatch::decode`]: history, first sequence, digest, record walk).
//! The raw batch carries its keys in plaintext and its values exactly as the
//! primary stored them — ciphertext for the sealed keyspaces of an encrypted
//! dataset, plaintext for the rest — which is why an encrypted dataset's frames
//! are sealed whole (`storage::tde::seal`, AES-256-GCM with the dataset key).
//! `key_check` is the SHA-256 of the dataset's stored `meta:tde:keycheck`
//! value (zeros when encryption is off): it names the dataset key a sealed
//! segment needs without revealing it.
//!
//! `observed_at` is the archiver's clock when it saw a write group: RocksDB
//! records no commit time, so nothing here is called one.
//!
//! # Files
//!
//! `<archive>/<history>/<epoch>/<first:020>-<last:020>.hseg`, mode 0600 in
//! 0700 directories; names sort in sequence order. A segment is written as
//! `<first:020>-open.hseg.partial` and sealed by footer + fsync + rename +
//! directory fsync, so a sealed name always holds a whole segment. After a
//! crash, [`repair_partial`] keeps every intact frame of an open segment
//! (frames are self-digested) and seals them.
//!
//! Every refusal names its file: `segment <path>: <reason>`.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::physical::{PhysicalBatch, MAX_BATCH_BYTES};
use crate::crypto::KeyManager;

pub const SEGMENT_MAGIC: [u8; 8] = *b"HSEG0001";
pub const FOOTER_MAGIC: [u8; 8] = *b"HSEGEND1";
pub const SEGMENT_FORMAT: u16 = 1;
/// Header flag: every frame is sealed with the dataset key.
pub const FLAG_SEALED: u16 = 1;
const HEADER_FIELDS_LEN: usize = 8 + 2 + 2 + 16 + 8 + 8 + 8 + 16 + 32;
pub const HEADER_LEN: usize = HEADER_FIELDS_LEN + 32;
const FOOTER_FIELDS_LEN: usize = 8 + 8 + 8 + 8 + 8 + 8 + 32;
pub const FOOTER_LEN: usize = FOOTER_FIELDS_LEN + 32;
/// The wire `Batch` payload's metadata before the raw batch: history, first
/// sequence, digest.
pub const FRAME_META: usize = 16 + 8 + 32;
/// The largest frame payload: the largest batch, plus the AES-GCM framing
/// of a sealed one. Spec P4.1c: [`MAX_BATCH_BYTES`] is the most a `u32`
/// frame length can carry with these prefixes, so this still fits the
/// length word; a reader allocates a frame only after checking its length
/// against it and against the bytes the segment's body has left.
pub const MAX_FRAME_LEN: usize = FRAME_META + MAX_BATCH_BYTES + crate::crypto::MIN_CIPHERTEXT_LEN;
pub const SEGMENT_SUFFIX: &str = ".hseg";
pub const PARTIAL_SUFFIX: &str = ".hseg.partial";
/// An open segment that could not be repaired is renamed with this suffix:
/// listed as corrupt, never pruned, never overwritten.
pub const CORRUPT_SUFFIX: &str = ".corrupt";
/// Where an encrypted dataset keeps its key-verification sentinel
/// (`storage::tde`); a segment's `key_check` is the SHA-256 of its value.
pub const KEY_CHECK_KEY: &[u8] = b"meta:tde:keycheck";
/// The refusal for a sealed segment read without the dataset key.
pub const SEALED_WITHOUT_KEY: &str = "sealed segment: dataset key required";

/// A segment that cannot be used, and why. Always names the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("segment {}: {}", .path.display(), .reason)]
pub struct SegmentError {
    pub path: PathBuf,
    pub reason: String,
}

pub type SegmentResult<T> = std::result::Result<T, SegmentError>;

fn refuse(path: &Path, reason: impl Into<String>) -> SegmentError {
    SegmentError {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn io_refuse(path: &Path, what: &str, error: &std::io::Error) -> SegmentError {
    refuse(path, format!("{what}: {error}"))
}

/// The archiver's clock: unix seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

fn modified_secs(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(modified.as_secs()).ok()
}

/// A bounds-checked cursor over fixed-size fields.
struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("field length overflow")?;
        let slice = self.bytes.get(self.at..end).ok_or("truncated")?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?.try_into().map_err(|_| "truncated".to_string())
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    fn uuid(&mut self) -> Result<Uuid, String> {
        Ok(Uuid::from_bytes(self.array()?))
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A segment's header: what it belongs to and where it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    pub flags: u16,
    pub history: Uuid,
    pub epoch: u64,
    pub first_sequence: u64,
    /// Unix seconds when the segment was opened (its first write group was
    /// observed then).
    pub created_at: i64,
    /// The node that wrote it.
    pub node_id: Uuid,
    /// SHA-256 of the dataset's `meta:tde:keycheck` value; zeros when the
    /// dataset is not encrypted.
    pub key_check: [u8; 32],
}

impl SegmentHeader {
    /// Whether every frame is sealed with the dataset key.
    pub fn sealed(&self) -> bool {
        self.flags & FLAG_SEALED != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN);
        out.extend_from_slice(&SEGMENT_MAGIC);
        out.extend_from_slice(&SEGMENT_FORMAT.to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(self.history.as_bytes());
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.first_sequence.to_be_bytes());
        out.extend_from_slice(&self.created_at.to_be_bytes());
        out.extend_from_slice(self.node_id.as_bytes());
        out.extend_from_slice(&self.key_check);
        let hash = sha256(&out);
        out.extend_from_slice(&hash);
        out
    }

    /// Decode and verify `bytes` (exactly [`HEADER_LEN`] of them). The magic
    /// is checked first, then the header hash, then the fields: a flipped
    /// byte anywhere is named `header hash mismatch`.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != HEADER_LEN {
            return Err(format!("truncated header: {} of {HEADER_LEN} bytes", bytes.len()));
        }
        let mut fields = Fields::new(bytes);
        if fields.array::<8>()? != SEGMENT_MAGIC {
            return Err("bad magic: not an archived WAL segment".into());
        }
        let covered = bytes.get(..HEADER_FIELDS_LEN).ok_or("truncated header")?;
        let stored = bytes.get(HEADER_FIELDS_LEN..).ok_or("truncated header")?;
        if sha256(covered).as_slice() != stored {
            return Err("header hash mismatch".into());
        }
        let format = fields.u16()?;
        if format != SEGMENT_FORMAT {
            return Err(format!("unsupported segment format {format}"));
        }
        let flags = fields.u16()?;
        if flags & !FLAG_SEALED != 0 {
            return Err(format!("unknown header flags {flags:#06x}"));
        }
        let header = Self {
            flags,
            history: fields.uuid()?,
            epoch: fields.u64()?,
            first_sequence: fields.u64()?,
            created_at: fields.i64()?,
            node_id: fields.uuid()?,
            key_check: fields.array()?,
        };
        if header.history.is_nil() || header.first_sequence == 0 {
            return Err("header names a nil history or sequence 0".into());
        }
        Ok(header)
    }
}

/// A sealed segment's footer: where it ends, what it holds, and the hash of
/// everything before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentFooter {
    pub last_sequence: u64,
    pub batch_count: u64,
    /// The archiver's clock (unix seconds) when it saw the first and the last
    /// write group of the segment.
    pub first_observed_at: i64,
    pub last_observed_at: i64,
    pub body_bytes: u64,
    /// SHA-256 over the header and the body.
    pub file_sha256: [u8; 32],
}

impl SegmentFooter {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FOOTER_LEN);
        out.extend_from_slice(&FOOTER_MAGIC);
        out.extend_from_slice(&self.last_sequence.to_be_bytes());
        out.extend_from_slice(&self.batch_count.to_be_bytes());
        out.extend_from_slice(&self.first_observed_at.to_be_bytes());
        out.extend_from_slice(&self.last_observed_at.to_be_bytes());
        out.extend_from_slice(&self.body_bytes.to_be_bytes());
        out.extend_from_slice(&self.file_sha256);
        let hash = sha256(&out);
        out.extend_from_slice(&hash);
        out
    }

    /// Decode and verify `bytes` (exactly [`FOOTER_LEN`] of them): magic,
    /// then the footer hash, then the fields.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != FOOTER_LEN {
            return Err(format!("truncated footer: {} of {FOOTER_LEN} bytes", bytes.len()));
        }
        let mut fields = Fields::new(bytes);
        if fields.array::<8>()? != FOOTER_MAGIC {
            return Err("bad footer magic: the segment is truncated or was never sealed".into());
        }
        let covered = bytes.get(..FOOTER_FIELDS_LEN).ok_or("truncated footer")?;
        let stored = bytes.get(FOOTER_FIELDS_LEN..).ok_or("truncated footer")?;
        if sha256(covered).as_slice() != stored {
            return Err("footer hash mismatch".into());
        }
        Ok(Self {
            last_sequence: fields.u64()?,
            batch_count: fields.u64()?,
            first_observed_at: fields.i64()?,
            last_observed_at: fields.i64()?,
            body_bytes: fields.u64()?,
            file_sha256: fields.array()?,
        })
    }
}

/// `<first:020>-<last:020>.hseg`: names sort in sequence order.
pub fn segment_file_name(first: u64, last: u64) -> String {
    format!("{first:020}-{last:020}{SEGMENT_SUFFIX}")
}

/// `<first:020>-open.hseg.partial`: the segment being written.
pub fn partial_file_name(first: u64) -> String {
    format!("{first:020}-open{PARTIAL_SUFFIX}")
}

fn twenty_digits(text: &str) -> Option<u64> {
    if text.len() != 20 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The interval a sealed segment's file name claims.
pub fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    let (first, last) = name.strip_suffix(SEGMENT_SUFFIX)?.split_once('-')?;
    Some((twenty_digits(first)?, twenty_digits(last)?))
}

/// The first sequence an open segment's file name claims.
pub fn parse_partial_name(name: &str) -> Option<u64> {
    twenty_digits(name.strip_suffix(PARTIAL_SUFFIX)?.strip_suffix("-open")?)
}

/// `<root>/<history>/<epoch>`: where one history's segments of one epoch live.
pub fn segment_dir(root: &Path, history: Uuid, epoch: u64) -> PathBuf {
    root.join(history.to_string()).join(epoch.to_string())
}

/// Create `dir` and its missing parents owner-only (0700).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// fsync a directory, so a rename or removal in it is durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The wire `Batch` payload of `batch`: `history(16) | first u64 BE |
/// digest(32) | raw` — byte for byte what `physical_wire` sends.
pub fn encode_batch_payload(batch: &PhysicalBatch) -> Vec<u8> {
    let mut payload = Vec::with_capacity(FRAME_META + batch.data().len());
    payload.extend_from_slice(&batch_meta(batch));
    payload.extend_from_slice(batch.data());
    payload
}

/// The `history | first | digest` prefix of [`encode_batch_payload`], alone.
fn batch_meta(batch: &PhysicalBatch) -> [u8; FRAME_META] {
    let history = batch.history();
    let first = batch.first_sequence().to_be_bytes();
    let digest = batch.digest();
    let fields = history.as_bytes().iter().chain(&first).chain(&digest);
    let mut meta = [0_u8; FRAME_META];
    for (to, from) in meta.iter_mut().zip(fields) {
        *to = *from;
    }
    meta
}

/// Decode a wire `Batch` payload through [`PhysicalBatch::decode`], which
/// verifies the history, the first sequence against the raw header, the
/// record walk and the digest.
pub fn decode_batch_payload(mut payload: Vec<u8>) -> Result<PhysicalBatch, String> {
    if payload.len() < FRAME_META + 12 {
        return Err(format!("frame payload truncated ({} bytes)", payload.len()));
    }
    let raw = payload.split_off(FRAME_META);
    let mut fields = Fields::new(&payload);
    let history = fields.uuid()?;
    let first = fields.u64()?;
    let digest: [u8; 32] = fields.array()?;
    PhysicalBatch::decode(history, first, raw, digest).map_err(|error| error.to_string())
}

/// One frame of `header`'s segment, opened and verified: it must start at
/// `expected_first` and belong to the segment's history.
fn open_frame(
    path: &Path,
    payload: Vec<u8>,
    header: &SegmentHeader,
    key: Option<&KeyManager>,
    expected_first: u64,
) -> SegmentResult<PhysicalBatch> {
    let plain = if header.sealed() {
        let key = key.ok_or_else(|| refuse(path, SEALED_WITHOUT_KEY))?;
        // Strict: a frame that does not open with this key is refused, never
        // read as plaintext (the stored-value codec's tolerant open would
        // turn a wrong key into a misleading decode error).
        crate::crypto::decrypt(key.key(), &payload).map_err(|_| {
            refuse(
                path,
                format!("the frame at sequence {expected_first} does not open with this dataset key"),
            )
        })?
    } else {
        payload
    };
    let batch = decode_batch_payload(plain).map_err(|error| {
        refuse(
            path,
            format!("frame digest: the frame at sequence {expected_first} does not verify ({error})"),
        )
    })?;
    if batch.history() != header.history {
        return Err(refuse(
            path,
            format!(
                "other history: the frame at sequence {} belongs to history {}, the segment to {}",
                batch.first_sequence(),
                batch.history(),
                header.history
            ),
        ));
    }
    if batch.first_sequence() != expected_first {
        return Err(refuse(
            path,
            format!(
                "noncontiguous frame: expected sequence {expected_first}, the frame starts at {}",
                batch.first_sequence()
            ),
        ));
    }
    Ok(batch)
}

/// What a sealed segment is, as its header and footer say (verified).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub header: SegmentHeader,
    pub footer: SegmentFooter,
    pub file_bytes: u64,
    /// Unix seconds the file was last written (its seal), when known.
    pub sealed_at: Option<i64>,
}

impl SegmentInfo {
    pub fn history(&self) -> Uuid {
        self.header.history
    }

    pub fn epoch(&self) -> u64 {
        self.header.epoch
    }

    pub fn first_sequence(&self) -> u64 {
        self.header.first_sequence
    }

    pub fn last_sequence(&self) -> u64 {
        self.footer.last_sequence
    }

    pub fn batch_count(&self) -> u64 {
        self.footer.batch_count
    }

    pub fn sealed_frames(&self) -> bool {
        self.header.sealed()
    }

    pub fn first_observed_at(&self) -> i64 {
        self.footer.first_observed_at
    }

    pub fn last_observed_at(&self) -> i64 {
        self.footer.last_observed_at
    }

    /// The file hash (header + body), lowercase hex.
    pub fn sha256_hex(&self) -> String {
        hex::encode(self.footer.file_sha256)
    }

    /// Re-hash the whole file against its footer and decode every frame:
    /// each must verify, start where the previous one ended, and belong to the
    /// segment's history; the last must end at the footer's sequence. A
    /// sealed segment's frames need the dataset key: without it the file hash
    /// is still checked, then `sealed segment: dataset key required`.
    pub fn verify_deep(&self, key: Option<&KeyManager>) -> SegmentResult<()> {
        let path = &self.path;
        let mut reader = BufReader::new(File::open(path).map_err(|error| io_refuse(path, "cannot open", &error))?);
        let mut header_bytes = vec![0_u8; HEADER_LEN];
        reader
            .read_exact(&mut header_bytes)
            .map_err(|error| io_refuse(path, "cannot read the header", &error))?;
        if SegmentHeader::decode(&header_bytes).as_ref() != Ok(&self.header) {
            return Err(refuse(path, "the header changed since the segment was opened"));
        }
        let decode = !self.header.sealed() || key.is_some();
        let body_bytes = self.footer.body_bytes;
        let mut body = Hashing {
            inner: (&mut reader).take(body_bytes),
            hasher: Sha256::new(),
        };
        body.hasher.update(&header_bytes);
        let mut structure: Option<String> = None;
        let mut decoded: Option<SegmentError> = None;
        let mut consumed = 0_u64;
        let mut frames = 0_u64;
        let mut next = self.header.first_sequence;
        let mut last = None;
        while consumed < body_bytes {
            let mut length = [0_u8; 4];
            if let Err(error) = body.read_exact(&mut length) {
                structure = Some(format!("truncated frame length at body offset {consumed}: {error}"));
                break;
            }
            let length = u32::from_be_bytes(length);
            let size = usize::try_from(length).unwrap_or(usize::MAX);
            let left = body_bytes.saturating_sub(consumed).saturating_sub(4);
            if size == 0 || size > MAX_FRAME_LEN || u64::from(length) > left {
                structure = Some(format!(
                    "frame length {length} at body offset {consumed} is out of bounds"
                ));
                break;
            }
            let mut payload = vec![0_u8; size];
            if let Err(error) = body.read_exact(&mut payload) {
                structure = Some(format!("truncated frame at body offset {consumed}: {error}"));
                break;
            }
            consumed += 4 + u64::from(length);
            frames += 1;
            if decode && decoded.is_none() {
                match open_frame(path, payload, &self.header, key, next) {
                    Ok(batch) => {
                        next = batch.last_sequence().saturating_add(1);
                        last = Some(batch.last_sequence());
                    }
                    Err(error) => decoded = Some(error),
                }
            }
        }
        // Hash whatever the frame walk did not read, so the file hash is
        // checked over the whole body even when a frame is malformed.
        std::io::copy(&mut body, &mut std::io::sink())
            .map_err(|error| io_refuse(path, "cannot read the body", &error))?;
        let computed: [u8; 32] = body.hasher.finalize().into();
        if computed != self.footer.file_sha256 {
            return Err(refuse(
                path,
                "file hash mismatch: the segment's bytes are not the bytes it was sealed with",
            ));
        }
        if let Some(reason) = structure {
            return Err(refuse(path, reason));
        }
        if let Some(error) = decoded {
            return Err(error);
        }
        if frames != self.footer.batch_count {
            return Err(refuse(
                path,
                format!(
                    "the footer counts {} frames, the body holds {frames}",
                    self.footer.batch_count
                ),
            ));
        }
        if !decode {
            return Err(refuse(path, SEALED_WITHOUT_KEY));
        }
        if last != Some(self.footer.last_sequence) {
            return Err(refuse(
                path,
                format!(
                    "the last frame ends at {last:?}, the footer says {}",
                    self.footer.last_sequence
                ),
            ));
        }
        Ok(())
    }

    /// The segment's write groups, in order. A sealed segment needs the
    /// dataset key (`sealed segment: dataset key required` without it). Each
    /// item is verified like [`Self::verify_deep`] verifies it; the first
    /// failure ends the iteration.
    pub fn frames<'k>(&self, key: Option<&'k KeyManager>) -> SegmentResult<FrameIter<'k>> {
        let path = &self.path;
        if self.header.sealed() && key.is_none() {
            return Err(refuse(path, SEALED_WITHOUT_KEY));
        }
        let mut file = File::open(path).map_err(|error| io_refuse(path, "cannot open", &error))?;
        let mut header_bytes = vec![0_u8; HEADER_LEN];
        file.read_exact(&mut header_bytes)
            .map_err(|error| io_refuse(path, "cannot read the header", &error))?;
        if SegmentHeader::decode(&header_bytes).as_ref() != Ok(&self.header) {
            return Err(refuse(path, "the header changed since the segment was opened"));
        }
        Ok(FrameIter {
            path: path.clone(),
            header: self.header.clone(),
            reader: BufReader::new(file).take(self.footer.body_bytes),
            key,
            next: self.header.first_sequence,
            remaining: self.footer.batch_count,
            failed: false,
        })
    }
}

/// A reader that hashes every byte it hands out.
struct Hashing<R: Read> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(buf.get(..read).unwrap_or_default());
        Ok(read)
    }
}

/// The write groups of one segment ([`SegmentInfo::frames`]).
pub struct FrameIter<'k> {
    path: PathBuf,
    header: SegmentHeader,
    reader: std::io::Take<BufReader<File>>,
    key: Option<&'k KeyManager>,
    next: u64,
    remaining: u64,
    failed: bool,
}

impl FrameIter<'_> {
    fn read_frame(&mut self) -> SegmentResult<PhysicalBatch> {
        let mut length = [0_u8; 4];
        self.reader
            .read_exact(&mut length)
            .map_err(|error| io_refuse(&self.path, "truncated frame length", &error))?;
        let size = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
        // Spec P4.1c: a frame may be gigabytes, so a damaged length word is
        // checked against what the body has left before it is allocated.
        if size == 0 || size > MAX_FRAME_LEN || u64::from(u32::from_be_bytes(length)) > self.reader.limit() {
            return Err(refuse(&self.path, format!("frame length {size} is out of bounds")));
        }
        let mut payload = vec![0_u8; size];
        self.reader
            .read_exact(&mut payload)
            .map_err(|error| io_refuse(&self.path, "truncated frame", &error))?;
        open_frame(&self.path, payload, &self.header, self.key, self.next)
    }
}

impl Iterator for FrameIter<'_> {
    type Item = SegmentResult<PhysicalBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining == 0 {
            return None;
        }
        let result = self.read_frame();
        match &result {
            Ok(batch) => {
                self.next = batch.last_sequence().saturating_add(1);
                self.remaining -= 1;
            }
            Err(_) => self.failed = true,
        }
        Some(result)
    }
}

/// Opens sealed segments.
pub struct SegmentReader;

impl SegmentReader {
    /// Verify a sealed segment's header and footer (two reads, not the
    /// body): magic, both hashes, the body length against the file size, a
    /// consistent interval, and the file name against the contents.
    pub fn open(path: &Path) -> SegmentResult<SegmentInfo> {
        Self::open_expecting(path, None)
    }

    /// [`Self::open`] for a segment found in `<history>/<epoch>`: a segment of
    /// another history or epoch is refused by name.
    pub fn open_expecting(path: &Path, expected: Option<(Uuid, u64)>) -> SegmentResult<SegmentInfo> {
        let mut file = File::open(path).map_err(|error| io_refuse(path, "cannot open", &error))?;
        let metadata = file
            .metadata()
            .map_err(|error| io_refuse(path, "cannot inspect", &error))?;
        let size = metadata.len();
        let minimum = (HEADER_LEN + FOOTER_LEN) as u64;
        if size < minimum {
            return Err(refuse(
                path,
                format!("truncated: {size} bytes, a sealed segment has at least {minimum}"),
            ));
        }
        let mut header_bytes = vec![0_u8; HEADER_LEN];
        file.read_exact(&mut header_bytes)
            .map_err(|error| io_refuse(path, "cannot read the header", &error))?;
        let header = SegmentHeader::decode(&header_bytes).map_err(|reason| refuse(path, reason))?;
        file.seek(SeekFrom::Start(size - FOOTER_LEN as u64))
            .map_err(|error| io_refuse(path, "cannot seek to the footer", &error))?;
        let mut footer_bytes = vec![0_u8; FOOTER_LEN];
        file.read_exact(&mut footer_bytes)
            .map_err(|error| io_refuse(path, "cannot read the footer", &error))?;
        let footer = SegmentFooter::decode(&footer_bytes).map_err(|reason| refuse(path, reason))?;
        let expected_size = (HEADER_LEN as u64)
            .checked_add(footer.body_bytes)
            .and_then(|bytes| bytes.checked_add(FOOTER_LEN as u64));
        if expected_size != Some(size) {
            return Err(refuse(
                path,
                format!(
                    "the footer's body length {} disagrees with the file size {size}",
                    footer.body_bytes
                ),
            ));
        }
        let interval_inverted = footer.last_sequence < header.first_sequence;
        if footer.batch_count == 0
            || interval_inverted
            || footer.batch_count > footer.last_sequence - header.first_sequence + 1
            || footer.last_observed_at < footer.first_observed_at
        {
            return Err(refuse(
                path,
                format!(
                    "the footer's interval is inconsistent: {} write groups for sequences {}..{}",
                    footer.batch_count, header.first_sequence, footer.last_sequence
                ),
            ));
        }
        if let Some((first, last)) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_segment_name)
        {
            if (first, last) != (header.first_sequence, footer.last_sequence) {
                return Err(refuse(
                    path,
                    format!(
                        "the name says sequences {first}..{last}, the contents {}..{}",
                        header.first_sequence, footer.last_sequence
                    ),
                ));
            }
        }
        if let Some((history, epoch)) = expected {
            if header.history != history {
                return Err(refuse(
                    path,
                    format!(
                        "other history: it is filed under history {history}, its header says {}",
                        header.history
                    ),
                ));
            }
            if header.epoch != epoch {
                return Err(refuse(
                    path,
                    format!(
                        "other epoch: it is filed under epoch {epoch}, its header says {}",
                        header.epoch
                    ),
                ));
            }
        }
        Ok(SegmentInfo {
            path: path.to_path_buf(),
            header,
            footer,
            file_bytes: size,
            sealed_at: modified_secs(&metadata),
        })
    }
}

/// The segment being written. Every frame reaches the file with one write,
/// so a crash (a SIGKILL included) loses at most the frame in flight;
/// [`repair_partial`] seals the rest. The frame data is fsynced by
/// [`Self::sync`] and at the seal.
pub struct OpenSegment {
    dir: PathBuf,
    path: PathBuf,
    file: File,
    header: SegmentHeader,
    hasher: Sha256,
    body_bytes: u64,
    last_sequence: u64,
    batch_count: u64,
    first_observed_at: i64,
    last_observed_at: i64,
    unsynced_bytes: u64,
    failed: bool,
}

impl OpenSegment {
    /// Create `<dir>/<first>-open.hseg.partial` (0600; `dir` is created
    /// 0700) and write `header` to it.
    pub fn create(dir: &Path, header: SegmentHeader) -> SegmentResult<Self> {
        create_private_dir(dir).map_err(|error| io_refuse(dir, "cannot create the segment directory", &error))?;
        let path = dir.join(partial_file_name(header.first_sequence));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| io_refuse(&path, "cannot create the open segment", &error))?;
        let bytes = header.encode();
        if let Err(error) = file.write_all(&bytes) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(io_refuse(&path, "cannot write the header", &error));
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let (first_sequence, created_at) = (header.first_sequence, header.created_at);
        Ok(Self {
            dir: dir.to_path_buf(),
            path,
            file,
            header,
            hasher,
            body_bytes: 0,
            last_sequence: first_sequence.saturating_sub(1),
            batch_count: 0,
            first_observed_at: created_at,
            last_observed_at: created_at,
            unsynced_bytes: 0,
            failed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    pub fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    pub fn batch_count(&self) -> u64 {
        self.batch_count
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn unsynced_bytes(&self) -> u64 {
        self.unsynced_bytes
    }

    /// Whether a write or fsync failed: the file's tail is uncertain, so the
    /// segment must be set aside ([`repair_partial`]) rather than continued.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The bytes `batch` adds to a segment: its length word and payload.
    pub fn frame_bytes(batch: &PhysicalBatch, sealed: bool) -> u64 {
        let seal = if sealed {
            crate::crypto::MIN_CIPHERTEXT_LEN as u64
        } else {
            0
        };
        4 + FRAME_META as u64 + batch.data().len() as u64 + seal
    }

    /// Append `batch` as one frame (sealed with `key` when the segment is).
    /// It must belong to the segment's history and continue its interval.
    pub fn append(&mut self, batch: &PhysicalBatch, observed_at: i64, key: Option<&KeyManager>) -> SegmentResult<u64> {
        if self.failed {
            return Err(refuse(
                &self.path,
                "an earlier write to this open segment failed; it must be set aside",
            ));
        }
        if batch.history() != self.header.history {
            return Err(refuse(
                &self.path,
                format!(
                    "other history: write group {} belongs to history {}, the segment to {}",
                    batch.first_sequence(),
                    batch.history(),
                    self.header.history
                ),
            ));
        }
        let expected = if self.batch_count == 0 {
            self.header.first_sequence
        } else {
            self.last_sequence.saturating_add(1)
        };
        if batch.first_sequence() != expected {
            return Err(refuse(
                &self.path,
                format!(
                    "noncontiguous frame: expected sequence {expected}, the write group starts at {}",
                    batch.first_sequence()
                ),
            ));
        }
        // A write group may be gigabytes (spec P4.1c): it is never joined with
        // its length word into one buffer. Unsealed, the frame is written from
        // the batch itself (metadata, then the raw group); sealed, the one
        // plaintext copy is consumed by the sealing, so it is freed before the
        // sealed payload is written.
        let sealed = if self.header.sealed() {
            let key = key.ok_or_else(|| refuse(&self.path, SEALED_WITHOUT_KEY))?;
            Some(
                crate::storage::tde::seal_owned(Some(key), encode_batch_payload(batch))
                    .map_err(|error| refuse(&self.path, format!("cannot seal the frame: {error}")))?,
            )
        } else {
            None
        };
        let meta = batch_meta(batch);
        let parts: [&[u8]; 2] = match &sealed {
            Some(payload) => [payload.as_slice(), &[]],
            None => [&meta, batch.data()],
        };
        let payload_len: usize = parts.iter().map(|part| part.len()).sum();
        if payload_len > MAX_FRAME_LEN {
            return Err(refuse(
                &self.path,
                format!("a frame of {payload_len} bytes exceeds the format's limit"),
            ));
        }
        let length = u32::try_from(payload_len)
            .map_err(|_| refuse(&self.path, "a frame longer than 4 GiB cannot be archived"))?;
        let word = length.to_be_bytes();
        let written = self
            .file
            .write_all(&word)
            .and_then(|()| parts.iter().try_for_each(|part| self.file.write_all(part)));
        if let Err(error) = written {
            self.failed = true;
            return Err(io_refuse(&self.path, "cannot append a frame", &error));
        }
        self.hasher.update(word);
        for part in parts {
            self.hasher.update(part);
        }
        let added = (word.len() + payload_len) as u64;
        self.body_bytes += added;
        self.unsynced_bytes += added;
        if self.batch_count == 0 {
            self.first_observed_at = observed_at;
            self.last_observed_at = observed_at;
        }
        self.batch_count += 1;
        self.last_sequence = batch.last_sequence();
        self.last_observed_at = self.last_observed_at.max(observed_at);
        Ok(added)
    }

    /// fsync the frames appended since the last fsync.
    pub fn sync(&mut self) -> SegmentResult<()> {
        if self.unsynced_bytes == 0 {
            return Ok(());
        }
        match self.file.sync_data() {
            Ok(()) => {
                self.unsynced_bytes = 0;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(io_refuse(&self.path, "cannot fsync the open segment", &error))
            }
        }
    }

    /// Seal: footer, fsync, rename to `<first>-<last>.hseg`, directory fsync.
    /// An empty segment is never sealed. On an error the partial file stays
    /// where it was, for [`repair_partial`].
    pub fn seal(mut self) -> SegmentResult<SegmentInfo> {
        if self.failed {
            return Err(refuse(
                &self.path,
                "an earlier write to this open segment failed; it must be set aside",
            ));
        }
        if self.batch_count == 0 {
            return Err(refuse(&self.path, "an empty segment is never sealed"));
        }
        let target = self
            .dir
            .join(segment_file_name(self.header.first_sequence, self.last_sequence));
        if target.exists() {
            return Err(refuse(&target, "a sealed segment already holds this interval"));
        }
        let footer = SegmentFooter {
            last_sequence: self.last_sequence,
            batch_count: self.batch_count,
            first_observed_at: self.first_observed_at,
            last_observed_at: self.last_observed_at,
            body_bytes: self.body_bytes,
            file_sha256: self.hasher.clone().finalize().into(),
        };
        self.file
            .write_all(&footer.encode())
            .map_err(|error| io_refuse(&self.path, "cannot write the footer", &error))?;
        self.file
            .sync_all()
            .map_err(|error| io_refuse(&self.path, "cannot fsync the segment", &error))?;
        std::fs::rename(&self.path, &target)
            .map_err(|error| io_refuse(&self.path, "cannot rename the sealed segment", &error))?;
        sync_dir(&self.dir).map_err(|error| io_refuse(&self.dir, "cannot fsync the segment directory", &error))?;
        let file_bytes = HEADER_LEN as u64 + self.body_bytes + FOOTER_LEN as u64;
        Ok(SegmentInfo {
            path: target,
            header: self.header,
            footer,
            file_bytes,
            sealed_at: Some(unix_now()),
        })
    }
}

fn remove_and_sync(path: &Path) -> SegmentResult<()> {
    std::fs::remove_file(path).map_err(|error| io_refuse(path, "cannot remove the empty open segment", &error))?;
    if let Some(dir) = path.parent() {
        let _ = sync_dir(dir);
    }
    Ok(())
}

/// Recover an open segment left by a crash: keep every frame up to the first
/// truncated or unverifiable one, cut the file there, write the footer and
/// seal it. Nothing valid is lost — every frame carries its own digest.
///
/// `Ok(None)`: the partial held no complete frame (or not even a header) and
/// was removed. `Err`: its header cannot be trusted, or it is sealed and no
/// dataset key was given; the file is left as it is.
pub fn repair_partial(path: &Path, key: Option<&KeyManager>) -> SegmentResult<Option<SegmentInfo>> {
    let metadata = std::fs::metadata(path).map_err(|error| io_refuse(path, "cannot inspect", &error))?;
    if metadata.len() < HEADER_LEN as u64 {
        remove_and_sync(path)?;
        return Ok(None);
    }
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| io_refuse(path, "cannot open", &error))?;
    let mut header_bytes = vec![0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)
        .map_err(|error| io_refuse(path, "cannot read the header", &error))?;
    let header = SegmentHeader::decode(&header_bytes)
        .map_err(|reason| refuse(path, format!("open segment header: {reason}")))?;
    if let Some(first) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(parse_partial_name)
    {
        if first != header.first_sequence {
            return Err(refuse(
                path,
                format!("the name says sequence {first}, the header {}", header.first_sequence),
            ));
        }
    }
    if header.sealed() && key.is_none() {
        return Err(refuse(path, format!("{SEALED_WITHOUT_KEY} to repair it")));
    }
    let mut hasher = Sha256::new();
    hasher.update(&header_bytes);
    let mut body = 0_u64;
    let mut count = 0_u64;
    let mut next = header.first_sequence;
    let mut last = None;
    {
        let mut available = metadata.len() - HEADER_LEN as u64;
        let mut reader = BufReader::new(&file);
        loop {
            let mut length = [0_u8; 4];
            if available < 4 || reader.read_exact(&mut length).is_err() {
                break;
            }
            let size = u32::from_be_bytes(length);
            if size == 0 || u64::from(size) > available - 4 || u64::from(size) > MAX_FRAME_LEN as u64 {
                break;
            }
            let mut payload = vec![0_u8; usize::try_from(size).unwrap_or(0)];
            if reader.read_exact(&mut payload).is_err() {
                break;
            }
            let mut candidate = hasher.clone();
            candidate.update(length);
            candidate.update(&payload);
            let Ok(batch) = open_frame(path, payload, &header, key, next) else {
                break;
            };
            hasher = candidate;
            body += 4 + u64::from(size);
            available -= 4 + u64::from(size);
            count += 1;
            next = batch.last_sequence().saturating_add(1);
            last = Some(batch.last_sequence());
        }
    }
    let Some(last) = last else {
        drop(file);
        remove_and_sync(path)?;
        return Ok(None);
    };
    let end = HEADER_LEN as u64 + body;
    // The archiver's clock for the last frame is not in the file: the file's
    // last write is the closest record of when it was observed.
    let observed_end = modified_secs(&metadata).unwrap_or_else(unix_now).max(header.created_at);
    let footer = SegmentFooter {
        last_sequence: last,
        batch_count: count,
        first_observed_at: header.created_at,
        last_observed_at: observed_end,
        body_bytes: body,
        file_sha256: hasher.finalize().into(),
    };
    file.set_len(end)
        .map_err(|error| io_refuse(path, "cannot cut the open segment after its last intact frame", &error))?;
    file.seek(SeekFrom::Start(end))
        .map_err(|error| io_refuse(path, "cannot seek", &error))?;
    file.write_all(&footer.encode())
        .map_err(|error| io_refuse(path, "cannot write the footer", &error))?;
    file.sync_all()
        .map_err(|error| io_refuse(path, "cannot fsync the repaired segment", &error))?;
    drop(file);
    let target = dir.join(segment_file_name(header.first_sequence, last));
    if target.exists() {
        return Err(refuse(&target, "a sealed segment already holds this interval"));
    }
    std::fs::rename(path, &target).map_err(|error| io_refuse(path, "cannot rename the repaired segment", &error))?;
    sync_dir(&dir).map_err(|error| io_refuse(&dir, "cannot fsync the segment directory", &error))?;
    Ok(Some(SegmentInfo {
        path: target,
        header,
        footer,
        file_bytes: end + FOOTER_LEN as u64,
        sealed_at: Some(unix_now()),
    }))
}

/// Rename a file that cannot be repaired to `<name>.corrupt` (a timestamp is
/// added when that name is taken), so its name is free again; returns where
/// it went. It is listed as corrupt and never pruned: the operator's call.
pub fn set_aside_corrupt(path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut target = path.with_file_name(format!("{name}{CORRUPT_SUFFIX}"));
    if target.exists() {
        target = path.with_file_name(format!("{name}.{}{CORRUPT_SUFFIX}", unix_now()));
    }
    std::fs::rename(path, &target)?;
    if let Some(dir) = path.parent() {
        let _ = sync_dir(dir);
    }
    Ok(target)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use super::*;
    use crate::replication::physical::{poll_source, FlushMemo};
    use tempfile::TempDir;

    /// `count` write groups of a fresh RocksDB store, as the archiver would
    /// see them (`poll_source`), each with `puts` puts.
    pub fn batches(count: usize, puts: usize) -> (Uuid, Vec<PhysicalBatch>) {
        let root = TempDir::new().unwrap();
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        let db = rocksdb::DB::open(&options, root.path().join("source")).unwrap();
        for group in 0..count {
            let mut batch = rocksdb::WriteBatch::default();
            for put in 0..puts {
                batch.put(
                    format!("data:t:{group}:{put}"),
                    vec![u8::try_from(group % 251).unwrap(); 64],
                );
            }
            db.write(batch).unwrap();
        }
        let history = Uuid::new_v4();
        let polled = poll_source(&db, history, 0, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), count);
        (history, polled.batches)
    }

    fn header(history: Uuid, first: u64, sealed: bool) -> SegmentHeader {
        SegmentHeader {
            flags: if sealed { FLAG_SEALED } else { 0 },
            history,
            epoch: 1,
            first_sequence: first,
            created_at: unix_now(),
            node_id: Uuid::new_v4(),
            key_check: if sealed { [7; 32] } else { [0; 32] },
        }
    }

    fn write_segment(dir: &Path, batches: &[PhysicalBatch], key: Option<&KeyManager>) -> SegmentInfo {
        let mut open = OpenSegment::create(
            dir,
            header(batches[0].history(), batches[0].first_sequence(), key.is_some()),
        )
        .unwrap();
        for batch in batches {
            open.append(batch, unix_now(), key).unwrap();
        }
        open.seal().unwrap()
    }

    fn payloads(batches: impl IntoIterator<Item = PhysicalBatch>) -> Vec<Vec<u8>> {
        batches.into_iter().map(|batch| encode_batch_payload(&batch)).collect()
    }

    /// Assemble a segment from raw frame payloads with correct hashes, so a
    /// test can plant a frame the writer would refuse.
    fn assemble(path: &Path, header: &SegmentHeader, frames: &[Vec<u8>], last: u64) {
        let mut bytes = header.encode();
        let mut body = 0_u64;
        for payload in frames {
            bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
            bytes.extend_from_slice(payload);
            body += 4 + payload.len() as u64;
        }
        let footer = SegmentFooter {
            last_sequence: last,
            batch_count: frames.len() as u64,
            first_observed_at: header.created_at,
            last_observed_at: header.created_at,
            body_bytes: body,
            file_sha256: sha256(&bytes),
        };
        bytes.extend_from_slice(&footer.encode());
        std::fs::write(path, bytes).unwrap();
    }

    fn flip(path: &Path, offset: u64) {
        let mut bytes = std::fs::read(path).unwrap();
        let index = usize::try_from(offset).unwrap();
        bytes[index] ^= 0x5A;
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_plain_segment_round_trips_and_its_frames_are_the_wire_payloads() {
        let dir = TempDir::new().unwrap();
        let (history, batches) = batches(5, 3);
        let sealed = write_segment(dir.path(), &batches, None);
        let name = sealed.path.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(
            name,
            segment_file_name(batches[0].first_sequence(), batches[4].last_sequence())
        );
        assert!(!dir.path().join(partial_file_name(batches[0].first_sequence())).exists());
        let info = SegmentReader::open_expecting(&sealed.path, Some((history, 1))).unwrap();
        assert_eq!(info.history(), history);
        assert_eq!(info.first_sequence(), batches[0].first_sequence());
        assert_eq!(info.last_sequence(), batches[4].last_sequence());
        assert_eq!(info.batch_count(), 5);
        assert!(!info.sealed_frames());
        assert_eq!(info.file_bytes, std::fs::metadata(&info.path).unwrap().len());
        assert_eq!(info.footer, sealed.footer);
        info.verify_deep(None).unwrap();
        let read: Vec<PhysicalBatch> = info.frames(None).unwrap().map(Result::unwrap).collect();
        assert_eq!(payloads(read), payloads(batches));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&info.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// Spec P3.1 §2: an encrypted dataset's frames are sealed whole; header
    /// and footer verify without the key, the frames need it, and a wrong key
    /// is refused by name.
    #[test]
    fn a_sealed_segment_needs_the_dataset_key_for_its_frames_only() {
        let dir = TempDir::new().unwrap();
        let (_, batches) = batches(4, 2);
        let key = KeyManager::generate_random();
        let sealed = write_segment(dir.path(), &batches, Some(&key));
        let info = SegmentReader::open(&sealed.path).unwrap();
        assert!(info.sealed_frames());
        let file = std::fs::read(&info.path).unwrap();
        let raw = batches[2].data();
        assert!(
            !file.windows(raw.len()).any(|window| window == raw),
            "a sealed segment carries no raw batch in the clear"
        );
        for error in [info.verify_deep(None).unwrap_err(), info.frames(None).err().unwrap()] {
            assert!(error.reason.contains(SEALED_WITHOUT_KEY), "{error}");
            assert!(error.to_string().starts_with("segment "), "{error}");
        }
        info.verify_deep(Some(&key)).unwrap();
        let read: Vec<PhysicalBatch> = info.frames(Some(&key)).unwrap().map(Result::unwrap).collect();
        assert_eq!(payloads(read), payloads(batches));
        let other = KeyManager::generate_random();
        let error = info.verify_deep(Some(&other)).unwrap_err();
        assert!(error.reason.contains("does not open with this dataset key"), "{error}");
    }

    /// Spec P3.1 §6: every corruption class is detected and named, with the
    /// file.
    #[test]
    fn every_corruption_class_is_detected_and_named() {
        let dir = TempDir::new().unwrap();
        let (history, batches) = batches(3, 2);
        let pristine = write_segment(dir.path(), &batches, None);
        let copy = |name: &str| {
            let path = dir.path().join(name);
            std::fs::copy(&pristine.path, &path).unwrap();
            path
        };
        let size = pristine.file_bytes;
        let opened = |path: &Path| SegmentReader::open(path).unwrap_err();

        let path = copy("magic.hseg");
        flip(&path, 0);
        assert!(opened(&path).reason.contains("bad magic"));

        let path = copy("header.hseg");
        flip(&path, 40);
        let error = opened(&path);
        assert!(error.reason.contains("header hash"), "{error}");
        assert!(error.to_string().contains("header.hseg"), "{error}");

        let path = copy("footer.hseg");
        flip(&path, size - 60);
        assert!(opened(&path).reason.contains("footer hash"));

        let path = copy("body.hseg");
        flip(&path, HEADER_LEN as u64 + 30);
        let info = SegmentReader::open(&path).expect("header and footer are intact");
        let error = info.verify_deep(None).unwrap_err();
        assert!(error.reason.contains("file hash"), "{error}");

        let path = copy("truncated.hseg");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
        let error = opened(&path);
        assert!(error.reason.contains("footer"), "{error}");

        // A frame whose digest does not match, in a file whose hashes do.
        let mut frames = payloads(batches.clone());
        frames[1][30] ^= 0xFF;
        let path = dir.path().join("digest.hseg");
        assemble(
            &path,
            &header(history, batches[0].first_sequence(), false),
            &frames,
            batches[2].last_sequence(),
        );
        let error = SegmentReader::open(&path).unwrap().verify_deep(None).unwrap_err();
        assert!(error.reason.contains("frame digest"), "{error}");

        // A frame that does not continue the previous one.
        let frames = payloads([batches[0].clone(), batches[2].clone()]);
        let path = dir.path().join("gap.hseg");
        assemble(
            &path,
            &header(history, batches[0].first_sequence(), false),
            &frames,
            batches[2].last_sequence(),
        );
        let error = SegmentReader::open(&path).unwrap().verify_deep(None).unwrap_err();
        assert!(error.reason.contains("noncontiguous frame"), "{error}");

        // Filed under another history or epoch.
        let error = SegmentReader::open_expecting(&pristine.path, Some((Uuid::new_v4(), 1))).unwrap_err();
        assert!(error.reason.contains("other history"), "{error}");
        let error = SegmentReader::open_expecting(&pristine.path, Some((history, 2))).unwrap_err();
        assert!(error.reason.contains("other epoch"), "{error}");
    }

    /// Spec P3.1 §2: the repair of an open segment keeps every intact frame,
    /// cuts a torn tail, and seals; an empty partial is removed.
    #[test]
    fn repair_keeps_every_intact_frame_and_seals() {
        let dir = TempDir::new().unwrap();
        let (_, batches) = batches(6, 2);
        let mut open = OpenSegment::create(
            dir.path(),
            header(batches[0].history(), batches[0].first_sequence(), false),
        )
        .unwrap();
        for batch in &batches[..5] {
            open.append(batch, unix_now(), None).unwrap();
        }
        let partial = open.path().to_path_buf();
        drop(open);
        // A torn sixth frame: its length and half its payload.
        let torn = encode_batch_payload(&batches[5]);
        let mut file = OpenOptions::new().append(true).open(&partial).unwrap();
        file.write_all(&u32::try_from(torn.len()).unwrap().to_be_bytes())
            .unwrap();
        file.write_all(&torn[..torn.len() / 2]).unwrap();
        drop(file);

        let info = repair_partial(&partial, None).unwrap().expect("five frames kept");
        assert!(!partial.exists());
        assert_eq!(info.batch_count(), 5);
        assert_eq!(info.last_sequence(), batches[4].last_sequence());
        let reopened = SegmentReader::open(&info.path).unwrap();
        reopened.verify_deep(None).unwrap();
        let read: Vec<PhysicalBatch> = reopened.frames(None).unwrap().map(Result::unwrap).collect();
        assert_eq!(payloads(read), payloads(batches[..5].to_vec()));

        // A partial with a header and no frame, and one torn inside its
        // header, hold nothing: removed.
        let empty = OpenSegment::create(dir.path(), header(batches[0].history(), 9_000, false)).unwrap();
        let empty_path = empty.path().to_path_buf();
        drop(empty);
        assert_eq!(repair_partial(&empty_path, None).unwrap(), None);
        assert!(!empty_path.exists());
        let torn_header = dir.path().join(partial_file_name(9_500));
        std::fs::write(&torn_header, &header(batches[0].history(), 9_500, false).encode()[..40]).unwrap();
        assert_eq!(repair_partial(&torn_header, None).unwrap(), None);
        assert!(!torn_header.exists());
    }

    #[test]
    fn segment_names_sort_in_sequence_order_and_parse_back() {
        let intervals = [(1_u64, 9_u64), (10, 99), (100, 1_000_000), (1_000_001, u64::MAX)];
        let mut names: Vec<String> = intervals
            .iter()
            .rev()
            .map(|(first, last)| segment_file_name(*first, *last))
            .collect();
        names.sort();
        let parsed: Vec<(u64, u64)> = names.iter().map(|name| parse_segment_name(name).unwrap()).collect();
        assert_eq!(parsed, intervals);
        assert_eq!(parse_partial_name(&partial_file_name(42)), Some(42));
        for bad in [
            "1-2.hseg",
            "00000000000000000001-x.hseg",
            "notes.txt",
            "00000000000000000001-open.hseg",
        ] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    /// The frame payload is the replication wire's `Batch` payload, so the
    /// existing decode verifies it.
    #[test]
    fn a_frame_payload_round_trips_through_the_wire_decode() {
        let (_, batches) = batches(1, 4);
        let payload = encode_batch_payload(&batches[0]);
        assert_eq!(payload.len(), FRAME_META + batches[0].data().len());
        let decoded = decode_batch_payload(payload).unwrap();
        assert_eq!(decoded.digest(), batches[0].digest());
        assert_eq!(decoded.data(), batches[0].data());
    }
}
