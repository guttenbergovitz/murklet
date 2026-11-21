//! Canonical, versioned cache identity for transformed image variants.

use crate::domain::{OutputFormat, TransformSpec};

/// Deterministic cache key derived from an origin identifier, source asset key,
/// and canonical [`TransformSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    digest_hex: String,
    format: OutputFormat,
}

impl CacheKey {
    /// Builds a canonical BLAKE3 cache key using length-prefixed fields so
    /// differing `(origin_id, source_key)` splits can never collide.
    #[must_use]
    pub fn from_parts(origin_id: &str, source_key: &str, spec: &TransformSpec) -> Self {
        let mut hasher = blake3::Hasher::new();
        update_length_prefixed(&mut hasher, origin_id.as_bytes());
        update_length_prefixed(&mut hasher, source_key.as_bytes());
        update_length_prefixed(&mut hasher, spec.canonical_representation().as_bytes());

        let digest_hex = hasher.finalize().to_hex().to_string();

        Self {
            digest_hex,
            format: spec.format,
        }
    }

    /// Returns the full 64-character lowercase hexadecimal BLAKE3 digest.
    #[must_use]
    pub fn digest_hex(&self) -> &str {
        &self.digest_hex
    }

    /// Returns the target [`OutputFormat`] associated with this variant key.
    #[must_use]
    pub const fn format(&self) -> OutputFormat {
        self.format
    }

    /// Returns the 2-character hexadecimal shard prefix used to fan out files
    /// across subdirectories on disk.
    #[must_use]
    pub fn shard_dir(&self) -> &str {
        self.digest_hex.get(..2).unwrap_or("00")
    }

    /// Returns the canonical filename (`<digest>.<ext>`) stored inside the shard directory.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.digest_hex, self.format.file_extension())
    }
}

/// Computes a canonical 64-character lowercase hexadecimal BLAKE3 digest for a
/// raw upstream source asset identified by `(origin_id, source_key)`.
#[must_use]
pub fn source_digest_hex(origin_id: &str, source_key: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    update_length_prefixed(&mut hasher, b"src:v1");
    update_length_prefixed(&mut hasher, origin_id.as_bytes());
    update_length_prefixed(&mut hasher, source_key.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Computes a strong quoted HTTP `ETag` from the BLAKE3 digest of encoded variant bytes.
#[must_use]
pub fn compute_etag(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    let hex = hash.to_hex();
    let short = hex.as_str().get(..32).unwrap_or(hex.as_str());
    format!("\"{short}\"")
}

fn update_length_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.update(&len.to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Axis, DEFAULT_MAX_DIMENSION, Dimension, Quality, TargetDimensions};

    #[test]
    fn equivalent_specs_produce_identical_cache_keys() {
        let spec_a = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 800, DEFAULT_MAX_DIMENSION).unwrap()),
                None,
            ),
            format: OutputFormat::Png,
            quality: Quality::new(30).unwrap(),
            filter: None,
            ..TransformSpec::default()
        };
        let spec_b = TransformSpec {
            quality: Quality::new(90).unwrap(),
            ..spec_a.clone()
        };

        let key_a = CacheKey::from_parts("local", "hero.png", &spec_a);
        let key_b = CacheKey::from_parts("local", "hero.png", &spec_b);

        assert_eq!(key_a, key_b);
        assert_eq!(key_a.digest_hex().len(), 64);
        assert_eq!(key_a.shard_dir().len(), 2);
        assert_eq!(key_a.file_name(), format!("{}.png", key_a.digest_hex()));
    }

    #[test]
    fn length_prefixing_prevents_origin_and_source_delimiter_collisions() {
        let spec = TransformSpec::default();
        let key_a = CacheKey::from_parts("a|b", "c", &spec);
        let key_b = CacheKey::from_parts("a", "b|c", &spec);

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn etag_is_strong_quoted_and_content_dependent() {
        let etag_a1 = compute_etag(b"image-bytes-a");
        let etag_a2 = compute_etag(b"image-bytes-a");
        let etag_b = compute_etag(b"image-bytes-b");

        assert_eq!(etag_a1, etag_a2);
        assert_ne!(etag_a1, etag_b);
        assert!(etag_a1.starts_with('"') && etag_a1.ends_with('"'));
        assert_eq!(etag_a1.len(), 34);
    }
}
