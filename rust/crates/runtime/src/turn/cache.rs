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
