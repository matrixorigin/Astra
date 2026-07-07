//! Mutex lock recovery utility.
//!
//! Provides a `LockRecovery` extension trait that converts a poisoned
//! [`std::sync::Mutex`] into the inner guard without panicking.
//! In production a poisoned mutex means another thread panicked while
//! holding the lock — almost always an irrecoverable invariant violation
//! for the poisoned data — but cascading the panic into yet another
//! thread only adds noise.  We recover the inner guard so the surviving
//! operation can complete (or log and abort gracefully at a safe point).

use std::sync::{Mutex, MutexGuard};

/// Extension trait that recovers a poisoned lock instead of panicking.
pub trait LockRecovery<T> {
    /// Lock the mutex, returning the guard even when the mutex is poisoned.
    ///
    /// # Panics
    ///
    /// Never panics (blocking lock on the same thread is programmer error
    /// and remains a panic via `std::sync::Mutex::lock`).
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockRecovery<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        astra_core::sync_poison::recover_mutex_lock(&self)
    }
}

#[cfg(test)]
mod tests {
    use super::LockRecovery;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn lock_recover_unpoisoned() {
        let m = Mutex::new(42u32);
        assert_eq!(*m.lock_recover(), 42);
    }

    #[test]
    fn lock_recover_poisoned() {
        let m = Arc::new(Mutex::new(42u32));
        let m2 = Arc::clone(&m);
        let handle = thread::spawn(move || {
            let _guard = m2.lock_recover();
            panic!("simulated panic while holding lock");
        });
        let _ = handle.join(); // poison the mutex
        // Should recover the inner value without panicking
        assert_eq!(*m.lock_recover(), 42);
    }
}
