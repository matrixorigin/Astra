//! Bounded display cache for immutable, validated Edge results. It is not an
//! execution or authorization owner; cache misses use the canonical dispatch.

use crate::orchestration::spawner::ToolOutcomeInspectionError;
use astra_text_utils::credential_redaction::SafeOutputProjection;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
// Allow for the bounded observation and identity in addition to the output.
const MAX_VIEW_BYTES: usize =
    astra_services::multi_agent::edge_dispatch::MAX_INSPECTED_EDGE_RESULT_BYTES as usize
        + 16 * 1024;
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

fn cache() -> &'static Mutex<Vec<Entry>> {
    static CACHE: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn lookup_in(entries: &mut Vec<Entry>, key: &Key) -> Option<Arc<ToolOutcomeView>> {
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

pub(super) fn can_load_continuation() -> bool {
    can_load_continuation_in(&mut cache().lock().unwrap_or_else(|error| error.into_inner()))
}

fn can_load_continuation_in(entries: &mut Vec<Entry>) -> bool {
    entries.retain(|entry| entry.inserted.elapsed() < TTL);
    // A different Server may already have returned a cursor. Avoid repeatedly
    // loading its complete result while this Server cannot admit a large view.
    entries.len() < MAX_ENTRIES
        && entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES - MAX_VIEW_BYTES
}

pub(super) fn page(
    key: Key,
    view: Arc<ToolOutcomeView>,
    offset: usize,
    max_bytes: usize,
) -> Result<Value, ToolOutcomeInspectionError> {
    page_with_cache(cache(), key, view, offset, max_bytes)
}

fn page_with_cache(
    cache: &Mutex<Vec<Entry>>,
    key: Key,
    view: Arc<ToolOutcomeView>,
    offset: usize,
    max_bytes: usize,
) -> Result<Value, ToolOutcomeInspectionError> {
    let mut page = view.page(offset, max_bytes)?;
    if page
        .pointer("/result/next_offset")
        .is_some_and(Value::is_number)
        && !insert_into(
            &mut cache.lock().unwrap_or_else(|error| error.into_inner()),
            key,
            view,
        )
    {
        page["result"]["continuation_note"] = json!(
            "Further output may be temporarily unavailable because inspection is at capacity. Try again later; inspection does not rerun the tool."
        );
    }
    Ok(page)
}

fn insert_into(entries: &mut Vec<Entry>, key: Key, view: Arc<ToolOutcomeView>) -> bool {
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
        return false;
    }
    entries.retain(|entry| entry.inserted.elapsed() < TTL);
    if entries.iter().any(|entry| entry.key == key) {
        return true;
    }
    if entries.len() >= MAX_ENTRIES
        || entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_BYTES
    {
        return false;
    }
    entries.push(Entry {
        key,
        inserted: Instant::now(),
        bytes,
        view,
    });
    true
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
        let mut entries = Vec::new();
        let key = |owner: &str, request: usize| {
            [
                owner.into(),
                format!("session-{request}"),
                format!("run-{request}"),
                "turn".into(),
                request.to_string(),
            ]
        };
        let view = Arc::new(ToolOutcomeView {
            observation: json!({"result":{"status":"completed"}}),
            output: Some(SafeOutputProjection::new("observed output".into())),
        });
        for request in 0..MAX_ENTRIES {
            assert!(insert_into(
                &mut entries,
                key("owner", request),
                view.clone()
            ));
        }
        assert!(!insert_into(
            &mut entries,
            key("owner", MAX_ENTRIES),
            view.clone()
        ));
        assert!(lookup_in(&mut entries, &key("owner", 0)).is_some());
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(lookup_in(&mut entries, &key("other-owner", MAX_ENTRIES - 1)).is_none());
        assert!(lookup_in(&mut entries, &key("owner", MAX_ENTRIES - 1)).is_some());
        entries.last_mut().unwrap().inserted = Instant::now() - TTL;
        assert!(lookup_in(&mut entries, &key("owner", MAX_ENTRIES - 1)).is_none());
        assert!(insert_into(&mut entries, key("owner", MAX_ENTRIES), view));
        entries.clear();
        let large = Arc::new(ToolOutcomeView {
            observation: json!({"result":{"status":"completed"}}),
            output: Some(SafeOutputProjection::new("x".repeat(MAX_BYTES / 2))),
        });
        assert!(insert_into(&mut entries, key("owner", 1), large.clone()));
        assert!(!insert_into(&mut entries, key("owner", 2), large));
        assert_eq!(
            entries.len(),
            1,
            "byte budget applies independently of entry count"
        );
        assert!(entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES);
    }

    #[test]
    fn interleaved_large_results_do_not_evict_active_pages() {
        let cache = Mutex::new(Vec::new());
        let key = |request: usize| {
            [
                "owner".into(),
                format!("session-{request}"),
                format!("run-{request}"),
                "turn".into(),
                request.to_string(),
            ]
        };
        let mut admitted = Vec::new();
        for request in 0..33 {
            let view = Arc::new(ToolOutcomeView {
                observation: json!({"result":{"status":"completed"}}),
                output: Some(SafeOutputProjection::new(format!(
                    "{request:02}{}",
                    "x".repeat(1_100_000)
                ))),
            });
            let page = page_with_cache(&cache, key(request), view, 0, 4096).unwrap();
            assert_eq!(page["result"]["status"], "completed");
            assert!(page["result"]["next_offset"].is_number());
            if page["result"]["continuation_note"].is_null() {
                admitted.push(request);
            } else {
                assert!(
                    page["result"]["continuation_note"]
                        .as_str()
                        .unwrap()
                        .contains("does not rerun")
                );
            }
        }
        assert!(admitted.len() < 33, "exercise byte pressure");
        let mut entries = cache.lock().unwrap();
        assert!(!can_load_continuation_in(&mut entries));
        let mut hits = 0;
        for request in (0..33).cycle().take(66) {
            if let Some(view) = lookup_in(&mut entries, &key(request)) {
                let first = view.page(0, 4096).unwrap();
                let next = first["result"]["next_offset"].as_u64().unwrap() as usize;
                let second = view.page(next, 4096).unwrap();
                assert!(
                    first["result"]["output"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("{request:02}"))
                );
                assert_eq!(second["result"]["offset"], next);
                hits += 1;
            } else {
                assert!(!can_load_continuation_in(&mut entries));
            }
        }
        assert_eq!(
            hits,
            admitted.len() * 2,
            "admitted readers must not evict each other"
        );
        for entry in entries.iter_mut() {
            entry.inserted = Instant::now() - TTL;
        }
        assert!(can_load_continuation_in(&mut entries));
    }

    #[test]
    fn single_page_result_does_not_reserve_cache_space() {
        let cache = Mutex::new(Vec::new());
        let view = Arc::new(ToolOutcomeView {
            observation: json!({"result":{"status":"completed"}}),
            output: Some(SafeOutputProjection::new("short output".into())),
        });
        let page = page_with_cache(
            &cache,
            ["u", "s", "r", "t", "q"].map(String::from),
            view,
            0,
            4096,
        )
        .unwrap();
        assert_eq!(page["result"]["output"], "short output");
        assert!(page["result"]["next_offset"].is_null());
        assert!(cache.lock().unwrap().is_empty());
    }
}
