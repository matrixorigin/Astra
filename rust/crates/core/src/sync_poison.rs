//! Recover from poisoned `std::sync` locks without panicking.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Acquire a [`Mutex`] guard, recovering from poison with logging.
#[inline]
pub fn recover_mutex_lock<'a, T>(lock: &'a Mutex<T>) -> MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        crate::agent_warn!("sync_poison", "Mutex poisoned; recovering via into_inner()");
        poisoned.into_inner()
    })
}

/// Acquire a [`RwLock`] read guard, recovering from poison with logging.
#[inline]
pub fn recover_rwlock_read<'a, T>(lock: &'a RwLock<T>) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        crate::agent_warn!(
            "sync_poison",
            "RwLock read poisoned; recovering via into_inner()"
        );
        poisoned.into_inner()
    })
}

/// Acquire a [`RwLock`] write guard, recovering from poison with logging.
#[inline]
pub fn recover_rwlock_write<'a, T>(lock: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        crate::agent_warn!(
            "sync_poison",
            "RwLock write poisoned; recovering via into_inner()"
        );
        poisoned.into_inner()
    })
}
