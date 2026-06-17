//! Helpers for recovering from poisoned `RwLock` guards.
//!
//! When a thread panics while holding a `RwLock`, the lock becomes poisoned
//! and subsequent lock attempts return `PoisonError`. These helpers clear
//! the poison flag and reset state to a safe default.

/// Acquire a write lock, clearing poison and resetting to `T::default()` if poisoned.
///
/// # Safety
///
/// This clears the poison flag and overwrites the guarded value with a fresh
/// default. Use only when the guarded type can be safely reset without losing
/// critical state (e.g., caches, tool activation sets).
///
/// # Panics
///
/// Panics if `T::default()` panics. The poison flag is cleared *before* the
/// default assignment, so if `default()` panics the lock remains usable.
pub fn rwlock_write_reset_on_poison<'a, T: Default>(
    lock: &'a std::sync::RwLock<T>,
    label: &str,
) -> std::sync::RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!(
                cache = label,
                "RwLock poisoned on write; clearing poison flag and resetting state"
            );
            // Clear poison FIRST: if T::default() panics below, the lock
            // remains usable for subsequent callers.
            lock.clear_poison();
            let mut guard = poisoned.into_inner();
            *guard = T::default();
            guard
        }
    }
}

/// Check if the guarded value contains a specific element, or reset to `T::default()` if poisoned.
///
/// This is a zero-clone read path optimized for `HashSet`/`Vec` containment checks.
/// Returns `Some(true/false)` for the check result, or `None` if the lock was poisoned and reset.
///
/// # Safety
///
/// This clears the poison flag and overwrites the guarded value with a fresh
/// default if poisoned. Use only when the guarded type can be safely reset without losing
/// critical state.
pub fn rwlock_check_contains_or_default<T, F>(
    lock: &std::sync::RwLock<T>,
    label: &str,
    check_fn: F,
) -> Option<bool>
where
    T: Default,
    F: FnOnce(&T) -> bool,
{
    match lock.read() {
        Ok(guard) => Some(check_fn(&*guard)),
        Err(poisoned) => {
            tracing::error!(
                cache = label,
                "RwLock poisoned on read; resetting cached state to default"
            );
            // Drop the poisoned read guard first, then acquire a write lock
            // to safely reset. Crucially, we do NOT clear the poison flag
            // until we hold the write guard: otherwise another reader could
            // acquire an `Ok(read)` between clear_poison() and write() and
            // observe the panicking thread's half-updated state.
            drop(poisoned);
            let mut guard = match lock.write() {
                Ok(g) => {
                    // Another caller already recovered (cleared poison +
                    // reset). Return their state untouched — do NOT overwrite.
                    return Some(check_fn(&*g));
                }
                Err(p) => {
                    // Still poisoned — we hold the write guard via
                    // into_inner(). Now it is safe to clear the flag and
                    // reset, since no other reader/writer can be in the
                    // critical section.
                    lock.clear_poison();
                    p.into_inner()
                }
            };
            *guard = T::default();
            None
        }
    }
}

/// Clone the guarded value, or reset to `T::default()` and return the clone if poisoned.
///
/// # Safety
///
/// This clears the poison flag and overwrites the guarded value with a fresh
/// default. Use only when the guarded type can be safely reset without losing
/// critical state.
///
/// # Panics
///
/// Panics if `T::default()` panics. The poison flag is cleared *before* the
/// default assignment, so if `default()` panics the lock remains usable.
pub fn rwlock_read_clone_or_default<T: Clone + Default>(
    lock: &std::sync::RwLock<T>,
    label: &str,
) -> T {
    match lock.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => {
            tracing::error!(
                cache = label,
                "RwLock poisoned on read; resetting cached state to default"
            );
            // Do NOT clear poison before acquiring the write guard — see
            // rwlock_check_contains_or_default for rationale.
            drop(poisoned);
            match lock.write() {
                Ok(g) => {
                    // Another caller already recovered — return their state untouched.
                    g.clone()
                }
                Err(p) => {
                    lock.clear_poison();
                    let mut guard = p.into_inner();
                    let default_val = T::default();
                    let result = default_val.clone();
                    *guard = default_val;
                    result
                }
            }
        }
    }
}
