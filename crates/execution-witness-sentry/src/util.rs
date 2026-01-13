use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

/// Value with timestamp.
struct Timestamped<V> {
    value: V,
    timestamp: Instant,
}

/// HashMap with expiring based on time.
pub struct ExpiringHashMap<K, V>
where
    K: Hash + Eq,
{
    hash_map: HashMap<K, Timestamped<V>>,
    max_age: Duration,
}

impl<K, V> ExpiringHashMap<K, V>
where
    K: Hash + Eq,
{
    /// Create a new ExpiringHashMap with the specified maximum age for hash_map.
    pub fn new(max_age: Duration) -> Self {
        Self {
            hash_map: HashMap::new(),
            max_age,
        }
    }

    /// Insert a key-value pair into the hash map.
    pub fn insert(&mut self, key: K, value: V) {
        self.hash_map.insert(
            key,
            Timestamped {
                value,
                timestamp: Instant::now(),
            },
        );
        self.cleanup();
    }

    /// Get a reference to the value associated with the key, if it exists.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.hash_map.get(key).map(|value| &value.value)
    }

    /// Remove and return the value associated with the key, if it exists.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.hash_map.remove(key).map(|value| value.value)
    }

    /// Remove all expired hash_map from the hash map.
    fn cleanup(&mut self) {
        let now = Instant::now();
        self.hash_map
            .retain(|_, v| now.duration_since(v.timestamp) < self.max_age);
    }
}
