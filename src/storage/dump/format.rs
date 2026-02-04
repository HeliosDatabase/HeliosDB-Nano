//! Dump file format definitions
//!
//! Defines the structure and serialization of HeliosDB dump files.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use crate::{Result, Error};

/// Magic number for dump file identification: "HELIODMP"
pub const DUMP_MAGIC_NUMBER: &[u8; 8] = b"HELIODMP";

/// Current dump format version
pub const DUMP_VERSION: u32 = 1;

/// Compression type for dump files
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    /// No compression
    None,
    /// Zstandard compression (default)
    Zstd,
    /// Gzip compression
    Gzip,
    /// Brotli compression
    Brotli,
}

impl CompressionType {
    /// Parse compression type from string
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "zstd" => Ok(Self::Zstd),
            "gzip" | "gz" => Ok(Self::Gzip),
            "brotli" | "br" => Ok(Self::Brotli),
            _ => Err(Error::config(format!("Unknown compression type: {}", s))),
        }
    }

    /// Get file extension for this compression type
    pub fn extension(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Zstd => ".zst",
            Self::Gzip => ".gz",
            Self::Brotli => ".br",
        }
    }
}

/// Dump metadata stored at the beginning of dump files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpMetadata {
    /// Unique dump identifier
    pub dump_id: String,
    /// Database version
    pub version: String,
    /// Timestamp of dump creation (Unix timestamp)
    pub created_at: u64,
    /// LSN (Log Sequence Number) at time of dump
    pub lsn: u64,
    /// Compression type used
    pub compression: CompressionType,
    /// Total number of tables
    pub table_count: usize,
    /// Total number of rows
    pub row_count: u64,
    /// Total bytes (uncompressed)
    pub bytes_uncompressed: u64,
    /// Total bytes (compressed)
    pub bytes_compressed: u64,
    /// Whether this is an incremental dump
    pub incremental: bool,
    /// Previous dump LSN (for incremental dumps)
    pub previous_lsn: Option<u64>,
}

impl DumpMetadata {
    /// Create a new dump metadata
    pub fn new(lsn: u64, compression: CompressionType, incremental: bool) -> Self {
        Self {
            dump_id: uuid::Uuid::new_v4().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            lsn,
            compression,
            table_count: 0,
            row_count: 0,
            bytes_uncompressed: 0,
            bytes_compressed: 0,
            incremental,
            previous_lsn: None,
        }
    }

    /// Write metadata to writer
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        // Write magic number
        writer.write_all(DUMP_MAGIC_NUMBER)
            .map_err(|e| Error::io(format!("Failed to write magic number: {}", e)))?;

        // Write version
        writer.write_all(&DUMP_VERSION.to_le_bytes())
            .map_err(|e| Error::io(format!("Failed to write version: {}", e)))?;

        // Serialize metadata as JSON
        let json = serde_json::to_vec(self)
            .map_err(|e| Error::io(format!("Failed to serialize metadata: {}", e)))?;

        // Write metadata length
        let len = json.len() as u32;
        writer.write_all(&len.to_le_bytes())
            .map_err(|e| Error::io(format!("Failed to write metadata length: {}", e)))?;

        // Write metadata
        writer.write_all(&json)
            .map_err(|e| Error::io(format!("Failed to write metadata: {}", e)))?;

        Ok(())
    }

    /// Read metadata from reader
    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        // Read and verify magic number
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)
            .map_err(|e| Error::io(format!("Failed to read magic number: {}", e)))?;

        if &magic != DUMP_MAGIC_NUMBER {
            return Err(Error::io("Invalid dump file: magic number mismatch"));
        }

        // Read version
        let mut version_bytes = [0u8; 4];
        reader.read_exact(&mut version_bytes)
            .map_err(|e| Error::io(format!("Failed to read version: {}", e)))?;
        let version = u32::from_le_bytes(version_bytes);

        if version != DUMP_VERSION {
            return Err(Error::io(format!(
                "Incompatible dump version: expected {}, found {}",
                DUMP_VERSION, version
            )));
        }

        // Read metadata length
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)
            .map_err(|e| Error::io(format!("Failed to read metadata length: {}", e)))?;
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Read metadata
        let mut json = vec![0u8; len];
        reader.read_exact(&mut json)
            .map_err(|e| Error::io(format!("Failed to read metadata: {}", e)))?;

        // Deserialize metadata
        let metadata: DumpMetadata = serde_json::from_slice(&json)
            .map_err(|e| Error::io(format!("Failed to deserialize metadata: {}", e)))?;

        Ok(metadata)
    }
}

/// Dump file format handler
pub struct DumpFormat;

impl DumpFormat {
    /// Compress data using the specified compression type
    pub fn compress(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
        match compression {
            CompressionType::None => Ok(data.to_vec()),
            CompressionType::Zstd => {
                zstd::encode_all(data, 3)
                    .map_err(|e| Error::io(format!("Zstd compression failed: {}", e)))
            }
            CompressionType::Gzip => {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(data)
                    .map_err(|e| Error::io(format!("Gzip compression failed: {}", e)))?;
                encoder.finish()
                    .map_err(|e| Error::io(format!("Gzip compression failed: {}", e)))
            }
            CompressionType::Brotli => {
                use brotli::enc::BrotliEncoderParams;
                let mut output = Vec::new();
                let params = BrotliEncoderParams::default();
                brotli::BrotliCompress(
                    &mut std::io::Cursor::new(data),
                    &mut output,
                    &params,
                ).map_err(|e| Error::io(format!("Brotli compression failed: {}", e)))?;
                Ok(output)
            }
        }
    }

    /// Decompress data using the specified compression type
    pub fn decompress(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
        match compression {
            CompressionType::None => Ok(data.to_vec()),
            CompressionType::Zstd => {
                zstd::decode_all(data)
                    .map_err(|e| Error::io(format!("Zstd decompression failed: {}", e)))
            }
            CompressionType::Gzip => {
                use flate2::read::GzDecoder;
                let mut decoder = GzDecoder::new(data);
                let mut output = Vec::new();
                decoder.read_to_end(&mut output)
                    .map_err(|e| Error::io(format!("Gzip decompression failed: {}", e)))?;
                Ok(output)
            }
            CompressionType::Brotli => {
                use brotli::BrotliDecompress;
                let mut output = Vec::new();
                BrotliDecompress(
                    &mut std::io::Cursor::new(data),
                    &mut output,
                ).map_err(|e| Error::io(format!("Brotli decompression failed: {}", e)))?;
                Ok(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_serialization() {
        let metadata = DumpMetadata::new(100, CompressionType::Zstd, false);
        let mut buffer = Vec::new();
        metadata.write_to(&mut buffer).unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let deserialized = DumpMetadata::read_from(&mut cursor).unwrap();

        assert_eq!(metadata.lsn, deserialized.lsn);
        assert_eq!(metadata.compression, deserialized.compression);
        assert_eq!(metadata.incremental, deserialized.incremental);
    }

    #[test]
    fn test_compression_roundtrip() {
        let data = b"Hello, World! This is test data for compression.";

        for compression in [
            CompressionType::None,
            CompressionType::Zstd,
            CompressionType::Gzip,
            CompressionType::Brotli,
        ] {
            let compressed = DumpFormat::compress(data, compression).unwrap();
            let decompressed = DumpFormat::decompress(&compressed, compression).unwrap();
            assert_eq!(data.as_ref(), decompressed.as_slice());
        }
    }
}
