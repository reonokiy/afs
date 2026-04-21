//! Git LFS pointer detection.
//!
//! LFS pointer format:
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:{64 hex chars}
//! size {bytes}
//! ```
//!
//! Pointer files are small (~130 bytes). During tree indexing, we check blobs
//! under 200 bytes for the LFS signature.

/// Maximum size of a blob that could be an LFS pointer.
pub const LFS_POINTER_MAX_SIZE: u64 = 200;

/// Parsed LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    /// The SHA-256 OID of the actual content (64 hex chars).
    pub oid: String,
    /// Size of the actual content in bytes.
    pub size: u64,
}

/// Try to parse blob data as an LFS pointer.
/// Returns `Some(LfsPointer)` if the data matches the LFS pointer format.
pub fn parse_lfs_pointer(data: &[u8]) -> Option<LfsPointer> {
    let text = std::str::from_utf8(data).ok()?;

    // Must start with the version line
    if !text.starts_with("version https://git-lfs.github.com/spec/v1") {
        return None;
    }

    let mut oid = None;
    let mut size = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("oid sha256:") {
            let hex = rest.trim();
            if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                oid = Some(hex.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("size ") {
            size = rest.trim().parse::<u64>().ok();
        }
    }

    match (oid, size) {
        (Some(oid), Some(size)) => Some(LfsPointer { oid, size }),
        _ => None,
    }
}

/// Check if blob data looks like it could be an LFS pointer (quick pre-check).
pub fn might_be_lfs_pointer(data: &[u8]) -> bool {
    data.len() < LFS_POINTER_MAX_SIZE as usize
        && data.starts_with(b"version https://git-lfs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pointer() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
        let parsed = parse_lfs_pointer(pointer).unwrap();
        assert_eq!(
            parsed.oid,
            "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
        );
        assert_eq!(parsed.size, 12345);
    }

    #[test]
    fn parse_pointer_with_extra_fields() {
        // LFS spec allows extensions
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\nsize 999\next-0-foo bar\n";
        let parsed = parse_lfs_pointer(pointer).unwrap();
        assert_eq!(parsed.size, 999);
    }

    #[test]
    fn reject_non_pointer() {
        assert!(parse_lfs_pointer(b"hello world").is_none());
        assert!(parse_lfs_pointer(b"version https://git-lfs.github.com/spec/v1\n").is_none());
        // Missing size
        assert!(parse_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n"
        ).is_none());
    }

    #[test]
    fn quick_check() {
        assert!(might_be_lfs_pointer(b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n"));
        assert!(!might_be_lfs_pointer(b"hello world"));
        assert!(!might_be_lfs_pointer(&vec![0u8; 300])); // too large
    }
}
