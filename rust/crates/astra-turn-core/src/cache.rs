use serde_json::{Map, Value};

use crate::lru_map::LruMap;

#[derive(Clone, Debug, Default)]
pub struct SessionCache {
    ttl: f64,
    entries: LruMap<String, CacheEntry>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    value: Map<String, Value>,
    touched_at: f64,
}

impl SessionCache {
    pub fn new(maxsize: usize, ttl: f64) -> Self {
        Self {
            ttl,
            entries: LruMap::new(maxsize),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Map<String, Value>, now: f64) {
        let key = key.into();
        self.entries.insert(
            key,
            CacheEntry {
                value,
                touched_at: now,
            },
        );
    }

    pub fn get(&mut self, key: &str, now: f64) -> Option<Map<String, Value>> {
        let key = key.to_string();
        let expired = match self.entries.get(&key) {
            Some(entry) => now - entry.touched_at > self.ttl,
            None => return None,
        };

        if expired {
            self.entries.remove(&key);
            return None;
        }

        let mut entry = self
            .entries
            .remove(&key)
            .expect("entry exists after expiry check");
        entry.touched_at = now;
        let value = entry.value.clone();
        self.entries.insert(key, entry);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty_map() -> Map<String, Value> {
        Map::new()
    }

    // --- insert ---

    #[test]
    fn insert_empty_cache() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 100.0);
        assert!(cache.get("a", 100.0).is_some());
    }

    #[test]
    fn insert_does_not_mutate_value_with_cache_metadata() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 42.0);
        let entry = cache.get("a", 42.0).unwrap();
        assert!(!entry.contains_key("ts"));
    }

    #[test]
    fn insert_preserves_domain_ts_field() {
        let mut cache = SessionCache::new(3, 60.0);
        let mut m = Map::new();
        m.insert("ts".to_string(), Value::from(10.0));
        cache.insert("a", m, 42.0);
        let entry = cache.get("a", 42.0).unwrap();
        assert_eq!(entry["ts"].as_f64().unwrap(), 10.0);
    }

    #[test]
    fn insert_evicts_oldest_at_maxsize() {
        let mut cache = SessionCache::new(2, 60.0);
        cache.insert("a", empty_map(), 1.0);
        cache.insert("b", empty_map(), 2.0);
        cache.insert("c", empty_map(), 3.0);
        assert!(cache.get("a", 3.0).is_none());
        assert!(cache.get("b", 3.0).is_some());
        assert!(cache.get("c", 3.0).is_some());
    }

    #[test]
    fn insert_existing_key_moves_to_end() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 1.0);
        cache.insert("b", empty_map(), 2.0);
        cache.insert("a", empty_map(), 3.0);
        assert_eq!(cache.entries.order(), vec!["b", "a"]);
    }

    // --- get ---

    #[test]
    fn get_nonexistent() {
        let mut cache = SessionCache::new(3, 60.0);
        assert!(cache.get("nope", 0.0).is_none());
    }

    #[test]
    fn get_expired_entry() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 0.0);
        assert!(cache.get("a", 61.0).is_none());
        // entry should be removed
        assert!(!cache.entries.contains_key(&"a".to_string()));
    }

    #[test]
    fn get_valid_refreshes_ttl_without_mutating_value() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 10.0);
        let entry = cache.get("a", 20.0).unwrap();
        assert!(!entry.contains_key("ts"));
        assert!(cache.get("a", 79.0).is_some());
    }

    #[test]
    fn get_moves_to_end_of_lru() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 1.0);
        cache.insert("b", empty_map(), 2.0);
        cache.insert("c", empty_map(), 3.0);
        cache.get("a", 4.0);
        assert_eq!(cache.entries.order().last().unwrap(), "a");
    }

    #[test]
    fn get_at_exact_ttl_boundary_expired() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 0.0);
        // TTL=60, now - ts > ttl means 60.001 > 60 → expired
        assert!(cache.get("a", 60.001).is_none());
    }

    #[test]
    fn get_at_exact_ttl_boundary_valid() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 0.0);
        // now - ts = 60.0, which is NOT > 60.0
        assert!(cache.get("a", 60.0).is_some());
    }

    // --- LRU integration ---

    #[test]
    fn lru_access_prevents_eviction() {
        let mut cache = SessionCache::new(2, 1000.0);
        cache.insert("a", empty_map(), 1.0);
        cache.insert("b", empty_map(), 2.0);
        // access "a" to move it to end
        cache.get("a", 3.0);
        // insert "c" should evict "b" (now oldest)
        cache.insert("c", empty_map(), 4.0);
        assert!(cache.get("a", 4.0).is_some());
        assert!(cache.get("b", 4.0).is_none());
        assert!(cache.get("c", 4.0).is_some());
    }
}
