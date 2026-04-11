//! Extension trait for `RwLock` that logs when recovering from poisoned locks.
//!
//! All adaptive engine components use `RwLock` with poison recovery (accepting
//! the data even if a thread panicked while holding the lock). This module
//! centralises the recovery logic so every call site gets observability for free.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Extension methods for `RwLock<T>` that log on poison recovery.
pub(crate) trait RwLockExt<T> {
    /// Acquire a read guard, recovering from poisoning with a warning log.
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T>;

    /// Acquire a write guard, recovering from poisoning with a warning log.
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    #[inline]
    #[track_caller]
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| {
            astra_core::agent_warn!(
                "lock",
                "RwLock poisoned (read), recovering — caller: {}",
                std::panic::Location::caller()
            );
            e.into_inner()
        })
    }

    #[inline]
    #[track_caller]
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| {
            astra_core::agent_warn!(
                "lock",
                "RwLock poisoned (write), recovering — caller: {}",
                std::panic::Location::caller()
            );
            e.into_inner()
        })
    }
}
