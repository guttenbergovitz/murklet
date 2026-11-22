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
        let Some(bytes) = self.read_entry_bytes(&path)? else {
            return Ok(None);
        };
        let etag = compute_etag(&bytes);
        Ok(Some(CachedVariant {
            bytes,
            format: key.format(),
            etag,
        }))
    }

    /// Looks up raw source bytes stored under a 64-character hex digest (`<shard>/<digest>.src`).
    ///
    /// Returns `Ok(None)` on cache miss or TTL expiry.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::EntryTooLarge`] or [`CacheError::Io`] on failure.
    pub fn get_raw(&self, digest_hex: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let path = self.raw_entry_path(digest_hex);
        self.read_entry_bytes(&path)
    }

    /// Looks up raw source bytes while invoking `reserve(target_total_bytes)` *before*
    /// allocating the payload buffer or extending it on concurrent file growth.
    ///
    /// Returns `Ok(None)` on cache miss or TTL expiry.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::BudgetExhausted`] when `reserve` rejects the allocation,
    /// [`CacheError::EntryTooLarge`], or [`CacheError::Io`].
    pub fn get_raw_with_budget<F>(
        &self,
        digest_hex: &str,
        mut reserve: F,
    ) -> Result<Option<Vec<u8>>, CacheError>
    where
        F: FnMut(usize) -> Result<(), CacheError>,
    {
        let path = self.raw_entry_path(digest_hex);
        self.read_entry_bytes_with_budget(&path, Some(&mut reserve))
    }

    /// Stores raw source bytes atomically under `<shard>/<digest>.src`.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::EntryTooLarge`] if `bytes.len() > max_entry_bytes`, or
    /// [`CacheError::Io`] on filesystem failure.
    pub fn put_raw(&self, digest_hex: &str, bytes: &[u8]) -> Result<(), CacheError> {
        if bytes.len() > self.max_entry_bytes {
            return Err(CacheError::EntryTooLarge {
                size: bytes.len(),
                max: self.max_entry_bytes,
            });
        }
        let shard = digest_hex.get(..2).unwrap_or("00");
        let shard_dir = self.root.join(shard);
        let final_path = shard_dir.join(format!("{digest_hex}.src"));
        Self::write_entry_atomic(&shard_dir, digest_hex, &final_path, bytes)
    }

    fn read_entry_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, CacheError> {
        self.read_entry_bytes_with_budget(path, None)
    }

    fn read_entry_bytes_with_budget(
        &self,
        path: &Path,
        mut reserve: Option<&mut dyn FnMut(usize) -> Result<(), CacheError>>,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        let mut file = match File::open(path) {
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
            let _ = fs::remove_file(path);
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

        let initial_cap = usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(self.max_entry_bytes);
        if initial_cap > 0
            && let Some(cb) = reserve.as_deref_mut()
        {
            cb(initial_cap)?;
        }
        let mut bytes = Vec::with_capacity(initial_cap);
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = file
                .read(&mut chunk)
                .map_err(|err| CacheError::Io(err.kind().to_string()))?;
            if read == 0 {
                break;
            }
            let new_len = bytes.len().saturating_add(read);
            if new_len > self.max_entry_bytes {
                return Err(CacheError::EntryTooLarge {
                    size: new_len,
                    max: self.max_entry_bytes,
                });
            }
            if let Some(cb) = reserve.as_deref_mut() {
                cb(new_len)?;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }

        Ok(Some(bytes))
    }

    fn write_entry_atomic(
        shard_dir: &Path,
        tmp_prefix: &str,
        final_path: &Path,
        bytes: &[u8],
    ) -> Result<(), CacheError> {
        fs::create_dir_all(shard_dir).map_err(|err| CacheError::Io(err.kind().to_string()))?;

        let seq = NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp_path = shard_dir.join(format!(".{tmp_prefix}.tmp-{pid}-{seq}"));

        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(bytes)?;
            file.flush()?;
            drop(file);
            fs::rename(&tmp_path, final_path)?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(CacheError::Io(err.kind().to_string()));
        }

        Ok(())
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

        let entry_dir = self.entry_dir(key);
        let final_path = entry_dir.join(key.file_name());
        Self::write_entry_atomic(&entry_dir, key.digest_hex(), &final_path, bytes)?;

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

    /// Deletes a single raw source blob (`<shard>/<digest>.src`) if present.
    ///
    /// Returns `Ok(true)` when removed and `Ok(false)` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on unexpected filesystem errors.
    pub fn delete_raw(&self, digest_hex: &str) -> Result<bool, CacheError> {
        let path = self.raw_entry_path(digest_hex);
        let removed = match fs::remove_file(&path) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(err) => return Err(CacheError::Io(err.kind().to_string())),
        };
        if removed && let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(removed)
    }

    /// Removes all cached variants belonging to `source_digest_hex` in O(1) directory lookup,
    /// leaving all other cached sources and in-flight hidden temp files untouched.
    ///
    /// Returns the number of cached variant files removed.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on unexpected filesystem errors.
    pub fn purge_source(&self, source_digest_hex: &str) -> Result<u64, CacheError> {
        let shard = source_digest_hex.get(..2).unwrap_or("00");
        let shard_dir = self.root.join(shard);
        let source_dir = shard_dir.join(source_digest_hex);

        let entries = match fs::read_dir(&source_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(CacheError::Io(err.kind().to_string())),
        };

        let mut removed: u64 = 0;
        for entry in entries {
            let entry = entry.map_err(|err| CacheError::Io(err.kind().to_string()))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            if remove_entry_best_effort(&entry.path())? {
                removed += 1;
            }
        }

        // Prune empty source and shard directories best-effort.
        let _ = fs::remove_dir(&source_dir);
        let _ = fs::remove_dir(&shard_dir);

        Ok(removed)
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

    fn entry_dir(&self, key: &CacheKey) -> PathBuf {
        self.root
            .join(key.shard_dir())
            .join(key.source_digest_hex())
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.entry_dir(key).join(key.file_name())
    }

    fn raw_entry_path(&self, digest_hex: &str) -> PathBuf {
        let shard = digest_hex.get(..2).unwrap_or("00");
        self.root.join(shard).join(format!("{digest_hex}.src"))
    }
}

struct DiskEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn collect_cache_entries(root: &Path) -> Result<Vec<DiskEntry>, CacheError> {
    let mut out = Vec::new();
    collect_cache_entries_in(root, &mut out)?;
    Ok(out)
}

fn collect_cache_entries_in(dir: &Path, out: &mut Vec<DiskEntry>) -> Result<(), CacheError> {
    let entries = fs::read_dir(dir).map_err(|err| CacheError::Io(err.kind().to_string()))?;

    for entry in entries {
        let entry = entry.map_err(|err| CacheError::Io(err.kind().to_string()))?;
        // Skip hidden atomic-write temp files (`.<digest>.tmp-*`).
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_cache_entries_in(&entry.path(), out)?;
        } else if file_type.is_file() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push(DiskEntry {
                path: entry.path(),
                size: meta.len(),
                modified,
            });
        }
    }

    Ok(())
}

fn remove_entry_best_effort(path: &Path) -> Result<bool, CacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CacheError::Io(err.kind().to_string())),
    }
}

fn prune_empty_shard_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            let path = entry.path();
            prune_empty_shard_dirs(&path);
            // `remove_dir` succeeds only when the directory is completely empty.
            let _ = fs::remove_dir(&path);
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

    #[test]
    fn raw_source_cache_round_trips_and_respects_ttl() {
        let temp = TempCacheDir::new("raw-source");
        let cache = FsCache::new(&temp.path, 4096)
            .unwrap()
            .with_ttl(Some(Duration::from_secs(60)));

        let digest = crate::cache::source_digest_hex("s3", "s3://photos/hero.jpg");
        assert_eq!(cache.get_raw(&digest).unwrap(), None);

        cache.put_raw(&digest, b"raw-upstream-jpeg-bytes").unwrap();
        assert_eq!(
            cache.get_raw(&digest).unwrap().as_deref(),
            Some(b"raw-upstream-jpeg-bytes".as_slice())
        );

        let two_mins_ago = SystemTime::now() - Duration::from_secs(120);
        OpenOptions::new()
            .write(true)
            .open(cache.raw_entry_path(&digest))
            .unwrap()
            .set_modified(two_mins_ago)
            .unwrap();

        assert_eq!(cache.get_raw(&digest).unwrap(), None);
    }

    #[test]
    fn purge_source_and_delete_raw_isolate_target_asset() {
        let temp = TempCacheDir::new("purge-source");
        let cache = FsCache::new(&temp.path, 4096).unwrap();

        let webp_spec = TransformSpec {
            format: OutputFormat::Webp,
            ..TransformSpec::default()
        };
        let jpg_spec = TransformSpec {
            format: OutputFormat::Jpeg,
            ..TransformSpec::default()
        };

        let hero_webp = CacheKey::from_parts("local", "hero.png", &webp_spec);
        let hero_jpg = CacheKey::from_parts("local", "hero.png", &jpg_spec);
        let other_webp = CacheKey::from_parts("local", "other.png", &webp_spec);

        cache.put(&hero_webp, b"hero-webp").unwrap();
        cache.put(&hero_jpg, b"hero-jpg").unwrap();
        cache.put(&other_webp, b"other-webp").unwrap();

        let digest_a = crate::cache::source_digest_hex("local", "hero.png");
        let removed = cache.purge_source(&digest_a).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(cache.get(&hero_webp).unwrap(), None);
        assert_eq!(cache.get(&hero_jpg).unwrap(), None);
        assert!(cache.get(&other_webp).unwrap().is_some());

        // Purging again is a clean idempotent no-op.
        assert_eq!(cache.purge_source(&digest_a).unwrap(), 0);

        // Raw source blob deletion works and is idempotent.
        cache.put_raw(&digest_a, b"raw-bytes").unwrap();
        assert!(cache.delete_raw(&digest_a).unwrap());
        assert!(!cache.delete_raw(&digest_a).unwrap());
        assert_eq!(cache.get_raw(&digest_a).unwrap(), None);
    }

    #[test]
    fn get_raw_with_budget_reserves_before_reading_and_aborts_on_exhaustion() {
        let dir = TempCacheDir::new("raw-budget");
        let cache = FsCache::new(&dir.path, 64 * 1024).unwrap();
        let digest = crate::cache::source_digest_hex("http", "https://cdn.example.com/big.png");
        let payload = vec![0xCDu8; 4096];
        cache.put_raw(&digest, &payload).unwrap();

        // Rejecting reservation before reading returns BudgetExhausted.
        let mut observed_target = 0usize;
        let rejected = cache.get_raw_with_budget(&digest, |target| {
            observed_target = target;
            Err(CacheError::BudgetExhausted)
        });
        assert_eq!(rejected, Err(CacheError::BudgetExhausted));
        assert_eq!(observed_target, 4096);

        // Approving reservation returns the full cached payload.
        let ok = cache
            .get_raw_with_budget(&digest, |_| Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(ok, payload);
    }
}
