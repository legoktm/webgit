use std::collections::BTreeMap;

/// A map bounded by the total size of its values rather than their count,
/// evicting the least recently used entries once that size is exceeded.
///
/// Each entry's size is whatever the caller says it is at insertion, so the
/// same structure bounds a cache of file pages and one of whole objects.
pub struct ByteLru<K, V> {
    entries: BTreeMap<K, LruEntry<V>>,
    /// Every entry's key under the tick it was last used at, oldest first.
    by_age: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
    limit: usize,
}

struct LruEntry<V> {
    value: V,
    bytes: usize,
    tick: u64,
}

impl<K: Ord + Copy, V> ByteLru<K, V> {
    /// An empty map holding at most `limit` bytes of values.
    pub fn new(limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            by_age: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            limit,
        }
    }

    /// An empty map that never evicts.
    pub fn unbounded() -> Self {
        Self::new(usize::MAX)
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Whether `key` is present, without counting as a use of it.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// The value under `key`, marking it as the most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        self.by_age.remove(&entry.tick);
        self.by_age.insert(tick, *key);
        entry.tick = tick;
        Some(&entry.value)
    }

    /// Insert `value`, which costs `bytes` against the limit, then evict the
    /// least recently used entries until the map fits again. A value larger
    /// than the whole limit is not kept at all, rather than emptying the map
    /// to make room for it and then evicting it too.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        self.remove(&key);
        if bytes > self.limit {
            return;
        }
        let tick = self.next_tick();
        self.entries.insert(key, LruEntry { value, bytes, tick });
        self.by_age.insert(tick, key);
        self.bytes += bytes;
        while self.bytes > self.limit {
            let Some((_, oldest)) = self.by_age.pop_first() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes -= evicted.bytes;
            }
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(entry) = self.entries.remove(key) {
            self.by_age.remove(&entry.tick);
            self.bytes -= entry.bytes;
        }
    }

    /// The total size of the values held, as reported at insertion.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_first() {
        let mut lru = ByteLru::new(3);
        lru.insert(1, "a", 1);
        lru.insert(2, "b", 1);
        lru.insert(3, "c", 1);
        // Using 1 makes 2 the oldest.
        assert_eq!(lru.get(&1), Some(&"a"));
        lru.insert(4, "d", 1);
        assert!(lru.contains_key(&1));
        assert!(!lru.contains_key(&2));
        assert!(lru.contains_key(&3));
        assert!(lru.contains_key(&4));
        assert_eq!(lru.bytes(), 3);
    }

    #[test]
    fn evicts_as_many_as_it_takes() {
        let mut lru = ByteLru::new(4);
        lru.insert(1, (), 2);
        lru.insert(2, (), 2);
        lru.insert(3, (), 3);
        assert!(!lru.contains_key(&1));
        assert!(!lru.contains_key(&2));
        assert!(lru.contains_key(&3));
        assert_eq!(lru.bytes(), 3);
    }

    #[test]
    fn replacing_a_key_does_not_double_count_it() {
        let mut lru = ByteLru::new(10);
        lru.insert(1, "a", 4);
        lru.insert(1, "b", 6);
        assert_eq!(lru.bytes(), 6);
        assert_eq!(lru.get(&1), Some(&"b"));
    }

    #[test]
    fn a_value_bigger_than_the_limit_is_not_kept() {
        let mut lru = ByteLru::new(4);
        lru.insert(1, (), 2);
        lru.insert(2, (), 5);
        assert!(lru.contains_key(&1));
        assert!(!lru.contains_key(&2));
        assert_eq!(lru.bytes(), 2);
    }
}
