//! Thread-safe process environment overlay.
//!
//! Avoids `std::env::set_var` (unsound under Rust 2024 when other threads read the environment).
//! Values are visible through [`get`] / [`merged_pairs`] and to child processes when
//! [`apply_to_command`] is used on a [`std::process::Command`].

use std::collections::HashMap;
use std::process::Command;
use std::sync::RwLock;

static ENV_OVERLAY: RwLock<Option<HashMap<String, Option<String>>>> = RwLock::new(None);

fn overlay_read() -> std::sync::RwLockReadGuard<'static, Option<HashMap<String, Option<String>>>> {
    ENV_OVERLAY.read().unwrap_or_else(|p| {
        crate::agent_warn!("session_env", "ENV_OVERLAY read lock poisoned, recovering");
        p.into_inner()
    })
}

fn overlay_write() -> std::sync::RwLockWriteGuard<'static, Option<HashMap<String, Option<String>>>>
{
    ENV_OVERLAY.write().unwrap_or_else(|p| {
        crate::agent_warn!("session_env", "ENV_OVERLAY write lock poisoned, recovering");
        p.into_inner()
    })
}

/// Read an env var: overlay entry wins; then falls back to `std::env::var`.
#[must_use]
pub fn get(name: &str) -> Option<String> {
    let guard = overlay_read();
    if let Some(ref map) = *guard
        && let Some(entry) = map.get(name)
    {
        return entry.clone();
    }
    std::env::var(name).ok()
}

/// Set a value in the overlay (does not touch the real process environment).
pub fn set(name: &str, value: &str) {
    overlay_write()
        .get_or_insert_with(HashMap::new)
        .insert(name.to_string(), Some(value.to_string()));
}

/// Mark a name as removed in the overlay (hides real env for child commands that use [`apply_to_command`]).
pub fn remove(name: &str) {
    overlay_write()
        .get_or_insert_with(HashMap::new)
        .insert(name.to_string(), None);
}

/// Real environment merged with overlay (overlay wins; `None` entries delete keys).
#[must_use]
pub fn merged_pairs() -> Vec<(String, String)> {
    let guard = overlay_read();
    let mut result: HashMap<String, String> = std::env::vars().collect();
    if let Some(ref map) = *guard {
        for (k, v) in map {
            match v {
                Some(val) => {
                    result.insert(k.clone(), val.clone());
                }
                None => {
                    result.remove(k);
                }
            }
        }
    }
    let mut pairs: Vec<_> = result.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// Apply overlay entries to a [`Command`] for spawned children.
pub fn apply_to_command(cmd: &mut Command) {
    let guard = overlay_read();
    if let Some(ref map) = *guard {
        for (k, v) in map {
            match v {
                Some(val) => {
                    cmd.env(k, val);
                }
                None => {
                    cmd.env_remove(k);
                }
            }
        }
    }
}
