//! Filesystem-backed variant cache with sharded directories and crash-safe atomic writes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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

/// Summary returned by [`FsCache::evict`] after a TTL and disk-budget sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// Number of entries removed because they exceeded the configured TTL.
    pub expired_removed: u64,
    /// Number of oldest entries removed to stay within `max_total_bytes`.
    pub evicted_for_size: u64,
    /// Total bytes freed across both phases.
    pub freed_bytes: u64,
    /// Number of cached entries remaining after the sweep.
    pub remaining_entries: u64,
    /// Total bytes occupied by remaining cached entries.
    pub remaining_bytes: u64,
}

/// Filesystem-backed variant cache storing entries under 2-character hex shard directories.
#[derive(Debug, Clone)]
pub struct FsCache {
    root: PathBuf,
    max_entry_bytes: usize,
    max_total_bytes: Option<u64>,
    ttl: Option<Duration>,
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
            max_total_bytes: None,
            ttl: None,
        })
    }

    /// Configures an optional total disk budget in bytes enforced by [`Self::evict`].
    #[must_use]
    pub const fn with_max_total_bytes(mut self, max_total_bytes: Option<u64>) -> Self {
        self.max_total_bytes = max_total_bytes;
        self
    }

    /// Configures an optional entry time-to-live enforced lazily on [`Self::get`]
    /// and proactively on [`Self::evict`].
    #[must_use]
    pub const fn with_ttl(mut self, ttl: Option<Duration>) -> Self {
        self.ttl = ttl;
        self
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

        if let Some(ttl) = self.ttl
            && let Ok(modified) = metadata.modified()
            && SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= ttl)
        {
            drop(file);
            let _ = fs::remove_file(&path);
            return Ok(None);
        }

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

    /// Runs a retention sweep removing expired entries (when `ttl` is set) and
    /// oldest-`mtime` entries exceeding `max_total_bytes`.
    ///
    /// Hidden temporary files created by concurrent atomic writes are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] if traversing the cache root fails.
    pub fn evict(&self) -> Result<EvictionReport, CacheError> {
        let now = SystemTime::now();
        let mut entries = collect_cache_entries(&self.root)?;
        let mut report = EvictionReport::default();

        if let Some(ttl) = self.ttl {
            let mut kept = Vec::with_capacity(entries.len());
            for entry in entries {
                let expired = now
                    .duration_since(entry.modified)
                    .is_ok_and(|age| age >= ttl);
                if expired {
                    if remove_entry_best_effort(&entry.path)? {
                        report.expired_removed += 1;
                        report.freed_bytes = report.freed_bytes.saturating_add(entry.size);
                    }
                } else {
                    kept.push(entry);
                }
            }
            entries = kept;
        }

        let mut total_bytes: u64 = entries.iter().map(|entry| entry.size).sum();
        let mut remaining_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);

        if let Some(budget) = self.max_total_bytes
            && total_bytes > budget
        {
            entries.sort_by(|a, b| {
                a.modified
                    .cmp(&b.modified)
                    .then_with(|| a.path.cmp(&b.path))
            });

            for entry in entries {
                if total_bytes <= budget {
                    break;
                }
                if remove_entry_best_effort(&entry.path)? {
                    report.evicted_for_size += 1;
                    report.freed_bytes = report.freed_bytes.saturating_add(entry.size);
                    total_bytes = total_bytes.saturating_sub(entry.size);
                    remaining_count = remaining_count.saturating_sub(1);
                }
            }
        }

        prune_empty_shard_dirs(&self.root);

        report.remaining_entries = remaining_count;
        report.remaining_bytes = total_bytes;
        Ok(report)
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.root.join(key.shard_dir()).join(key.file_name())
    }
}

struct DiskEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn collect_cache_entries(root: &Path) -> Result<Vec<DiskEntry>, CacheError> {
    let mut out = Vec::new();
    let shards = fs::read_dir(root).map_err(|err| CacheError::Io(err.kind().to_string()))?;

    for shard in shards {
        let shard = shard.map_err(|err| CacheError::Io(err.kind().to_string()))?;
        let Ok(shard_type) = shard.file_type() else {
            continue;
        };
        if !shard_type.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files {
            let Ok(file) = file else {
                continue;
            };
            // Skip hidden atomic-write temp files (`.<digest>.tmp-*`).
            if file
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push(DiskEntry {
                path: file.path(),
                size: meta.len(),
                modified,
            });
        }
    }

    Ok(out)
}

fn remove_entry_best_effort(path: &Path) -> Result<bool, CacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CacheError::Io(err.kind().to_string())),
    }
}

fn prune_empty_shard_dirs(root: &Path) {
    let Ok(shards) = fs::read_dir(root) else {
        return;
    };
    for shard in shards.flatten() {
        if shard.file_type().is_ok_and(|ft| ft.is_dir()) {
            // `remove_dir` succeeds only when the shard directory is completely empty.
            let _ = fs::remove_dir(shard.path());
        }
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

    #[test]
    fn evicts_expired_entries_lazily_and_during_sweep() {
        let temp = TempCacheDir::new("ttl");
        let cache = FsCache::new(&temp.path, 4096)
            .unwrap()
            .with_ttl(Some(Duration::from_secs(60)));

        let old_key = CacheKey::from_parts("local", "old.jpg", &TransformSpec::default());
        let stale_key = CacheKey::from_parts("local", "stale.jpg", &TransformSpec::default());
        let fresh_key = CacheKey::from_parts("local", "fresh.jpg", &TransformSpec::default());

        cache.put(&old_key, b"old-bytes").unwrap();
        cache.put(&stale_key, b"stale-bytes").unwrap();
        cache.put(&fresh_key, b"fresh-bytes").unwrap();

        let two_mins_ago = SystemTime::now() - Duration::from_secs(120);
        OpenOptions::new()
            .write(true)
            .open(cache.entry_path(&old_key))
            .unwrap()
            .set_modified(two_mins_ago)
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(cache.entry_path(&stale_key))
            .unwrap()
            .set_modified(two_mins_ago)
            .unwrap();

        // Lazy TTL check on get() removes `old_key` and returns a miss.
        assert_eq!(cache.get(&old_key).unwrap(), None);
        assert!(!cache.entry_path(&old_key).exists());

        // Active sweep removes `stale_key` while keeping `fresh_key`.
        let report = cache.evict().unwrap();
        assert_eq!(report.expired_removed, 1);
        assert_eq!(report.evicted_for_size, 0);
        assert_eq!(report.remaining_entries, 1);
        assert_eq!(report.remaining_bytes, 11);
        assert!(cache.get(&fresh_key).unwrap().is_some());
        assert_eq!(cache.get(&stale_key).unwrap(), None);
    }

    #[test]
    fn evicts_oldest_entries_when_total_bytes_exceeds_budget() {
        let temp = TempCacheDir::new("budget");
        let cache = FsCache::new(&temp.path, 4096)
            .unwrap()
            .with_max_total_bytes(Some(20));

        let k1 = CacheKey::from_parts("local", "1.jpg", &TransformSpec::default());
        let k2 = CacheKey::from_parts("local", "2.jpg", &TransformSpec::default());
        let k3 = CacheKey::from_parts("local", "3.jpg", &TransformSpec::default());

        cache.put(&k1, &[1_u8; 10]).unwrap();
        cache.put(&k2, &[2_u8; 10]).unwrap();
        cache.put(&k3, &[3_u8; 10]).unwrap();

        let now = SystemTime::now();
        OpenOptions::new()
            .write(true)
            .open(cache.entry_path(&k1))
            .unwrap()
            .set_modified(now - Duration::from_secs(300))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(cache.entry_path(&k2))
            .unwrap()
            .set_modified(now - Duration::from_secs(200))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(cache.entry_path(&k3))
            .unwrap()
            .set_modified(now - Duration::from_secs(100))
            .unwrap();

        // Place a hidden in-flight temp file to verify it is ignored by eviction.
        let shard = cache.root().join(k3.shard_dir());
        let hidden_tmp = shard.join(".inflight.tmp-1-0");
        fs::write(&hidden_tmp, [9_u8; 50]).unwrap();

        let report = cache.evict().unwrap();
        assert_eq!(report.expired_removed, 0);
        assert_eq!(report.evicted_for_size, 1);
        assert_eq!(report.freed_bytes, 10);
        assert_eq!(report.remaining_entries, 2);
        assert_eq!(report.remaining_bytes, 20);

        // Oldest entry (`k1`) was evicted; newer `k2`, `k3`, and hidden temp file remain.
        assert_eq!(cache.get(&k1).unwrap(), None);
        assert!(cache.get(&k2).unwrap().is_some());
        assert!(cache.get(&k3).unwrap().is_some());
        assert!(hidden_tmp.exists());
    }
}
