//! Internal sync helpers.
//!
//! `lock_recover` unwraps a `Mutex` lock result, recovering from poisoning
//! instead of panicking. Rayon workers can panic — a single panicked worker
//! holding any of our caches would otherwise cascade-kill every subsequent
//! `.lock().unwrap()` and bring down the run. We never use the guarded data
//! across an assumed-consistent invariant that a panic could break (each
//! cache entry is independent), so recovering the inner value is safe.

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poison) => {
            tracing::warn!("recovering from poisoned mutex");
            poison.into_inner()
        }
    }
}
