//! Linearisable in-memory invalidation epochs preventing purge-vs-write races.
//!
//! When `POST /cache/purge` or `POST /cache/cleanup` runs while an earlier cache-miss
//! request is still fetching or transforming a stale source, the in-flight request must
//! not persist its stale result onto disk after invalidation completes.
//!
//! Each in-flight cache-miss acquires a [`FlightToken`] before fetching. Purges and
//! cleanups advance the relevant generation counter and hold an exclusive write gate
//! while deleting files; cache writes hold a shared read gate and verify that their
//! token's generation is still current before touching disk.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

#[derive(Debug, Default)]
struct ActiveSource {
    active_flights: usize,
    generation: u64,
}

#[derive(Debug, Default)]
struct TrackerInner {
    global_epoch: AtomicU64,
    sources: Mutex<HashMap<String, ActiveSource>>,
    /// Shared lock for cache writes; exclusive lock for purge/cleanup disk deletions.
    /// Guarantees linearisability between checking a token's generation and renaming
    /// the temp file into place.
    io_gate: RwLock<()>,
}

/// Coordinates in-flight cache writes with per-source purges and full cleanups.
#[derive(Debug, Clone, Default)]
pub struct InvalidationTracker {
    inner: Arc<TrackerInner>,
}

/// RAII token representing a single in-flight fetch/transform for a source digest.
///
/// Automatically cleans up per-source tracking state when dropped so memory usage
/// stays bounded to currently active cache-miss requests.
#[derive(Debug)]
pub struct FlightToken {
    tracker: Arc<TrackerInner>,
    source_digest: String,
    global_epoch: u64,
    source_generation: u64,
}

impl Drop for FlightToken {
    fn drop(&mut self) {
        let mut sources = self
            .tracker
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = sources.get_mut(&self.source_digest) {
            entry.active_flights = entry.active_flights.saturating_sub(1);
            if entry.active_flights == 0 {
                sources.remove(&self.source_digest);
            }
        }
    }
}

impl InvalidationTracker {
    /// Creates a new invalidation tracker with initial epoch `0`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an in-flight cache-miss for `source_digest` and returns its [`FlightToken`].
    ///
    /// Must be called *before* reading raw source bytes from an origin or source cache.
    #[must_use]
    pub fn begin_flight(&self, source_digest: impl Into<String>) -> FlightToken {
        let source_digest = source_digest.into();
        let global_epoch = self.inner.global_epoch.load(Ordering::Acquire);
        let mut sources = self
            .inner
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = sources.entry(source_digest.clone()).or_default();
        entry.active_flights += 1;
        let source_generation = entry.generation;

        FlightToken {
            tracker: Arc::clone(&self.inner),
            source_digest,
            global_epoch,
            source_generation,
        }
    }

    /// Executes `write_fn` under a shared I/O gate if `token` has not been invalidated
    /// by a concurrent purge or full cleanup.
    ///
    /// Returns `Some(R)` when the write was executed, or `None` when skipped because
    /// the flight was invalidated.
    pub fn write_if_current<R>(
        &self,
        token: &FlightToken,
        write_fn: impl FnOnce() -> R,
    ) -> Option<R> {
        let _gate = self
            .inner
            .io_gate
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if !self.is_current(token) {
            return None;
        }
        Some(write_fn())
    }

    /// Advances the generation for `source_digest` and executes `purge_fn` under the
    /// exclusive I/O gate so no in-flight write can repopulate stale files.
    pub fn run_source_purge<R>(&self, source_digest: &str, purge_fn: impl FnOnce() -> R) -> R {
        let _gate = self
            .inner
            .io_gate
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        {
            let mut sources = self
                .inner
                .sources
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = sources.get_mut(source_digest) {
                entry.generation = entry.generation.wrapping_add(1);
            }
        }
        purge_fn()
    }

    /// Advances the global cleanup epoch and executes `cleanup_fn` under the exclusive
    /// I/O gate so no in-flight write can repopulate stale files.
    pub fn run_full_cleanup<R>(&self, cleanup_fn: impl FnOnce() -> R) -> R {
        let _gate = self
            .inner
            .io_gate
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        self.inner.global_epoch.fetch_add(1, Ordering::AcqRel);
        cleanup_fn()
    }

    fn is_current(&self, token: &FlightToken) -> bool {
        if self.inner.global_epoch.load(Ordering::Acquire) != token.global_epoch {
            return false;
        }
        let sources = self
            .inner
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        sources
            .get(&token.source_digest)
            .is_some_and(|entry| entry.generation == token.source_generation)
    }

    #[cfg(test)]
    fn tracked_source_count(&self) -> usize {
        self.inner
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_purge_invalidates_only_matching_in_flight_tokens() {
        let tracker = InvalidationTracker::new();
        let flight_a = tracker.begin_flight("digest-a");
        let flight_b = tracker.begin_flight("digest-b");
        assert_eq!(tracker.tracked_source_count(), 2);

        tracker.run_source_purge("digest-a", || {});

        // Stale flight A is rejected; unrelated flight B still writes cleanly.
        assert_eq!(tracker.write_if_current(&flight_a, || "wrote-a"), None);
        assert_eq!(
            tracker.write_if_current(&flight_b, || "wrote-b"),
            Some("wrote-b")
        );

        // A new flight started after purge succeeds even while flight A is still alive.
        let flight_a_fresh = tracker.begin_flight("digest-a");
        assert_eq!(
            tracker.write_if_current(&flight_a_fresh, || "wrote-a-fresh"),
            Some("wrote-a-fresh")
        );

        drop(flight_a);
        drop(flight_a_fresh);
        drop(flight_b);
        assert_eq!(tracker.tracked_source_count(), 0);
    }

    #[test]
    fn full_cleanup_invalidates_all_in_flight_tokens() {
        let tracker = InvalidationTracker::new();
        let flight_a = tracker.begin_flight("digest-a");
        let flight_b = tracker.begin_flight("digest-b");

        tracker.run_full_cleanup(|| {});

        assert_eq!(tracker.write_if_current(&flight_a, || 1), None);
        assert_eq!(tracker.write_if_current(&flight_b, || 2), None);

        let after_cleanup = tracker.begin_flight("digest-a");
        assert_eq!(tracker.write_if_current(&after_cleanup, || 3), Some(3));
    }
}
