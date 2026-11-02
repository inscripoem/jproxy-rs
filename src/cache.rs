use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

struct Entry<V> {
    inserted_at: Instant,
    value: V,
}

/// Small in-process TTL cache with bounded entry count.
///
/// Expiry is lazy: an entry is removed when read after its TTL, and expired
/// entries are purged before capacity eviction.
pub struct TtlCache<V> {
    ttl: Duration,
    capacity: usize,
    entries: RwLock<HashMap<String, Entry<V>>>,
}

impl<V: Clone> TtlCache<V> {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get(&self, key: &str) -> Option<V> {
        if self.ttl.is_zero() {
            return None;
        }
        let mut entries = self.entries.write().await;
        match entries.get(key) {
            Some(entry) if entry.inserted_at.elapsed() < self.ttl => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub async fn insert(&self, key: String, value: V) {
        if self.ttl.is_zero() || self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.write().await;
        entries.retain(|_, entry| entry.inserted_at.elapsed() < self.ttl);
        if entries.len() >= self.capacity
            && !entries.contains_key(&key)
            && let Some(victim) = entries.keys().next().cloned()
        {
            entries.remove(&victim);
        }
        entries.insert(
            key,
            Entry {
                inserted_at: Instant::now(),
                value,
            },
        );
    }

    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }

    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn entries_expire_after_ttl() {
        let cache = TtlCache::new(Duration::from_millis(15), 4);
        cache.insert("key".to_owned(), "value".to_owned()).await;
        assert_eq!(cache.get("key").await.as_deref(), Some("value"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(cache.get("key").await, None);
        assert_eq!(cache.len().await, 0);
    }

    #[tokio::test]
    async fn zero_ttl_disables_cache() {
        let cache = TtlCache::new(Duration::ZERO, 4);
        cache.insert("key".to_owned(), 1_u8).await;
        assert_eq!(cache.get("key").await, None);
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn capacity_is_bounded() {
        let cache = TtlCache::new(Duration::from_secs(10), 2);
        cache.insert("a".to_owned(), 1_u8).await;
        cache.insert("b".to_owned(), 2_u8).await;
        cache.insert("c".to_owned(), 3_u8).await;
        assert_eq!(cache.len().await, 2);
    }
}
