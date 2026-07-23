//! Name/value pairs of a request — one buffer, many ranges.
//!
//! Backs both the headers and the query parameters. They are the same problem
//! wearing different names: a short list of `(name, value)` pairs, cut out of
//! bytes that have just been read, looked up a handful of times, then dropped
//! with the request. Only the case rule differs, so that is the one thing
//! [`Fields`] is generic over — resolved at compile time, so a lookup pays no
//! branch for it.
//!
//! The obvious model is `HashMap<String, String>`, and that is what this
//! replaced. On a typical browser request (8 headers) it cost ~690ns of the
//! ~1.2µs spent parsing: ~295ns allocating 16 short `String`s, ~167ns hashing
//! them, and ~226ns freeing them again when the request was dropped. The
//! scanning of the bytes themselves was only ~196ns. A query string of two
//! parameters cost a further ~175ns for the same reason. Measured with
//! `kernway-http`'s `parse` benches.
//!
//! So the cost was the container, not the parsing. This one copies the header
//! bytes once into a single `Vec<u8>` and stores `(name, value)` as ranges into
//! it: one allocation for the whole set, and dropping a request frees one
//! buffer rather than walking sixteen.
//!
//! Lookup is a linear scan rather than a hash. At the counts real requests
//! carry that is not a compromise — comparing a handful of short byte strings
//! beats hashing one, and it removes the `to_lowercase` allocation the old
//! header `get` made on *every* lookup.

use std::ops::Range;

/// A `(name, value)` pair, as ranges into [`Fields::buf`].
///
/// `u32` rather than `usize` keeps this to 16 bytes; the parser caps a head at
/// 8KiB long before four gigabytes becomes a question.
#[derive(Debug, Clone)]
struct Entry {
    name: Range<u32>,
    value: Range<u32>,
}

/// A set of `(name, value)` pairs cut from one buffer.
///
/// `CASE_INSENSITIVE` says whether names are matched — and stored — without
/// regard to case. Prefer the two aliases to naming it directly: [`Headers`],
/// where RFC 9110 §5.1 makes field names case-insensitive, and [`QueryParams`],
/// where they are not (`?Page=2` and `?page=2` are different parameters).
#[derive(Default, Clone)]
pub struct Fields<const CASE_INSENSITIVE: bool> {
    /// The name and value text. Bytes belonging to a replaced pair stay here as
    /// dead weight — see [`Fields::insert`].
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

/// The headers of a request. Names are matched and stored case-insensitively.
pub type Headers = Fields<true>;

/// The query parameters of a request. Names keep their case and must match it.
pub type QueryParams = Fields<false>;

impl<const CASE_INSENSITIVE: bool> Fields<CASE_INSENSITIVE> {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty set sized for `count` pairs occupying about `bytes` bytes.
    ///
    /// The parser knows both up front, which is what makes one allocation per
    /// request possible instead of one per pair.
    pub fn with_capacity(bytes: usize, count: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            entries: Vec::with_capacity(count),
        }
    }

    /// Add `name: value`, replacing any pair of the same name.
    ///
    /// Replacement drops the old entry but leaves its bytes in the buffer:
    /// compacting would invalidate every range after it, and a replaced pair
    /// is rare enough that a few dead bytes are the cheaper trade.
    ///
    /// # Panics
    /// If the accumulated text would exceed 4GiB.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.entries.retain(|e| !Self::name_matches(&self.buf, e, name));
        self.push(name, value);
    }

    /// Add `name: value` without checking for an existing pair of that name.
    ///
    /// The parse path uses this: it walks the input in order, and a repeated
    /// name there means the *last* one wins, which falls out of the reverse
    /// scan in [`Fields::get`] without paying for a duplicate check per pair.
    ///
    /// # Panics
    /// If the accumulated text would exceed 4GiB.
    pub fn append(&mut self, name: &str, value: &str) {
        self.push(name, value);
    }

    fn push(&mut self, name: &str, value: &str) {
        let name_start = Self::offset(self.buf.len());
        if CASE_INSENSITIVE {
            // Normalized once here so that `get` never has to allocate a
            // lowercased copy of the name it is handed.
            self.buf.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        } else {
            self.buf.extend_from_slice(name.as_bytes());
        }
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

    /// The value of `name`, if present.
    ///
    /// Scanned in reverse so that when a name appears twice the last one wins,
    /// matching what a `HashMap` built by inserting in order would hold.
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

    /// Every pair, in the order it was added.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|e| (self.slice(&e.name), self.slice(&e.value)))
    }

    /// How many pairs are stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set is empty.
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
        if CASE_INSENSITIVE {
            stored.eq_ignore_ascii_case(name.as_bytes())
        } else {
            stored == name.as_bytes()
        }
    }
}

impl<const CASE_INSENSITIVE: bool> std::fmt::Debug for Fields<CASE_INSENSITIVE> {
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
    fn query_params_keep_their_name_case() {
        let mut q = QueryParams::new();
        q.insert("sortBy", "name");
        assert_eq!(q.get("sortBy"), Some("name"));
        assert_eq!(q.iter().map(|(n, _)| n).collect::<Vec<_>>(), vec!["sortBy"]);
    }

    #[test]
    fn query_param_lookup_is_case_sensitive() {
        // `?Page=2` and `?page=2` are different parameters — unlike headers.
        let mut q = QueryParams::new();
        q.insert("page", "2");
        assert_eq!(q.get("page"), Some("2"));
        assert_eq!(q.get("Page"), None);
    }

    #[test]
    fn query_params_of_differing_case_coexist() {
        let mut q = QueryParams::new();
        q.insert("page", "2");
        q.insert("Page", "9");
        assert_eq!(q.len(), 2, "case-sensitive names must not collapse");
        assert_eq!(q.get("page"), Some("2"));
        assert_eq!(q.get("Page"), Some("9"));
    }

    #[test]
    fn non_ascii_values_survive_intact() {
        // Names are ASCII by RFC 9110 §5.1, but values reach us as UTF-8.
        let mut h = Headers::new();
        h.insert("x-name", "Grüße-日本");
        assert_eq!(h.get("x-name"), Some("Grüße-日本"));
    }
}
