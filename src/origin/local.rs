//! Local filesystem origin adapter with path-traversal, symlink-escape,
//! regular-file, and bounded-read defences.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::origin::error::OriginError;

/// Raw source image payload fetched from an origin adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedImage {
    /// Raw encoded bytes read from the origin within configured size limits.
    pub bytes: Vec<u8>,
}

/// Origin adapter serving static assets from a configured local directory.
#[derive(Debug, Clone)]
pub struct LocalFsOrigin {
    root: PathBuf,
    max_bytes: usize,
}

impl LocalFsOrigin {
    /// Creates a local filesystem origin anchored at `root` with a strict byte ceiling.
    ///
    /// Canonicalises `root` eagerly so subsequent containment checks compare against
    /// a resolved directory path without symlink ambiguity.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::InvalidRoot`] if `root` cannot be canonicalised, does not
    /// exist, or is not a directory.
    pub fn new(root: impl AsRef<Path>, max_bytes: usize) -> Result<Self, OriginError> {
        let canonical_root = fs::canonicalize(root.as_ref())
            .map_err(|err| OriginError::InvalidRoot(err.kind().to_string()))?;

        if !canonical_root.is_dir() {
            return Err(OriginError::InvalidRoot(
                "configured root is not a directory".to_owned(),
            ));
        }

        Ok(Self {
            root: canonical_root,
            max_bytes,
        })
    }

    /// Returns the canonicalised root directory path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the maximum payload size in bytes accepted by this origin.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Fetches a regular file located under `self.root` by a validated relative path.
    ///
    /// Rejects traversal segments, absolute paths, symlink escapes outside the root,
    /// non-regular files, and files exceeding `max_bytes`.
    ///
    /// # Errors
    ///
    /// Returns an [`OriginError`] without exposing internal host filesystem paths.
    pub fn fetch(&self, relative_path: &str) -> Result<FetchedImage, OriginError> {
        let candidate = self.resolve_candidate(relative_path)?;

        let canonical_target = fs::canonicalize(&candidate).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => OriginError::NotFound,
            kind => OriginError::Io(kind.to_string()),
        })?;

        if !canonical_target.starts_with(&self.root) {
            return Err(OriginError::PathEscapesRoot);
        }

        let file = File::open(&canonical_target).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => OriginError::NotFound,
            kind => OriginError::Io(kind.to_string()),
        })?;

        // Check file type and size on the open handle to narrow TOCTOU races.
        let metadata = file
            .metadata()
            .map_err(|err| OriginError::Io(err.kind().to_string()))?;

        if !metadata.is_file() {
            return Err(OriginError::NotRegularFile);
        }

        if let Ok(len) = usize::try_from(metadata.len())
            && len > self.max_bytes
        {
            return Err(OriginError::PayloadTooLarge {
                size: len,
                max: self.max_bytes,
            });
        }

        // Stream with a strict cap of `max_bytes + 1` so concurrent file growth
        // cannot trigger unbounded heap allocation.
        let read_limit = u64::try_from(self.max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);

        let initial_cap = usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(self.max_bytes);
        let mut bytes = Vec::with_capacity(initial_cap);

        file.take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|err| OriginError::Io(err.kind().to_string()))?;

        if bytes.len() > self.max_bytes {
            return Err(OriginError::PayloadTooLarge {
                size: bytes.len(),
                max: self.max_bytes,
            });
        }

        Ok(FetchedImage { bytes })
    }

    /// Validates `relative_path` lexically and joins its normal components onto `self.root`.
    fn resolve_candidate(&self, relative_path: &str) -> Result<PathBuf, OriginError> {
        if relative_path.is_empty() {
            return Err(OriginError::InvalidPath(
                "path must not be empty".to_owned(),
            ));
        }
        if relative_path.contains('\0') {
            return Err(OriginError::InvalidPath(
                "path must not contain null bytes".to_owned(),
            ));
        }
        if relative_path.contains('\\') {
            return Err(OriginError::InvalidPath(
                "path must not contain backslashes".to_owned(),
            ));
        }
        if relative_path.starts_with('/') {
            return Err(OriginError::InvalidPath(
                "path must be relative, not absolute".to_owned(),
            ));
        }

        let mut candidate = self.root.clone();

        for segment in relative_path.split('/') {
            match segment {
                "" => {
                    return Err(OriginError::InvalidPath(
                        "path must not contain empty or trailing slash segments".to_owned(),
                    ));
                }
                "." => {
                    return Err(OriginError::InvalidPath(
                        "current-directory segments ('.') are not permitted".to_owned(),
                    ));
                }
                ".." => {
                    return Err(OriginError::InvalidPath(
                        "path traversal ('..') is not permitted".to_owned(),
                    ));
                }
                _ => {
                    // Reject platform-specific prefixes (for example drive letters on Windows).
                    let mut components = Path::new(segment).components();
                    match (components.next(), components.next()) {
                        (Some(Component::Normal(_)), None) => candidate.push(segment),
                        _ => {
                            return Err(OriginError::InvalidPath(
                                "path contains a non-normal segment".to_owned(),
                            ));
                        }
                    }
                }
            }
        }

        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("murklet-{label}-{pid}-{id}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn rejects_invalid_or_non_directory_root() {
        let missing = std::env::temp_dir().join("murklet-definitely-does-not-exist-999999");
        assert!(matches!(
            LocalFsOrigin::new(&missing, 1024),
            Err(OriginError::InvalidRoot(_))
        ));

        let temp = TempDir::new("root-file");
        let file_path = temp.path.join("not-a-dir.txt");
        fs::write(&file_path, b"hello").unwrap();

        assert!(matches!(
            LocalFsOrigin::new(&file_path, 1024),
            Err(OriginError::InvalidRoot(_))
        ));
    }

    #[test]
    fn fetches_valid_nested_file_and_reports_not_found() {
        let temp = TempDir::new("fetch-valid");
        let nested_dir = temp.path.join("products/2026");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(nested_dir.join("hero.jpg"), b"fake-jpeg-bytes").unwrap();

        let origin = LocalFsOrigin::new(&temp.path, 1024).unwrap();

        let fetched = origin.fetch("products/2026/hero.jpg").unwrap();
        assert_eq!(fetched.bytes, b"fake-jpeg-bytes");

        assert_eq!(
            origin.fetch("products/missing.jpg"),
            Err(OriginError::NotFound)
        );
    }

    #[test]
    fn rejects_traversal_absolute_null_and_backslash_paths() {
        let temp = TempDir::new("lexical");
        let origin = LocalFsOrigin::new(&temp.path, 1024).unwrap();

        for bad_path in [
            "",
            "/etc/passwd",
            "../secret.png",
            "nested/../../secret.png",
            "./image.png",
            "nested/./image.png",
            "image\0.png",
            r"nested\image.png",
        ] {
            assert!(
                matches!(origin.fetch(bad_path), Err(OriginError::InvalidPath(_))),
                "expected InvalidPath for {bad_path:?}"
            );
        }
    }

    #[test]
    fn rejects_directories_and_oversized_files() {
        let temp = TempDir::new("dir-and-size");
        fs::create_dir_all(temp.path.join("subdir")).unwrap();
        fs::write(temp.path.join("large.bin"), vec![42_u8; 256]).unwrap();

        let origin = LocalFsOrigin::new(&temp.path, 128).unwrap();

        assert_eq!(origin.fetch("subdir"), Err(OriginError::NotRegularFile));
        assert_eq!(
            origin.fetch("large.bin"),
            Err(OriginError::PayloadTooLarge {
                size: 256,
                max: 128
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape_outside_root_while_allowing_in_root_symlink() {
        use std::os::unix::fs::symlink;

        let outside = TempDir::new("outside");
        let secret = outside.path.join("secret.txt");
        fs::write(&secret, b"top-secret").unwrap();

        let root = TempDir::new("symlink-root");
        let legit = root.path.join("real.png");
        fs::write(&legit, b"legit-bytes").unwrap();

        symlink(&secret, root.path.join("escape.png")).unwrap();
        symlink(&legit, root.path.join("alias.png")).unwrap();

        let origin = LocalFsOrigin::new(&root.path, 1024).unwrap();

        let escape_err = origin.fetch("escape.png").unwrap_err();
        assert_eq!(escape_err, OriginError::PathEscapesRoot);
        assert!(
            !escape_err
                .to_string()
                .contains(&root.path.to_string_lossy().to_string()),
            "error message must not leak host root path"
        );

        let alias = origin.fetch("alias.png").unwrap();
        assert_eq!(alias.bytes, b"legit-bytes");
    }
}
