//! Pre-allocation and incremental streaming byte-budget reservation for origin fetches.
//!
//! Ensures raw source bytes are accounted for against `limits.max_in_flight_source_bytes`
//! *before* allocating buffers or appending streamed chunks, and automatically releases
//! permits and live-byte gauges when the reservation drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::origin::OriginError;

/// Converts a byte count into 1 KiB semaphore permit units (rounded up, minimum 1 for non-zero).
#[must_use]
pub fn bytes_to_kib_permits(bytes: usize) -> u32 {
    if bytes == 0 {
        return 0;
    }
    let kib = bytes.div_ceil(1024).max(1);
    u32::try_from(kib.min(Semaphore::MAX_PERMITS)).unwrap_or(u32::MAX)
}

/// Shared aggregate byte budget governing raw source payloads held in memory across
/// all concurrent origin fetches and queued transformations.
#[derive(Debug, Clone)]
pub struct ByteBudget {
    permits: Arc<Semaphore>,
    budget_kib: u32,
    live_bytes: Arc<AtomicUsize>,
}

impl ByteBudget {
    /// Creates a new byte budget backed by `max_bytes` (rounded up to 1 KiB permit units)
    /// and a shared `live_bytes` atomic gauge.
    #[must_use]
    pub fn new(max_bytes: usize, live_bytes: Arc<AtomicUsize>) -> Self {
        let budget_kib = bytes_to_kib_permits(max_bytes).max(1);
        Self {
            permits: Arc::new(Semaphore::new(budget_kib as usize)),
            budget_kib,
            live_bytes,
        }
    }

    /// Returns the underlying KiB permit semaphore (used by tests to inspect or saturate budget).
    #[must_use]
    pub fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.permits)
    }

    /// Starts an initially empty [`ByteReservation`] bounded by `timeout` per incremental acquire.
    #[must_use]
    pub fn begin(&self, timeout: Duration) -> ByteReservation {
        ByteReservation {
            budget: self.clone(),
            timeout,
            permit: None,
            reserved_kib: 0,
            reserved_bytes: 0,
        }
    }
}

/// RAII reservation tracking permits and live bytes for a single in-flight source payload.
///
/// Grows monotonically as streamed chunks arrive (before copying into buffers) and
/// releases all permits and gauge bytes automatically on [`Drop`].
#[derive(Debug)]
pub struct ByteReservation {
    budget: ByteBudget,
    timeout: Duration,
    permit: Option<OwnedSemaphorePermit>,
    reserved_kib: u32,
    reserved_bytes: usize,
}

impl ByteReservation {
    /// Reserves enough budget to hold `target_total_bytes` in memory before allocating or
    /// appending data.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::BudgetExhausted`] if `target_total_bytes` exceeds the total
    /// configured budget or if waiting for permits exceeds `self.timeout`.
    pub async fn reserve_total(&mut self, target_total_bytes: usize) -> Result<(), OriginError> {
        if target_total_bytes <= self.reserved_bytes {
            return Ok(());
        }

        let target_kib = bytes_to_kib_permits(target_total_bytes);
        if target_kib > self.budget.budget_kib {
            return Err(OriginError::BudgetExhausted);
        }

        let delta_kib = target_kib.saturating_sub(self.reserved_kib);
        if delta_kib > 0 {
            let acquired = tokio::time::timeout(
                self.timeout,
                Arc::clone(&self.budget.permits).acquire_many_owned(delta_kib),
            )
            .await
            .map_err(|_| OriginError::BudgetExhausted)?
            .map_err(|_| OriginError::BudgetExhausted)?;

            match &mut self.permit {
                Some(existing) => existing.merge(acquired),
                None => self.permit = Some(acquired),
            }
            self.reserved_kib = target_kib;
        }

        let delta_bytes = target_total_bytes.saturating_sub(self.reserved_bytes);
        self.reserved_bytes = target_total_bytes;
        self.budget
            .live_bytes
            .fetch_add(delta_bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Blocking counterpart to [`Self::reserve_total`] for use inside `spawn_blocking`
    /// filesystem reads.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::BudgetExhausted`] if the budget cannot be satisfied within
    /// `self.timeout`.
    pub fn reserve_total_blocking(&mut self, target_total_bytes: usize) -> Result<(), OriginError> {
        if target_total_bytes <= self.reserved_bytes {
            return Ok(());
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            return handle.block_on(self.reserve_total(target_total_bytes));
        }

        // Non-Tokio synchronous fallback (for direct unit tests without a runtime).
        let target_kib = bytes_to_kib_permits(target_total_bytes);
        if target_kib > self.budget.budget_kib {
            return Err(OriginError::BudgetExhausted);
        }
        let delta_kib = target_kib.saturating_sub(self.reserved_kib);
        if delta_kib > 0 {
            let acquired = Arc::clone(&self.budget.permits)
                .try_acquire_many_owned(delta_kib)
                .map_err(|_| OriginError::BudgetExhausted)?;
            match &mut self.permit {
                Some(existing) => existing.merge(acquired),
                None => self.permit = Some(acquired),
            }
            self.reserved_kib = target_kib;
        }

        let delta_bytes = target_total_bytes.saturating_sub(self.reserved_bytes);
        self.reserved_bytes = target_total_bytes;
        self.budget
            .live_bytes
            .fetch_add(delta_bytes, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        if self.reserved_bytes > 0 {
            self.budget
                .live_bytes
                .fetch_sub(self.reserved_bytes, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incremental_reservation_tracks_live_bytes_and_releases_on_drop() {
        let live = Arc::new(AtomicUsize::new(0));
        let budget = ByteBudget::new(4096, Arc::clone(&live));
        let mut res = budget.begin(Duration::from_millis(50));

        res.reserve_total(1500).await.unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 1500);
        assert_eq!(budget.semaphore().available_permits(), 2);

        res.reserve_total(3500).await.unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 3500);
        assert_eq!(budget.semaphore().available_permits(), 0);

        // Exceeding total budget fails fast without corrupting live counter.
        assert_eq!(
            res.reserve_total(5000).await,
            Err(OriginError::BudgetExhausted)
        );
        assert_eq!(live.load(Ordering::Relaxed), 3500);

        drop(res);
        assert_eq!(live.load(Ordering::Relaxed), 0);
        assert_eq!(budget.semaphore().available_permits(), 4);
    }
}
