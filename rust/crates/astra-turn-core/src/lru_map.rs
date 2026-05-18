use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Clone, Debug)]
pub(crate) struct LruMap<K, V> {
    capacity: usize,
    order: VecDeque<K>,
    entries: HashMap<K, V>,
}

impl<K, V> Default for LruMap<K, V> {
    fn default() -> Self {
        Self {
            capacity: 0,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }
}

impl<K, V> LruMap<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }

        self.remove(&key);
        self.order.push_back(key.clone());
        self.entries.insert(key, value);

        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.remove_order_key(key);
        self.entries.remove(key)
    }

    pub(crate) fn clear(&mut self) {
        self.order.clear();
        self.entries.clear();
    }

    pub(crate) fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&K, &V) -> bool,
    {
        self.entries.retain(|key, value| keep(key, value));
        self.order.retain(|key| self.entries.contains_key(key));
    }

    #[cfg(test)]
    pub(crate) fn order(&self) -> Vec<K> {
        self.order.iter().cloned().collect()
    }

    fn remove_order_key(&mut self, key: &K) {
        if let Some(index) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(index);
        }
    }
}
