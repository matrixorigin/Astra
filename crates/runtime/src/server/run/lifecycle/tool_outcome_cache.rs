//! Bounded display cache for immutable, validated Edge results. It is not an
//! execution or authorization owner; every miss uses the canonical dispatch.

use crate::orchestration::spawner::ToolOutcomeInspectionError;
use astra_text_utils::credential_redaction::SafeOutputProjection;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const TTL: Duration = Duration::from_secs(300);
pub(super) type Key = [String; 5]; // run owner, session, child run, turn, request

pub(super) struct ToolOutcomeView {
    pub observation: Value,
    pub output: Option<SafeOutputProjection>,
}

impl ToolOutcomeView {
    pub fn page(
        &self,
        offset: usize,
        max_bytes: usize,
    ) -> Result<Value, ToolOutcomeInspectionError> {
        let mut observation = self.observation.clone();
        let Some(projection) = &self.output else {
            return Ok(observation);
        };
        if offset > projection.total_bytes() {
            return Err(ToolOutcomeInspectionError::InvalidOffset);
        }
        let mut bytes = max_bytes.min(3498);
        let (window, end, total, _) = loop {
            let page = projection.window(offset, bytes);
            if serde_json::to_string(&page.0)
                .map_err(|error| error.to_string())?
                .len()
                <= 3500
                || bytes <= 64
            {
                break page;
            }
            bytes = (bytes / 2).max(64);
        };
        if end.saturating_sub(window.len()) != offset {
            return Err(ToolOutcomeInspectionError::UnsafePageBoundary);
        }
        let result = &mut observation["result"];
        result["output"] = json!(window);
        result["offset"] = json!(offset);
        result["next_offset"] = json!((end < total).then_some(end));
        result["total_bytes"] = json!(total);
        Ok(observation)
    }
}

struct Entry {
    key: Key,
    inserted: Instant,
    bytes: usize,
    view: Arc<ToolOutcomeView>,
}

fn cache() -> &'static Mutex<VecDeque<Entry>> {
    static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn lookup_in(entries: &mut VecDeque<Entry>, key: &Key) -> Option<Arc<ToolOutcomeView>> {
    entries.retain(|entry| entry.inserted.elapsed() < TTL);
    entries
        .iter()
        .find(|entry| &entry.key == key)
        .map(|entry| entry.view.clone())
}

pub(super) fn lookup(key: &Key) -> Option<Arc<ToolOutcomeView>> {
    lookup_in(
        &mut cache().lock().unwrap_or_else(|error| error.into_inner()),
        key,
    )
}

pub(super) fn insert(key: Key, view: Arc<ToolOutcomeView>) {
    let mut entries = cache().lock().unwrap_or_else(|error| error.into_inner());
    insert_into(&mut entries, key, view);
}

fn insert_into(entries: &mut VecDeque<Entry>, key: Key, view: Arc<ToolOutcomeView>) {
    // Include key/metadata allocations and bounded container overhead as well
    // as the text and redaction index. No raw executor output is cached.
    let bytes = key.iter().map(String::capacity).sum::<usize>()
        + serde_json::to_vec(&view.observation).map_or(MAX_BYTES, |body| body.len() * 4)
        + view
            .output
            .as_ref()
            .map_or(0, SafeOutputProjection::heap_bytes)
        + 1024;
    if bytes > MAX_BYTES {
        return;
    }
    entries.retain(|entry| entry.inserted.elapsed() < TTL && entry.key != key);
    let mut retained = entries.iter().map(|entry| entry.bytes).sum::<usize>();
    while entries.len() >= MAX_ENTRIES || retained + bytes > MAX_BYTES {
        let Some(entry) = entries.pop_front() else {
            break;
        };
        retained -= entry.bytes;
    }
    entries.push_back(Entry {
        key,
        inserted: Instant::now(),
        bytes,
        view,
    });
}

pub(super) fn response(observations: Vec<Value>, next: Option<Value>) -> Value {
    json!({"observations": observations, "next_cursor": next,
        "note": "Validated terminal results may use a bounded display cache. Dispatch status alone does not establish whether a tool ran or succeeded; read an exact result for a validated executor observation. Missing or expired records remain unknown. Inspection does not execute a tool."})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_cache_is_bounded_expiring_and_owner_scoped() {
        let mut entries = VecDeque::new();
        let key = |owner: &str, request: usize| {
            [
                owner.into(),
                "session".into(),
                "run".into(),
                "turn".into(),
                request.to_string(),
            ]
        };
        let view = Arc::new(ToolOutcomeView {
            observation: json!({"result":{"status":"completed"}}),
            output: Some(SafeOutputProjection::new("observed output".into())),
        });
        for request in 0..=MAX_ENTRIES {
            insert_into(&mut entries, key("owner", request), view.clone());
        }
        assert!(lookup_in(&mut entries, &key("owner", 0)).is_none());
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(lookup_in(&mut entries, &key("other-owner", MAX_ENTRIES)).is_none());
        assert!(lookup_in(&mut entries, &key("owner", MAX_ENTRIES)).is_some());
        entries.back_mut().unwrap().inserted = Instant::now() - TTL;
        assert!(lookup_in(&mut entries, &key("owner", MAX_ENTRIES)).is_none());
        entries.clear();
        let large = Arc::new(ToolOutcomeView {
            observation: json!({"result":{"status":"completed"}}),
            output: Some(SafeOutputProjection::new("x".repeat(MAX_BYTES / 2))),
        });
        insert_into(&mut entries, key("owner", 1), large.clone());
        insert_into(&mut entries, key("owner", 2), large);
        assert_eq!(
            entries.len(),
            1,
            "byte budget applies independently of entry count"
        );
        assert!(entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES);
    }
}
