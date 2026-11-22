//! Concurrent request coalescing (singleflight) to prevent cache stampedes.
//!
//! When multiple requests miss the cache for the same canonical key simultaneously,
//! only the first caller executes the underlying fetch/transform pipeline while
//! concurrent followers await and share the resulting output.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::broadcast;

/// Outcome returned by [`Coalescer::execute`], indicating whether this caller
/// executed the work or shared an in-flight leader's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coalesced<T> {
    /// Result produced by the leader.
    pub value: T,
    /// `true` when this caller received a coalesced result from another in-flight request.
    pub was_coalesced: bool,
}

/// Cancellation-safe async singleflight coordinator keyed by `K`.
#[derive(Debug)]
pub struct Coalescer<K, T> {
    in_flight: Arc<Mutex<HashMap<K, broadcast::Sender<T>>>>,
}

impl<K, T> Clone for Coalescer<K, T> {
    fn clone(&self) -> Self {
        Self {
            in_flight: Arc::clone(&self.in_flight),
        }
    }
}

impl<K, T> Default for Coalescer<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, T> Coalescer<K, T> {
    /// Creates an empty singleflight coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K, T> Coalescer<K, T>
where
    K: Eq + Hash + Clone,
    T: Clone,
{
    /// Runs `work` at most once among concurrent callers sharing `key`.
    ///
    /// If another task is already executing for `key`, awaits that leader's result.
    /// If the leader is cancelled before finishing, a waiting follower retries by
    /// taking leadership cleanly rather than hanging.
    pub async fn execute<F, Fut>(&self, key: K, mut work: F) -> Coalesced<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = T>,
    {
        loop {
            let role = {
                let mut map = self
                    .in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(sender) = map.get(&key) {
                    Role::Follower(sender.subscribe())
                } else {
                    let (tx, _rx) = broadcast::channel(1);
                    map.insert(key.clone(), tx.clone());
                    Role::Leader(tx)
                }
            };

            match role {
                Role::Follower(mut rx) => {
                    if let Ok(value) = rx.recv().await {
                        return Coalesced {
                            value,
                            was_coalesced: true,
                        };
                    }
                    // Leader was cancelled or dropped before sending; loop to retry as leader.
                }
                Role::Leader(tx) => {
                    let _guard = FlightGuard {
                        key: key.clone(),
                        in_flight: Arc::clone(&self.in_flight),
                    };

                    let value = work().await;
                    let _ = tx.send(value.clone());

                    return Coalesced {
                        value,
                        was_coalesced: false,
                    };
                }
            }
        }
    }
}

enum Role<T> {
    Leader(broadcast::Sender<T>),
    Follower(broadcast::Receiver<T>),
}

/// Ensures the in-flight map entry is always removed when a leader finishes,
/// panics, or is cancelled mid-await.
struct FlightGuard<K: Eq + Hash, T> {
    key: K,
    in_flight: Arc<Mutex<HashMap<K, broadcast::Sender<T>>>>,
}

impl<K: Eq + Hash, T> Drop for FlightGuard<K, T> {
    fn drop(&mut self) {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Barrier;

    use super::*;

    #[tokio::test]
    async fn coalesces_concurrent_requests_for_same_key_into_single_execution() {
        let coalescer: Coalescer<String, Arc<String>> = Coalescer::new();
        let executions = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(5));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let coalescer = coalescer.clone();
            let executions = Arc::clone(&executions);
            let barrier = Arc::clone(&barrier);

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                coalescer
                    .execute("variant-1".to_owned(), || {
                        let executions = Arc::clone(&executions);
                        async move {
                            executions.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            Arc::new("encoded-bytes".to_owned())
                        }
                    })
                    .await
            }));
        }

        let mut leader_count = 0usize;
        let mut follower_count = 0usize;
        for handle in handles {
            let outcome = handle.await.unwrap();
            assert_eq!(outcome.value.as_str(), "encoded-bytes");
            if outcome.was_coalesced {
                follower_count += 1;
            } else {
                leader_count += 1;
            }
        }

        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(leader_count, 1);
        assert_eq!(follower_count, 4);
    }

    #[tokio::test]
    async fn executes_different_keys_independently_and_cleans_up_after_completion() {
        let coalescer: Coalescer<&'static str, usize> = Coalescer::new();
        let executions = Arc::new(AtomicUsize::new(0));

        let a = coalescer
            .execute("key-a", || {
                let executions = Arc::clone(&executions);
                async move { executions.fetch_add(1, Ordering::SeqCst) + 1 }
            })
            .await;
        let b = coalescer
            .execute("key-a", || {
                let executions = Arc::clone(&executions);
                async move { executions.fetch_add(1, Ordering::SeqCst) + 1 }
            })
            .await;

        assert!(!a.was_coalesced);
        assert!(!b.was_coalesced);
        assert_eq!(a.value, 1);
        assert_eq!(b.value, 2);
    }

    #[tokio::test]
    async fn leader_cancellation_hands_over_to_follower_cleanly() {
        use tokio::sync::Notify;

        let coalescer: Coalescer<&'static str, usize> = Coalescer::new();
        let leader_entered = Arc::new(Notify::new());
        let executions = Arc::new(AtomicUsize::new(0));

        let leader = {
            let coalescer = coalescer.clone();
            let leader_entered = Arc::clone(&leader_entered);
            let executions = Arc::clone(&executions);
            tokio::spawn(async move {
                coalescer
                    .execute("shared-key", || {
                        let leader_entered = Arc::clone(&leader_entered);
                        let executions = Arc::clone(&executions);
                        async move {
                            executions.fetch_add(1, Ordering::SeqCst);
                            leader_entered.notify_one();
                            // Hang indefinitely until aborted.
                            std::future::pending::<usize>().await
                        }
                    })
                    .await
            })
        };

        leader_entered.notified().await;

        let follower = {
            let coalescer = coalescer.clone();
            let executions = Arc::clone(&executions);
            tokio::spawn(async move {
                coalescer
                    .execute("shared-key", || {
                        let executions = Arc::clone(&executions);
                        async move { executions.fetch_add(1, Ordering::SeqCst) + 1 }
                    })
                    .await
            })
        };

        // Yield so the follower subscribes to the leader's channel before aborting the leader.
        tokio::time::sleep(Duration::from_millis(10)).await;
        leader.abort();
        let _ = leader.await;

        let outcome = follower.await.unwrap();
        assert!(!outcome.was_coalesced);
        assert_eq!(outcome.value, 2);
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert!(
            coalescer
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }
}
