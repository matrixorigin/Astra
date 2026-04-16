use std::collections::HashMap;

use serde_json::{Map, Value};

#[derive(Clone, Debug, Default)]
pub struct SessionCache {
    maxsize: usize,
    ttl: f64,
    order: Vec<String>,
    entries: HashMap<String, Map<String, Value>>,
}

impl SessionCache {
    pub fn new(maxsize: usize, ttl: f64) -> Self {
        Self {
            maxsize,
            ttl,
            order: Vec::new(),
            entries: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, mut value: Map<String, Value>, now: f64) {
        let key = key.into();
        value.entry("ts".to_string()).or_insert(Value::from(now));

        self.remove_order_key(&key);
        self.order.push(key.clone());
        self.entries.insert(key.clone(), value);

        while self.order.len() > self.maxsize {
            if let Some(oldest) = self.order.first().cloned() {
                self.order.remove(0);
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn get(&mut self, key: &str, now: f64) -> Option<Map<String, Value>> {
        let expired = match self.entries.get(key) {
            Some(entry) => now - entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > self.ttl,
            None => return None,
        };

        if expired {
            self.entries.remove(key);
            self.remove_order_key(key);
            return None;
        }

        let entry = self
            .entries
            .get_mut(key)
            .expect("entry exists after expiry check");
        entry.insert("ts".to_string(), Value::from(now));
        let cloned = entry.clone();
        self.remove_order_key(key);
        self.order.push(key.to_string());
        Some(cloned)
    }

    fn remove_order_key(&mut self, key: &str) {
        if let Some(index) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn insert_sets_ts_if_missing() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 42.0);
        let entry = cache.get("a", 42.0).unwrap();
        assert_eq!(entry["ts"].as_f64().unwrap(), 42.0);
    }

    #[test]
    fn insert_preserves_existing_ts() {
        let mut cache = SessionCache::new(3, 60.0);
        let mut m = Map::new();
        m.insert("ts".to_string(), json!(10.0));
        cache.insert("a", m, 42.0);
        let entry = cache.entries.get("a").unwrap();
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
        assert_eq!(cache.order, vec!["b", "a"]);
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
        assert!(!cache.entries.contains_key("a"));
    }

    #[test]
    fn get_valid_updates_ts() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 10.0);
        let entry = cache.get("a", 20.0).unwrap();
        assert_eq!(entry["ts"].as_f64().unwrap(), 20.0);
    }

    #[test]
    fn get_moves_to_end_of_lru() {
        let mut cache = SessionCache::new(3, 60.0);
        cache.insert("a", empty_map(), 1.0);
        cache.insert("b", empty_map(), 2.0);
        cache.insert("c", empty_map(), 3.0);
        cache.get("a", 4.0);
        assert_eq!(cache.order.last().unwrap(), "a");
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
