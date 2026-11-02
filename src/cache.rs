use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

struct Entry<V> {
    inserted_at: Instant,
    value: V,
}

/// Caffeine-compatible expiry mode.
///
/// Java uses `expireAfterAccess` for the general cache manager and
/// `expireAfterCreate` for the indexer-result and sync-interval caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    AfterCreate,
    AfterAccess,
}

/// Small in-process TTL cache with bounded entry count.
///
/// Expiry is lazy: an entry is removed when read after its TTL, and expired
/// entries are purged before capacity eviction. An `AfterAccess` cache resets
/// the timer on every successful read, matching Caffeine.
pub struct TtlCache<V> {
    ttl: Duration,
    capacity: usize,
    expiry: Expiry,
    entries: RwLock<HashMap<String, Entry<V>>>,
}

impl<V: Clone> TtlCache<V> {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self::with_expiry(ttl, capacity, Expiry::AfterCreate)
    }

    pub fn with_expiry(ttl: Duration, capacity: usize, expiry: Expiry) -> Self {
        Self {
            ttl,
            capacity,
            expiry,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get(&self, key: &str) -> Option<V> {
        if self.ttl.is_zero() {
            return None;
        }
        let mut entries = self.entries.write().await;
        match entries.get_mut(key) {
            Some(entry) if entry.inserted_at.elapsed() < self.ttl => {
                if self.expiry == Expiry::AfterAccess {
                    entry.inserted_at = Instant::now();
                }
                Some(entry.value.clone())
            }
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
    async fn after_access_reads_refresh_the_timer() {
        let cache = TtlCache::with_expiry(Duration::from_millis(80), 4, Expiry::AfterAccess);
        cache.insert("key".to_owned(), 1_u8).await;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(cache.get("key").await, Some(1));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cache.get("key").await, None);
    }

    #[tokio::test]
    async fn after_create_reads_do_not_refresh_the_timer() {
        let cache = TtlCache::with_expiry(Duration::from_millis(80), 4, Expiry::AfterCreate);
        cache.insert("key".to_owned(), 1_u8).await;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert_eq!(cache.get("key").await, None);
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
