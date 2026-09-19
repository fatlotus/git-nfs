use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;

/// An in-memory LRU cache for decompressed Git objects and blocks.
pub struct MemoryCache {
    cache: Mutex<LruCache<String, Arc<Vec<u8>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl MemoryCache {
    /// Creates a new MemoryCache with a maximum capacity (number of objects).
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("non-zero capacity");
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Gets a blob from the in-memory cache by OID.
    pub fn get(&self, oid: &str) -> Option<Arc<Vec<u8>>> {
        let mut cache = self.cache.lock();
        if let Some(data) = cache.get(oid) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(data.clone())
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Puts a blob into the in-memory cache by OID.
    pub fn put(&self, oid: &str, data: Arc<Vec<u8>>) {
        let mut cache = self.cache.lock();
        cache.put(oid.to_string(), data);
    }

    /// Puts multiple blobs into the cache.
    #[allow(dead_code)]
    pub fn put_many<I>(&self, items: I)
    where
        I: IntoIterator<Item = (String, Arc<Vec<u8>>)>,
    {
        let mut cache = self.cache.lock();
        for (oid, data) in items {
            cache.put(oid, data);
        }
    }

    /// Returns the cache hit count.
    #[allow(dead_code)]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Returns the cache miss count.
    #[allow(dead_code)]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Clears the memory cache.
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut cache = self.cache.lock();
        cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_cache_lru() {
        let cache = MemoryCache::new(2);
        let b1 = Arc::new(vec![1, 2, 3]);
        let b2 = Arc::new(vec![4, 5, 6]);
        let b3 = Arc::new(vec![7, 8, 9]);

        cache.put("oid1", b1.clone());
        cache.put("oid2", b2.clone());

        assert_eq!(cache.get("oid1").unwrap().as_slice(), &[1, 2, 3]);
        assert_eq!(cache.hits(), 1);

        // Insert b3 -> should evict oid2 (since oid1 was recently accessed)
        cache.put("oid3", b3.clone());
        assert!(cache.get("oid2").is_none());
        assert_eq!(cache.misses(), 1);

        assert!(cache.get("oid1").is_some());
        assert!(cache.get("oid3").is_some());
    }
}
