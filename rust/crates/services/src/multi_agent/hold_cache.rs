//! Hold cache: tracks which tasks this process has successfully leased.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Best-effort map of which task IDs this process has successfully
/// leased for each `agent_id`.
#[derive(Default)]
pub struct TaskLeaseHoldCache {
    inner: Mutex<HashMap<String, HashSet<String>>>,
}

impl TaskLeaseHoldCache {
    pub fn record_hold(&self, agent_id: &str, task_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.entry(agent_id.to_string())
                .or_default()
                .insert(task_id.to_string());
        }
    }

    pub fn release_hold(&self, agent_id: &str, task_id: &str) {
        if let Ok(mut g) = self.inner.lock()
            && let Some(set) = g.get_mut(agent_id)
        {
            set.remove(task_id);
        }
    }

    pub fn held_task_ids_for_agent(&self, agent_id: &str) -> HashSet<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(agent_id).cloned())
            .unwrap_or_default()
    }
}
