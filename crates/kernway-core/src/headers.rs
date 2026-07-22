//! Request headers — one buffer, many ranges.
//!
//! The obvious model is `HashMap<String, String>`, and that is what this
//! replaced. On a typical browser request (8 headers) it cost ~690ns of the
//! ~1.2µs spent parsing: ~295ns allocating 16 short `String`s, ~167ns hashing
//! them, and ~226ns freeing them again when the request was dropped. The
//! scanning of the bytes themselves was only ~196ns. Measured with
//! `kernway-http`'s `parse` benches.
//!
//! So the cost was the container, not the parsing. This one copies the header
//! bytes once into a single `Vec<u8>` and stores `(name, value)` as ranges into
//! it: one allocation for the whole set, and dropping a request frees one
//! buffer rather than walking sixteen.
//!
//! Lookup is a linear scan rather than a hash. With the header counts real
//! requests carry that is not a compromise — comparing a handful of short byte
//! strings beats hashing one, and it removes the `to_lowercase` allocation the
//! old `get` made on *every* lookup.

use std::ops::Range;

/// A `(name, value)` pair, as ranges into [`Headers::buf`].
///
/// `u32` rather than `usize` keeps this to 16 bytes; the parser caps a head at
/// 8KiB long before four gigabytes becomes a question.
#[derive(Debug, Clone)]
struct Entry {
    name: Range<u32>,
    value: Range<u32>,
}

/// The headers of a request.
///
/// Names are stored lowercased, so lookup never has to allocate a normalized
/// copy of the name it was handed.
#[derive(Default, Clone)]
pub struct Headers {
    /// Header text, names already lowercased. Bytes belonging to a replaced
    /// header stay here as dead weight — see [`Headers::insert`].
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

impl Headers {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty set sized for `count` headers occupying about `bytes` bytes.
    ///
    /// The parser knows both up front, which is what makes one allocation per
    /// request possible instead of one per header.
    pub fn with_capacity(bytes: usize, count: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            entries: Vec::with_capacity(count),
        }
    }

    /// Add `name: value`, replacing any header of the same name.
    ///
    /// Replacement drops the old entry but leaves its bytes in the buffer:
    /// compacting would invalidate every range after it, and a replaced header
    /// is rare enough that a few dead bytes are the cheaper trade.
    ///
    /// # Panics
    /// If the accumulated header text would exceed 4GiB.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.entries.retain(|e| !Self::name_matches(&self.buf, e, name));
        self.push(name, value);
    }

    /// Add `name: value` without checking for an existing header of that name.
    ///
    /// The parse path uses this: it walks the head in order, and a repeated
    /// header there means the *last* one wins, which falls out of the reverse
    /// scan in [`Headers::get`] without paying for a duplicate check per line.
    ///
    /// # Panics
    /// If the accumulated header text would exceed 4GiB.
    pub fn append(&mut self, name: &str, value: &str) {
        self.push(name, value);
    }

    fn push(&mut self, name: &str, value: &str) {
        let name_start = Self::offset(self.buf.len());
        self.buf.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        let name_end = Self::offset(self.buf.len());
        self.buf.extend_from_slice(value.as_bytes());
        let value_end = Self::offset(self.buf.len());
        self.entries.push(Entry {
            name: name_start..name_end,
            value: name_end..value_end,
        });
    }

    fn offset(len: usize) -> u32 {
        u32::try_from(len).expect("header text stays far below 4GiB")
    }

    /// The value of `name`, if present. Matching is ASCII case-insensitive.
    ///
    /// Scanned in reverse so that when a header appears twice the last one
    /// wins, matching what a `HashMap` built by inserting in order would hold.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| Self::name_matches(&self.buf, e, name))
            .map(|e| self.slice(&e.value))
    }

    /// Whether `name` is present.
    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Every header, in the order it was added.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|e| (self.slice(&e.name), self.slice(&e.value)))
    }

    /// How many headers are stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no headers.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn slice(&self, range: &Range<u32>) -> &str {
        let bytes = &self.buf[range.start as usize..range.end as usize];
        // Every write goes through `push`, which is only ever handed `&str`, so
        // any range of the buffer that a range covers is still valid UTF-8.
        std::str::from_utf8(bytes).unwrap_or("")
    }

    /// Free function rather than a method so it can be called while `entries`
    /// is mutably borrowed by `retain`.
    fn name_matches(buf: &[u8], entry: &Entry, name: &str) -> bool {
        let stored = &buf[entry.name.start as usize..entry.name.end as usize];
        stored.eq_ignore_ascii_case(name.as_bytes())
    }
}

impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Headers {
        let mut h = Headers::new();
        h.insert("Host", "example.com");
        h.insert("Content-Type", "application/json");
        h
    }

    #[test]
    fn lookup_is_case_insensitive_in_both_directions() {
        let h = sample();
        assert_eq!(h.get("host"), Some("example.com"));
        assert_eq!(h.get("HOST"), Some("example.com"));
        assert_eq!(h.get("Host"), Some("example.com"));
    }

    #[test]
    fn a_missing_header_is_none() {
        assert_eq!(sample().get("authorization"), None);
    }

    #[test]
    fn insert_replaces_rather_than_duplicating() {
        let mut h = sample();
        h.insert("host", "other.example");
        assert_eq!(h.get("host"), Some("other.example"));
        assert_eq!(h.len(), 2, "replacing must not grow the set");
    }

    #[test]
    fn replacing_leaves_no_stale_value_reachable() {
        // The old bytes stay in the buffer; what matters is that no entry
        // still points at them.
        let mut h = sample();
        h.insert("host", "other.example");
        assert_eq!(h.iter().filter(|(n, _)| *n == "host").count(), 1);
    }

    #[test]
    fn append_keeps_duplicates_and_the_last_one_wins() {
        // What the parse path does with a header sent twice.
        let mut h = Headers::new();
        h.append("x-forwarded-for", "1.1.1.1");
        h.append("x-forwarded-for", "2.2.2.2");
        assert_eq!(h.get("x-forwarded-for"), Some("2.2.2.2"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn names_are_stored_lowercased() {
        assert_eq!(
            sample().iter().map(|(n, _)| n).collect::<Vec<_>>(),
            vec!["host", "content-type"]
        );
    }

    #[test]
    fn values_keep_their_case_and_spacing() {
        let mut h = Headers::new();
        h.insert("user-agent", "Mozilla/5.0 (Macintosh)");
        assert_eq!(h.get("user-agent"), Some("Mozilla/5.0 (Macintosh)"));
    }

    #[test]
    fn an_empty_value_is_present_but_empty() {
        let mut h = Headers::new();
        h.insert("x-empty", "");
        assert_eq!(h.get("x-empty"), Some(""));
        assert!(h.contains_key("x-empty"));
    }

    #[test]
    fn empty_set_reports_itself_empty() {
        let h = Headers::new();
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
        assert_eq!(h.get("host"), None);
    }

    #[test]
    fn debug_renders_the_pairs() {
        let shown = format!("{:?}", sample());
        assert!(shown.contains("\"host\": \"example.com\""), "got {shown}");
    }

    #[test]
    fn non_ascii_values_survive_intact() {
        // Names are ASCII by RFC 9110 §5.1, but values reach us as UTF-8.
        let mut h = Headers::new();
        h.insert("x-name", "Phạm");
        assert_eq!(h.get("x-name"), Some("Phạm"));
    }
}
