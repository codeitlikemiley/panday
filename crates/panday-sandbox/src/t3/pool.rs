//! Warm microVM pools (docs/14 M14.6).
//!
//! > "snapshot/restore gives warm-start pools and *session resume with running processes* — a
//! > product feature, not just an optimization."
//!
//! A cold Firecracker boot is ~125ms; a snapshot restore is a fraction of that. The pool exists to
//! turn the second number into the one users experience, and the whole design follows from two
//! rules that are not about speed at all:
//!
//! - **A VM never serves two sessions.** On return it is destroyed and the pool refills from the
//!   golden snapshot. Reuse would be faster and would mean one stranger's code inherits another's
//!   memory, page cache and open file descriptors — which is the thing T3 exists to prevent. This
//!   is why `checkin` takes the VM by value: there is no API for handing it back.
//! - **An empty pool is a slow request, never a failed one.** Under a burst the pool falls back to
//!   a cold boot rather than blocking on a refill. A queue would convert a traffic spike into a
//!   timeout for everybody rather than into latency for the unlucky.
//!
//! Backed by a trait, so the pool's behaviour is tested without KVM. What the fake cannot tell us is
//! how fast a real restore is; what it does tell us is that we never hand out a VM twice, never
//! leak one, and never wedge when the backend fails — which is where pool managers actually go
//! wrong.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A running microVM the pool is responsible for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledVm {
    pub id: String,
    /// True when it came from a snapshot rather than a cold boot. Carried so a caller can report
    /// the hit rate honestly rather than inferring it from timing.
    pub warm: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("backend: {0}")]
    Backend(String),
    #[error("the pool is shutting down")]
    ShuttingDown,
}

/// What the pool needs from the world.
#[async_trait::async_trait]
pub trait VmBackend: Send + Sync {
    /// Restore a paused VM from the golden snapshot. Paused, so nothing runs until it is handed
    /// out — a warm VM that started executing while it waited would drift from its snapshot.
    async fn restore(&self) -> Result<PooledVm, PoolError>;
    /// Boot from scratch, for when the pool is empty.
    async fn cold_boot(&self) -> Result<PooledVm, PoolError>;
    /// Let a restored VM run.
    async fn resume(&self, vm: &PooledVm) -> Result<(), PoolError>;
    /// End it. Called on every return, without exception.
    async fn destroy(&self, vm: PooledVm) -> Result<(), PoolError>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    pub warm_hits: u64,
    pub cold_starts: u64,
    pub destroyed: u64,
    /// Refills that failed. A pool that cannot refill still serves — slowly — and this is the
    /// number that says so before anyone notices the latency.
    pub refill_failures: u64,
}

/// A pool of paused, ready microVMs.
pub struct WarmPool {
    backend: Arc<dyn VmBackend>,
    target: usize,
    idle: Mutex<VecDeque<PooledVm>>,
    warm_hits: AtomicU64,
    cold_starts: AtomicU64,
    destroyed: AtomicU64,
    refill_failures: AtomicU64,
}

impl WarmPool {
    pub fn new(backend: Arc<dyn VmBackend>, target: usize) -> Self {
        Self {
            backend,
            target,
            idle: Mutex::new(VecDeque::new()),
            warm_hits: AtomicU64::new(0),
            cold_starts: AtomicU64::new(0),
            destroyed: AtomicU64::new(0),
            refill_failures: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            warm_hits: self.warm_hits.load(Ordering::Relaxed),
            cold_starts: self.cold_starts.load(Ordering::Relaxed),
            destroyed: self.destroyed.load(Ordering::Relaxed),
            refill_failures: self.refill_failures.load(Ordering::Relaxed),
        }
    }

    pub fn idle_count(&self) -> usize {
        self.idle.lock().unwrap().len()
    }

    /// Fill the pool to `target`. Called at start-up and after each checkout.
    ///
    /// Failures are counted rather than propagated: a pool that could not refill still serves from
    /// cold boots, and turning a transient backend hiccup into a failed *user request* would be
    /// worse than being slow.
    pub async fn refill(&self) {
        loop {
            let missing = {
                let idle = self.idle.lock().unwrap();
                self.target.saturating_sub(idle.len())
            };
            if missing == 0 {
                return;
            }
            match self.backend.restore().await {
                Ok(vm) => self.idle.lock().unwrap().push_back(vm),
                Err(e) => {
                    // Counted rather than logged from here: this crate deliberately has no logging
                    // dependency (a sandbox should not be able to write to the host's log by
                    // running), and `PoolStats::refill_failures` is what a caller alarms on.
                    let _ = e;
                    self.refill_failures.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    /// Take a VM, warm if one is ready and cold otherwise.
    ///
    /// The returned VM is resumed and running. A caller that received a paused VM would have to
    /// know which kind it got, and that distinction is exactly what the pool exists to hide.
    pub async fn checkout(&self) -> Result<PooledVm, PoolError> {
        let warm = self.idle.lock().unwrap().pop_front();

        let vm = match warm {
            Some(vm) => {
                self.warm_hits.fetch_add(1, Ordering::Relaxed);
                self.backend.resume(&vm).await?;
                PooledVm { warm: true, ..vm }
            }
            None => {
                // An empty pool is a slow request, not a failed one. Waiting for a refill would
                // turn a burst into a timeout for everybody instead of latency for the unlucky.
                self.cold_starts.fetch_add(1, Ordering::Relaxed);
                PooledVm {
                    warm: false,
                    ..self.backend.cold_boot().await?
                }
            }
        };
        Ok(vm)
    }

    /// Give a VM back. It is destroyed, always.
    ///
    /// By value, because there is deliberately no way to return one for reuse: a VM that ran one
    /// stranger's code must never serve another, and an API that made reuse *possible* would make
    /// it eventually happen.
    pub async fn checkin(&self, vm: PooledVm) -> Result<(), PoolError> {
        let result = self.backend.destroy(vm).await;
        self.destroyed.fetch_add(1, Ordering::Relaxed);
        result
    }

    /// Destroy every idle VM. Called on shutdown; leaving them running would leak a machine's worth
    /// of memory per pool.
    pub async fn drain(&self) {
        loop {
            let vm = self.idle.lock().unwrap().pop_front();
            match vm {
                Some(vm) => {
                    let _ = self.backend.destroy(vm).await;
                    self.destroyed.fetch_add(1, Ordering::Relaxed);
                }
                None => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out numbered VMs and records everything it was asked to do.
    #[derive(Default)]
    struct FakeBackend {
        next: AtomicU64,
        restored: AtomicU64,
        cold: AtomicU64,
        destroyed: Mutex<Vec<String>>,
        resumed: Mutex<Vec<String>>,
        fail_restore: bool,
    }

    #[async_trait::async_trait]
    impl VmBackend for FakeBackend {
        async fn restore(&self) -> Result<PooledVm, PoolError> {
            if self.fail_restore {
                return Err(PoolError::Backend("no snapshot".into()));
            }
            self.restored.fetch_add(1, Ordering::Relaxed);
            Ok(PooledVm {
                id: format!("warm-{}", self.next.fetch_add(1, Ordering::Relaxed)),
                warm: true,
            })
        }
        async fn cold_boot(&self) -> Result<PooledVm, PoolError> {
            self.cold.fetch_add(1, Ordering::Relaxed);
            Ok(PooledVm {
                id: format!("cold-{}", self.next.fetch_add(1, Ordering::Relaxed)),
                warm: false,
            })
        }
        async fn resume(&self, vm: &PooledVm) -> Result<(), PoolError> {
            self.resumed.lock().unwrap().push(vm.id.clone());
            Ok(())
        }
        async fn destroy(&self, vm: PooledVm) -> Result<(), PoolError> {
            self.destroyed.lock().unwrap().push(vm.id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_warm_checkout_comes_from_the_pool_and_is_resumed() {
        let backend = Arc::new(FakeBackend::default());
        let pool = WarmPool::new(backend.clone(), 2);
        pool.refill().await;
        assert_eq!(pool.idle_count(), 2);

        let vm = pool.checkout().await.unwrap();
        assert!(vm.warm);
        assert_eq!(
            backend.resumed.lock().unwrap().as_slice(),
            std::slice::from_ref(&vm.id)
        );
        assert_eq!(pool.stats().warm_hits, 1);
        assert_eq!(pool.stats().cold_starts, 0);
    }

    #[tokio::test]
    async fn an_empty_pool_is_slow_rather_than_broken() {
        // A queue would turn a traffic spike into a timeout for everybody instead of latency for
        // the unlucky.
        let backend = Arc::new(FakeBackend::default());
        let pool = WarmPool::new(backend.clone(), 0);

        let vm = pool.checkout().await.unwrap();
        assert!(!vm.warm);
        assert_eq!(pool.stats().cold_starts, 1);
        assert_eq!(backend.cold.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_returned_vm_is_destroyed_and_never_handed_out_again() {
        // The rule the whole tier rests on: one stranger's code must not inherit another's memory,
        // page cache or file descriptors.
        let backend = Arc::new(FakeBackend::default());
        let pool = WarmPool::new(backend.clone(), 1);
        pool.refill().await;

        let first = pool.checkout().await.unwrap();
        pool.checkin(first.clone()).await.unwrap();
        assert_eq!(
            backend.destroyed.lock().unwrap().as_slice(),
            std::slice::from_ref(&first.id)
        );

        pool.refill().await;
        let second = pool.checkout().await.unwrap();
        assert_ne!(second.id, first.id, "a destroyed VM came back");
    }

    #[tokio::test]
    async fn the_pool_refills_to_target_and_no_further() {
        let backend = Arc::new(FakeBackend::default());
        let pool = WarmPool::new(backend.clone(), 3);
        pool.refill().await;
        pool.refill().await;
        assert_eq!(pool.idle_count(), 3);
        assert_eq!(backend.restored.load(Ordering::Relaxed), 3, "over-filled");
    }

    #[tokio::test]
    async fn a_backend_that_cannot_refill_still_serves() {
        // A pool that could not refill turning into failed user requests would be worse than one
        // that is merely slow — and the failure count is what says so before anyone notices.
        let backend = Arc::new(FakeBackend {
            fail_restore: true,
            ..Default::default()
        });
        let pool = WarmPool::new(backend.clone(), 2);
        pool.refill().await;

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.stats().refill_failures, 1);

        let vm = pool.checkout().await.unwrap();
        assert!(!vm.warm, "it fell back to a cold boot");
    }

    #[tokio::test]
    async fn draining_leaves_nothing_running() {
        // A pool that shut down without draining leaks a machine's worth of memory.
        let backend = Arc::new(FakeBackend::default());
        let pool = WarmPool::new(backend.clone(), 4);
        pool.refill().await;
        pool.drain().await;

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(backend.destroyed.lock().unwrap().len(), 4);
        assert_eq!(pool.stats().destroyed, 4);
    }

    #[tokio::test]
    async fn every_vm_handed_out_is_distinct() {
        // The property a pool is most likely to break under concurrency: handing the same VM to
        // two callers.
        let backend = Arc::new(FakeBackend::default());
        let pool = Arc::new(WarmPool::new(backend.clone(), 8));
        pool.refill().await;

        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move { pool.checkout().await.unwrap() }));
        }
        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.unwrap().id);
        }
        ids.sort();
        let distinct = {
            let mut d = ids.clone();
            d.dedup();
            d.len()
        };
        assert_eq!(distinct, 8, "a VM was handed out twice: {ids:?}");
    }
}
