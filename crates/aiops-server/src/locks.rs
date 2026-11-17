//! Wait and hold times of the collector's in-process locks, so their cost on the ingest
//! hot path is measured rather than assumed (D-026). Each acquisition costs three
//! monotonic clock reads and five relaxed atomic updates; the totals and maxima are
//! exposed with the collector-wide counters (`GET /v1/collector/stats`, `admin` scope).

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Counters for one lock (or one family of locks guarding the same structure).
#[derive(Default)]
pub struct LockStats {
    acquisitions: AtomicU64,
    wait_ns: AtomicU64,
    wait_max_ns: AtomicU64,
    hold_ns: AtomicU64,
    hold_max_ns: AtomicU64,
}

/// What [`LockStats`] has seen since the collector started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LockView {
    pub acquisitions: u64,
    /// Time spent waiting for the lock, summed over every acquisition.
    pub wait_ns_total: u64,
    pub wait_ns_max: u64,
    /// Time the lock was held, summed over every acquisition.
    pub hold_ns_total: u64,
    pub hold_ns_max: u64,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl LockStats {
    /// Locks `mutex`, recording how long the caller waited; the returned guard records how
    /// long it was held when it is dropped. `None` when the mutex is poisoned (every caller
    /// already treats that as "not available").
    pub fn lock<'a, T>(&'a self, mutex: &'a Mutex<T>) -> Option<Timed<'a, T>> {
        let asked = Instant::now();
        let guard = mutex.lock().ok()?;
        let acquired = Instant::now();
        let wait = nanos(acquired.duration_since(asked));
        self.acquisitions.fetch_add(1, Ordering::Relaxed);
        self.wait_ns.fetch_add(wait, Ordering::Relaxed);
        self.wait_max_ns.fetch_max(wait, Ordering::Relaxed);
        Some(Timed {
            guard,
            stats: self,
            acquired,
        })
    }

    pub fn view(&self) -> LockView {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LockView {
            acquisitions: g(&self.acquisitions),
            wait_ns_total: g(&self.wait_ns),
            wait_ns_max: g(&self.wait_max_ns),
            hold_ns_total: g(&self.hold_ns),
            hold_ns_max: g(&self.hold_max_ns),
        }
    }
}

/// A held lock that records its hold time on drop (measured before the guard releases).
pub struct Timed<'a, T> {
    guard: MutexGuard<'a, T>,
    stats: &'a LockStats,
    acquired: Instant,
}

impl<T> Deref for Timed<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for Timed<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for Timed<'_, T> {
    fn drop(&mut self) {
        let held = nanos(self.acquired.elapsed());
        self.stats.hold_ns.fetch_add(held, Ordering::Relaxed);
        self.stats.hold_max_ns.fetch_max(held, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_acquisitions_waits_and_holds() {
        let stats = LockStats::default();
        let m = Mutex::new(0u32);
        {
            let mut g = stats.lock(&m).unwrap();
            *g += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        let v = stats.view();
        assert_eq!(v.acquisitions, 1);
        assert!(v.hold_ns_max >= 5_000_000, "held at least 5 ms: {v:?}");
        assert_eq!(v.hold_ns_total, v.hold_ns_max);
        assert_eq!(*m.lock().unwrap(), 1);
    }

    #[test]
    fn a_contended_lock_reports_the_wait() {
        let stats = std::sync::Arc::new(LockStats::default());
        let m = std::sync::Arc::new(Mutex::new(()));
        let g = stats.lock(&m).unwrap();
        let (s2, m2) = (stats.clone(), m.clone());
        let waiter = std::thread::spawn(move || drop(s2.lock(&m2)));
        std::thread::sleep(Duration::from_millis(20));
        drop(g);
        waiter.join().unwrap();
        let v = stats.view();
        assert_eq!(v.acquisitions, 2);
        assert!(
            v.wait_ns_max >= 10_000_000,
            "the second caller waited: {v:?}"
        );
    }

    #[test]
    fn a_poisoned_lock_is_none_and_counts_nothing() {
        let stats = LockStats::default();
        let m = std::sync::Arc::new(Mutex::new(()));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(stats.lock(&m).is_none());
        assert_eq!(stats.view().acquisitions, 0);
    }
}
