//! AFPK pack file format — groups small blobs into larger S3 objects.
//!
//! Format:
//! ```text
//! [magic: "AFPK" 4B][version: u32 LE][entry_count: u32 LE]
//! [entries...]:
//!   [oid: 20B raw SHA-1][comp_size: u32 LE][raw_size: u32 LE][zstd compressed blob]
//! [footer: SHA-256 of everything above, 32B]
//! ```

use std::io::Write;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"AFPK";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 4 + 4 + 4; // magic + version + count
pub const ENTRY_HEADER_SIZE: usize = 20 + 4 + 4; // oid + comp_size + raw_size
const FOOTER_SIZE: usize = 32; // SHA-256

/// A single entry in a pack file.
#[derive(Debug, Clone)]
pub struct PackEntryData {
    /// Git object OID (hex string, 40 chars).
    pub oid: String,
    /// Raw (uncompressed) blob data.
    pub data: Vec<u8>,
}

/// Index entry describing where a blob lives inside a pack.
#[derive(Debug, Clone)]
pub struct PackIndexEntry {
    pub oid: String,
    /// Byte offset from start of pack file to this entry's header.
    pub offset: u64,
    /// Size of the zstd-compressed data.
    pub comp_size: u32,
    /// Size of the original uncompressed data.
    pub raw_size: u32,
}

/// Build a pack file from a list of blobs. Returns (pack_bytes, index_entries).
pub fn write_pack(entries: &[PackEntryData]) -> Result<(Vec<u8>, Vec<PackIndexEntry>)> {
    let mut buf = Vec::new();
    let mut index = Vec::new();

    // Header
    buf.write_all(MAGIC)?;
    buf.write_all(&VERSION.to_le_bytes())?;
    buf.write_all(&(entries.len() as u32).to_le_bytes())?;

    for entry in entries {
        let oid_bytes = hex_to_bytes20(&entry.oid)?;
        let compressed = zstd::encode_all(entry.data.as_slice(), 3)
            .context("zstd compress")?;

        let offset = buf.len() as u64;
        let comp_size = compressed.len() as u32;
        let raw_size = entry.data.len() as u32;

        // Entry header
        buf.write_all(&oid_bytes)?;
        buf.write_all(&comp_size.to_le_bytes())?;
        buf.write_all(&raw_size.to_le_bytes())?;
        // Compressed data
        buf.write_all(&compressed)?;

        index.push(PackIndexEntry {
            oid: entry.oid.clone(),
            offset,
            comp_size,
            raw_size,
        });
    }

    // Footer: SHA-256 of everything so far
    let hash = Sha256::digest(&buf);
    buf.write_all(&hash)?;

    Ok((buf, index))
}

/// Read a single blob from pack data given its offset and compressed size.
pub fn read_blob_from_pack(pack_data: &[u8], offset: u64, comp_size: u32) -> Result<Vec<u8>> {
    let start = offset as usize;
    if start + ENTRY_HEADER_SIZE > pack_data.len() {
        bail!("offset {} out of range (pack size {})", offset, pack_data.len());
    }

    // Skip entry header (oid + comp_size + raw_size)
    let data_start = start + ENTRY_HEADER_SIZE;
    let data_end = data_start + comp_size as usize;
    if data_end > pack_data.len() - FOOTER_SIZE {
        bail!("compressed data extends past pack boundary");
    }

    let compressed = &pack_data[data_start..data_end];
    let decompressed = zstd::decode_all(compressed)
        .context("zstd decompress")?;

    Ok(decompressed)
}

/// Read a single blob using only a byte range (for S3 range reads).
/// The `range_data` should contain exactly the entry header + compressed data.
pub fn read_blob_from_range(range_data: &[u8], comp_size: u32) -> Result<Vec<u8>> {
    if range_data.len() < ENTRY_HEADER_SIZE + comp_size as usize {
        bail!("range data too short");
    }

    let compressed = &range_data[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + comp_size as usize];
    let decompressed = zstd::decode_all(compressed)
        .context("zstd decompress from range")?;

    Ok(decompressed)
}

/// Verify pack integrity by checking the SHA-256 footer.
pub fn verify_pack(pack_data: &[u8]) -> Result<bool> {
    if pack_data.len() < HEADER_SIZE + FOOTER_SIZE {
        return Ok(false);
    }

    let content = &pack_data[..pack_data.len() - FOOTER_SIZE];
    let stored_hash = &pack_data[pack_data.len() - FOOTER_SIZE..];
    let computed = Sha256::digest(content);

    Ok(computed.as_slice() == stored_hash)
}

/// Parse the pack header and return entry count.
pub fn read_pack_header(data: &[u8]) -> Result<u32> {
    if data.len() < HEADER_SIZE {
        bail!("pack too short for header");
    }
    if &data[..4] != MAGIC {
        bail!("invalid pack magic");
    }
    let version = u32::from_le_bytes(data[4..8].try_into()?);
    if version != VERSION {
        bail!("unsupported pack version {}", version);
    }
    let count = u32::from_le_bytes(data[8..12].try_into()?);
    Ok(count)
}

/// Parse a full pack file and return all index entries.
pub fn parse_pack_index(pack_data: &[u8]) -> Result<Vec<PackIndexEntry>> {
    let count = read_pack_header(pack_data)?;
    let mut entries = Vec::with_capacity(count as usize);
    let mut pos = HEADER_SIZE;

    for _ in 0..count {
        if pos + ENTRY_HEADER_SIZE > pack_data.len() - FOOTER_SIZE {
            bail!("pack truncated");
        }

        let oid = bytes20_to_hex(&pack_data[pos..pos + 20]);
        let comp_size = u32::from_le_bytes(pack_data[pos + 20..pos + 24].try_into()?);
        let raw_size = u32::from_le_bytes(pack_data[pos + 24..pos + 28].try_into()?);

        entries.push(PackIndexEntry {
            oid,
            offset: pos as u64,
            comp_size,
            raw_size,
        });

        pos += ENTRY_HEADER_SIZE + comp_size as usize;
    }

    Ok(entries)
}

/// Default size threshold: blobs smaller than this go into packs.
pub const DEFAULT_PACK_THRESHOLD: usize = 256 * 1024; // 256KB

/// Default target pack file size.
pub const DEFAULT_TARGET_PACK_SIZE: usize = 64 * 1024 * 1024; // 64MB

/// Pack configuration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PackConfig {
    /// Blobs smaller than this (in bytes) are grouped into packs.
    #[serde(default = "default_pack_threshold")]
    pub pack_threshold: usize,
    /// Target size for a single pack file (in bytes).
    #[serde(default = "default_target_pack_size")]
    pub target_pack_size: usize,
}

fn default_pack_threshold() -> usize {
    DEFAULT_PACK_THRESHOLD
}

fn default_target_pack_size() -> usize {
    DEFAULT_TARGET_PACK_SIZE
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            pack_threshold: DEFAULT_PACK_THRESHOLD,
            target_pack_size: DEFAULT_TARGET_PACK_SIZE,
        }
    }
}

// Keep backwards-compatible aliases
pub const PACK_THRESHOLD: usize = DEFAULT_PACK_THRESHOLD;
pub const TARGET_PACK_SIZE: usize = DEFAULT_TARGET_PACK_SIZE;

fn hex_to_bytes20(hex: &str) -> Result<[u8; 20]> {
    if hex.len() != 40 {
        bail!("expected 40-char hex OID, got {}", hex.len());
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .context("invalid hex")?;
    }
    Ok(out)
}

fn bytes20_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        let entries = vec![
            PackEntryData {
                oid: "a".repeat(40),
                data: b"hello world".to_vec(),
            },
            PackEntryData {
                oid: "b".repeat(40),
                data: b"goodbye world".to_vec(),
            },
            PackEntryData {
                oid: "c".repeat(40),
                data: vec![0u8; 1024],
            },
        ];

        let (pack_bytes, index) = write_pack(&entries).unwrap();
        assert_eq!(index.len(), 3);

        // Verify integrity
        assert!(verify_pack(&pack_bytes).unwrap());

        // Read each blob back
        for (i, idx_entry) in index.iter().enumerate() {
            let blob = read_blob_from_pack(&pack_bytes, idx_entry.offset, idx_entry.comp_size)
                .unwrap();
            assert_eq!(blob, entries[i].data);
        }

        // Parse index from pack
        let parsed = parse_pack_index(&pack_bytes).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].oid, "a".repeat(40));
        assert_eq!(parsed[1].oid, "b".repeat(40));
    }

    #[test]
    fn range_read() {
        let entries = vec![PackEntryData {
            oid: "d".repeat(40),
            data: b"range read test data".to_vec(),
        }];

        let (pack_bytes, index) = write_pack(&entries).unwrap();
        let idx = &index[0];

        // Simulate S3 range read: extract just the entry bytes
        let start = idx.offset as usize;
        let end = start + ENTRY_HEADER_SIZE + idx.comp_size as usize;
        let range = &pack_bytes[start..end];

        let blob = read_blob_from_range(range, idx.comp_size).unwrap();
        assert_eq!(blob, b"range read test data");
    }
}
