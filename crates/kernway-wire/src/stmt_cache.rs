//! A bounded cache of server-side prepared statements, keyed by SQL text.

use std::collections::{HashMap, VecDeque};

/// Prepared statements by SQL text, at most `cap` of them, evicting the oldest.
///
/// `S` is the driver's handle for a statement (MySQL: a numeric id and its
/// parameter count; Postgres: a statement name). Eviction hands the evicted
/// handle back so the driver can close it on the server its own way — keeping
/// server-side statement memory bounded under dynamic SQL (e.g. varying `IN`
/// lists).
pub struct StmtCache<S> {
    map: HashMap<String, S>,
    order: VecDeque<String>,
    cap: usize,
}

impl<S: Clone> StmtCache<S> {
    /// An empty cache holding at most `cap` statements (at least one).
    pub fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), cap: cap.max(1) }
    }

    /// The cached statement for `sql`, if any.
    #[inline]
    pub fn get(&self, sql: &str) -> Option<S> {
        self.map.get(sql).cloned()
    }

    /// Whether `sql` is cached.
    pub fn contains(&self, sql: &str) -> bool {
        self.map.contains_key(sql)
    }

    /// The statement to evict before inserting a new one, if the cache is full —
    /// removed from the cache; the driver closes it on the server. Call this
    /// **before** preparing the new statement, so a failed close leaves the
    /// cache consistent.
    pub fn evict_for_insert(&mut self) -> Option<S> {
        if self.map.len() < self.cap {
            return None;
        }
        let oldest = self.order.pop_front()?;
        self.map.remove(&oldest)
    }

    /// Remember `stmt` as the prepared form of `sql` (replacing any previous one).
    pub fn insert(&mut self, sql: &str, stmt: S) {
        if self.map.insert(sql.to_string(), stmt).is_none() {
            self.order.push_back(sql.to_string());
        }
    }

    /// Forget `sql` (the server no longer knows it, so there is nothing to close).
    pub fn remove(&mut self, sql: &str) -> Option<S> {
        let removed = self.map.remove(sql);
        if removed.is_some() {
            self.order.retain(|s| s != sql);
        }
        removed
    }

    /// Empty the cache, handing back every statement (for the driver to
    /// close on the server), oldest first.
    pub fn drain(&mut self) -> Vec<S> {
        let order = std::mem::take(&mut self.order);
        order.into_iter().filter_map(|k| self.map.remove(&k)).collect()
    }

    /// Number of cached statements.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_oldest_once_full_and_hands_it_back() {
        let mut c = StmtCache::new(2);
        assert_eq!(c.evict_for_insert(), None);
        c.insert("a", 1);
        c.insert("b", 2);
        assert_eq!(c.evict_for_insert(), Some(1), "oldest goes first");
        c.insert("c", 3);
        assert_eq!((c.get("a"), c.get("b"), c.get("c")), (None, Some(2), Some(3)));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn re_inserting_and_removing_keep_the_order_consistent() {
        let mut c = StmtCache::new(2);
        c.insert("a", 1);
        c.insert("a", 9); // replace, not a second entry
        c.insert("b", 2);
        assert_eq!(c.remove("a"), Some(9));
        assert_eq!(c.remove("a"), None);
        assert_eq!(c.evict_for_insert(), None, "one slot free after the removal");
        c.insert("c", 3);
        assert_eq!(c.evict_for_insert(), Some(2));
        assert!(c.contains("c") && !c.is_empty());
        assert_eq!(StmtCache::<u8>::new(0).capacity(), 1);
    }
}
