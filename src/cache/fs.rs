//! Filesystem-backed variant cache with sharded directories and crash-safe atomic writes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cache::error::CacheError;
use crate::cache::key::{CacheKey, compute_etag};
use crate::domain::OutputFormat;

static NEXT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A cached encoded image variant returned by [`FsCache`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVariant {
    /// Encoded image bytes.
    pub bytes: Vec<u8>,
    /// Output format corresponding to the cache key.
    pub format: OutputFormat,
    /// Strong quoted `ETag` computed from the encoded bytes.
    pub etag: String,
}

/// Filesystem-backed variant cache storing entries under 2-character hex shard directories.
#[derive(Debug, Clone)]
pub struct FsCache {
    root: PathBuf,
    max_entry_bytes: usize,
}

impl FsCache {
    /// Prepares a filesystem cache rooted at `root`, creating the directory if absent.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::InvalidRoot`] if `root` cannot be created, canonicalised,
    /// or exists as a non-directory.
    pub fn new(root: impl AsRef<Path>, max_entry_bytes: usize) -> Result<Self, CacheError> {
        let root_ref = root.as_ref();
        fs::create_dir_all(root_ref)
            .map_err(|err| CacheError::InvalidRoot(err.kind().to_string()))?;

        let canonical_root = fs::canonicalize(root_ref)
            .map_err(|err| CacheError::InvalidRoot(err.kind().to_string()))?;

        if !canonical_root.is_dir() {
            return Err(CacheError::InvalidRoot(
                "configured cache root is not a directory".to_owned(),
            ));
        }

        Ok(Self {
            root: canonical_root,
            max_entry_bytes,
        })
    }

    /// Returns the canonicalised cache root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Looks up a cached variant by canonical [`CacheKey`].
    ///
    /// Returns `Ok(None)` on cache miss.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::EntryTooLarge`] if the stored file exceeds `max_entry_bytes`,
    /// or [`CacheError::Io`] on unexpected filesystem errors.
    pub fn get(&self, key: &CacheKey) -> Result<Option<CachedVariant>, CacheError> {
        let path = self.entry_path(key);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(CacheError::Io(err.kind().to_string())),
        };

        let metadata = file
            .metadata()
            .map_err(|err| CacheError::Io(err.kind().to_string()))?;

        if let Ok(len) = usize::try_from(metadata.len())
            && len > self.max_entry_bytes
        {
            return Err(CacheError::EntryTooLarge {
                size: len,
                max: self.max_entry_bytes,
            });
        }

        let read_limit = u64::try_from(self.max_entry_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);

        let initial_cap = usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(self.max_entry_bytes);
        let mut bytes = Vec::with_capacity(initial_cap);

        file.take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|err| CacheError::Io(err.kind().to_string()))?;

        if bytes.len() > self.max_entry_bytes {
            return Err(CacheError::EntryTooLarge {
                size: bytes.len(),
                max: self.max_entry_bytes,
            });
        }

        let etag = compute_etag(&bytes);
        Ok(Some(CachedVariant {
            bytes,
            format: key.format(),
            etag,
        }))
    }

    /// Stores encoded variant bytes atomically via a temporary file and rename.
    ///
    /// Readers never observe partially written files, and temporary files are removed
    /// if writing or renaming fails.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::EntryTooLarge`] if `bytes.len() > max_entry_bytes`, or
    /// [`CacheError::Io`] on filesystem failure.
    pub fn put(&self, key: &CacheKey, bytes: &[u8]) -> Result<CachedVariant, CacheError> {
        if bytes.len() > self.max_entry_bytes {
            return Err(CacheError::EntryTooLarge {
                size: bytes.len(),
                max: self.max_entry_bytes,
            });
        }

        let shard_dir = self.root.join(key.shard_dir());
        fs::create_dir_all(&shard_dir).map_err(|err| CacheError::Io(err.kind().to_string()))?;

        let seq = NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp_path = shard_dir.join(format!(".{}.tmp-{pid}-{seq}", key.digest_hex()));
        let final_path = shard_dir.join(key.file_name());

        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(bytes)?;
            file.flush()?;
            drop(file);
            fs::rename(&tmp_path, &final_path)?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(CacheError::Io(err.kind().to_string()));
        }

        let etag = compute_etag(bytes);
        Ok(CachedVariant {
            bytes: bytes.to_vec(),
            format: key.format(),
            etag,
        })
    }

    /// Deletes a single cached variant if present.
    ///
    /// Returns `Ok(true)` when an entry was removed and `Ok(false)` when it was absent.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on unexpected filesystem errors.
    pub fn delete(&self, key: &CacheKey) -> Result<bool, CacheError> {
        match fs::remove_file(self.entry_path(key)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(CacheError::Io(err.kind().to_string())),
        }
    }

    /// Removes all cached files and shard subdirectories inside `self.root` while
    /// keeping the root directory itself intact.
    ///
    /// Returns the number of cached files removed.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] if directory traversal or removal fails.
    pub fn cleanup(&self) -> Result<u64, CacheError> {
        remove_children_recursively(&self.root)
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.root.join(key.shard_dir()).join(key.file_name())
    }
}

fn remove_children_recursively(dir: &Path) -> Result<u64, CacheError> {
    let mut removed_files: u64 = 0;
    let entries = fs::read_dir(dir).map_err(|err| CacheError::Io(err.kind().to_string()))?;

    for entry in entries {
        let entry = entry.map_err(|err| CacheError::Io(err.kind().to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| CacheError::Io(err.kind().to_string()))?;

        if file_type.is_dir() {
            removed_files += remove_children_recursively(&path)?;
            fs::remove_dir(&path).map_err(|err| CacheError::Io(err.kind().to_string()))?;
        } else {
            fs::remove_file(&path).map_err(|err| CacheError::Io(err.kind().to_string()))?;
            removed_files += 1;
        }
    }

    Ok(removed_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OutputFormat, TransformSpec};

    static NEXT_CACHE_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TempCacheDir {
        path: PathBuf,
    }

    impl TempCacheDir {
        fn new(label: &str) -> Self {
            let id = NEXT_CACHE_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("murklet-cache-{label}-{pid}-{id}"));
            Self { path }
        }
    }

    impl Drop for TempCacheDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn put_get_overwrite_and_delete_round_trip() {
        let temp = TempCacheDir::new("round-trip");
        let cache = FsCache::new(&temp.path, 4096).unwrap();

        let key = CacheKey::from_parts(
            "local",
            "photos/cat.jpg",
            &TransformSpec {
                format: OutputFormat::Webp,
                ..TransformSpec::default()
            },
        );

        // Initial lookup is a miss.
        assert_eq!(cache.get(&key).unwrap(), None);

        // Put writes atomically and returns a strong ETag.
        let stored = cache.put(&key, b"webp-v1-bytes").unwrap();
        assert_eq!(stored.bytes, b"webp-v1-bytes");
        assert_eq!(stored.format, OutputFormat::Webp);
        assert_eq!(stored.etag, compute_etag(b"webp-v1-bytes"));

        // Subsequent get hits and matches byte-for-byte.
        let loaded = cache.get(&key).unwrap().unwrap();
        assert_eq!(loaded, stored);

        // Overwriting the same key atomically replaces content and updates ETag.
        let updated = cache.put(&key, b"webp-v2-updated").unwrap();
        assert_ne!(updated.etag, stored.etag);
        assert_eq!(cache.get(&key).unwrap().unwrap(), updated);

        // Delete removes the entry; deleting again returns false.
        assert!(cache.delete(&key).unwrap());
        assert!(!cache.delete(&key).unwrap());
        assert_eq!(cache.get(&key).unwrap(), None);
    }

    #[test]
    fn rejects_entries_exceeding_max_entry_bytes() {
        let temp = TempCacheDir::new("oversized");
        let cache = FsCache::new(&temp.path, 16).unwrap();
        let key = CacheKey::from_parts("local", "big.jpg", &TransformSpec::default());

        assert_eq!(
            cache.put(&key, &[7_u8; 32]),
            Err(CacheError::EntryTooLarge { size: 32, max: 16 })
        );
        assert_eq!(cache.get(&key).unwrap(), None);
    }

    #[test]
    fn cleanup_removes_all_entries_and_shards_while_preserving_root() {
        let temp = TempCacheDir::new("cleanup");
        let cache = FsCache::new(&temp.path, 4096).unwrap();

        let key_a = CacheKey::from_parts("local", "a.jpg", &TransformSpec::default());
        let key_b = CacheKey::from_parts("local", "b.jpg", &TransformSpec::default());
        cache.put(&key_a, b"aaa").unwrap();
        cache.put(&key_b, b"bbb").unwrap();

        let removed = cache.cleanup().unwrap();
        assert_eq!(removed, 2);
        assert!(cache.root().is_dir());
        assert_eq!(cache.get(&key_a).unwrap(), None);
        assert_eq!(cache.get(&key_b).unwrap(), None);
    }

    #[test]
    fn rejects_root_that_is_an_existing_file() {
        let temp = TempCacheDir::new("root-file");
        fs::create_dir_all(&temp.path).unwrap();
        let file = temp.path.join("not-a-dir");
        fs::write(&file, b"hello").unwrap();

        assert!(matches!(
            FsCache::new(&file, 1024),
            Err(CacheError::InvalidRoot(_))
        ));
    }
}
