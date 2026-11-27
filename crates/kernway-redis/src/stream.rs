//! Redis Streams — typed replies for `XREADGROUP`, `XAUTOCLAIM`, `XPENDING`, ….
//!
//! A stream used with a consumer group is a **durable** queue, unlike pub/sub: an
//! entry stays in the group's pending list until a consumer `XACK`s it, and a
//! consumer that died mid-work has its entries reclaimed with `XAUTOCLAIM`. The
//! typed helpers live on [`Connection`](crate::Connection) (`xadd`, `xreadgroup`,
//! …); this module holds the reply types and the pure `Value` → type parsing, kept
//! free of I/O so it is unit-tested against hand-built replies.
//!
//! RESP2 shapes handled here:
//!
//! ```text
//! entry        = [id, [field, value, field, value, …]]   (fields may be nil:
//!                                                          a deleted entry still
//!                                                          in the pending list)
//! XREADGROUP   = nil (BLOCK timed out) | [[key, [entry…]], …]
//! XAUTOCLAIM   = [next-start, [entry…]] (+ [deleted-id…] on Redis ≥ 7)
//! XPENDING k g = [count, smallest-id, largest-id, [[consumer, count]…]]
//! ```

use crate::error::RedisError;
use crate::resp::Value;

/// One stream entry: its ID and its field/value pairs, in insertion order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntry {
    /// The entry ID (`"1700000000000-0"`).
    pub id: String,
    /// The field/value pairs. Empty for an entry that was deleted while still
    /// pending (Redis returns its fields as nil).
    pub fields: Vec<(String, Vec<u8>)>,
}

impl StreamEntry {
    /// The value of the first field named `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_slice())
    }
}

/// The entries read from one stream key (one element of an `XREADGROUP` reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntries {
    /// The stream key.
    pub key: String,
    /// Its entries, oldest first.
    pub entries: Vec<StreamEntry>,
}

fn bulk_string(cmd: &str, v: &Value) -> Result<String, RedisError> {
    match v {
        Value::Bulk(b) => String::from_utf8(b.clone())
            .map_err(|_| RedisError::Protocol(format!("{cmd}: non-UTF-8 string in reply"))),
        Value::Simple(s) => Ok(s.clone()),
        other => Err(RedisError::unexpected(cmd, other)),
    }
}

/// Parse one `[id, [field, value, …]]` entry.
pub fn parse_entry(cmd: &str, v: &Value) -> Result<StreamEntry, RedisError> {
    let Value::Array(parts) = v else {
        return Err(RedisError::unexpected(cmd, v));
    };
    let [id, fields] = parts.as_slice() else {
        return Err(RedisError::unexpected(cmd, v));
    };
    let id = bulk_string(cmd, id)?;
    let fields = match fields {
        Value::Nil => Vec::new(),
        Value::Array(flat) => {
            if flat.len() % 2 != 0 {
                return Err(RedisError::unexpected(cmd, v));
            }
            flat.chunks_exact(2)
                .map(|kv| {
                    let name = bulk_string(cmd, &kv[0])?;
                    match &kv[1] {
                        Value::Bulk(b) => Ok((name, b.clone())),
                        Value::Simple(s) => Ok((name, s.clone().into_bytes())),
                        other => Err(RedisError::unexpected(cmd, other)),
                    }
                })
                .collect::<Result<_, _>>()?
        }
        other => return Err(RedisError::unexpected(cmd, other)),
    };
    Ok(StreamEntry { id, fields })
}

/// Parse an array of entries. Nil elements (deleted entries, as Redis 6.2's
/// `XAUTOCLAIM` reports them) are skipped; a nil array is empty.
pub fn parse_entries(cmd: &str, v: &Value) -> Result<Vec<StreamEntry>, RedisError> {
    match v {
        Value::Nil => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .filter(|item| !matches!(item, Value::Nil))
            .map(|item| parse_entry(cmd, item))
            .collect(),
        other => Err(RedisError::unexpected(cmd, other)),
    }
}

/// Parse an `XREAD`/`XREADGROUP` reply. Nil (a `BLOCK` that timed out, or nothing
/// new) is an empty vec.
pub fn parse_read_reply(v: &Value) -> Result<Vec<StreamEntries>, RedisError> {
    const CMD: &str = "XREADGROUP";
    match v {
        Value::Nil => Ok(Vec::new()),
        // RESP3: a map of stream key → entries.
        Value::Map(streams) => streams
            .iter()
            .map(|(key, entries)| Ok(StreamEntries { key: bulk_string(CMD, key)?, entries: parse_entries(CMD, entries)? }))
            .collect(),
        Value::Array(streams) => streams
            .iter()
            .map(|s| match s {
                Value::Array(pair) => match pair.as_slice() {
                    [key, entries] => Ok(StreamEntries {
                        key: bulk_string(CMD, key)?,
                        entries: parse_entries(CMD, entries)?,
                    }),
                    _ => Err(RedisError::unexpected(CMD, s)),
                },
                other => Err(RedisError::unexpected(CMD, other)),
            })
            .collect(),
        other => Err(RedisError::unexpected(CMD, other)),
    }
}

/// Parse an `XAUTOCLAIM` reply into `(next start id, claimed entries)`. The
/// Redis ≥ 7 third element (IDs of entries deleted meanwhile) is ignored — Redis
/// already dropped them from the pending list. A next start of `"0-0"` means the
/// scan has wrapped around the whole pending list.
pub fn parse_autoclaim_reply(v: &Value) -> Result<(String, Vec<StreamEntry>), RedisError> {
    const CMD: &str = "XAUTOCLAIM";
    let Value::Array(parts) = v else {
        return Err(RedisError::unexpected(CMD, v));
    };
    match parts.as_slice() {
        [next, entries] | [next, entries, _] => {
            Ok((bulk_string(CMD, next)?, parse_entries(CMD, entries)?))
        }
        _ => Err(RedisError::unexpected(CMD, v)),
    }
}

/// Parse the summary form of `XPENDING key group` into the total pending count.
pub fn parse_pending_count(v: &Value) -> Result<i64, RedisError> {
    const CMD: &str = "XPENDING";
    match v {
        Value::Array(parts) => match parts.first() {
            Some(Value::Int(n)) => Ok(*n),
            _ => Err(RedisError::unexpected(CMD, v)),
        },
        other => Err(RedisError::unexpected(CMD, other)),
    }
}

/// Whether a server error is `BUSYGROUP` (the consumer group already exists).
#[must_use]
pub fn is_busygroup(err: &RedisError) -> bool {
    matches!(err, RedisError::Server(msg) if msg.starts_with("BUSYGROUP"))
}

// --- command builders (pure, so the argument layout is unit-tested) ---------

/// `XADD key [MAXLEN ~ n] id field value …`.
pub(crate) fn xadd_args(
    key: &str,
    id: &str,
    fields: &[(&str, &[u8])],
    maxlen_approx: Option<u64>,
) -> Vec<Vec<u8>> {
    let mut args: Vec<Vec<u8>> = vec![b"XADD".to_vec(), key.into()];
    if let Some(n) = maxlen_approx {
        args.extend([
            b"MAXLEN".to_vec(),
            b"~".to_vec(),
            n.to_string().into_bytes(),
        ]);
    }
    args.push(id.into());
    for (name, value) in fields {
        args.push(name.as_bytes().to_vec());
        args.push(value.to_vec());
    }
    args
}

/// `XREADGROUP GROUP g c [COUNT n] [BLOCK ms] STREAMS k1 k2 … id1 id2 …`.
pub(crate) fn xreadgroup_args(
    group: &str,
    consumer: &str,
    streams: &[(&str, &str)],
    count: Option<usize>,
    block_ms: Option<u64>,
) -> Vec<Vec<u8>> {
    let mut args: Vec<Vec<u8>> = vec![
        b"XREADGROUP".to_vec(),
        b"GROUP".to_vec(),
        group.into(),
        consumer.into(),
    ];
    if let Some(n) = count {
        args.extend([b"COUNT".to_vec(), n.to_string().into_bytes()]);
    }
    if let Some(ms) = block_ms {
        args.extend([b"BLOCK".to_vec(), ms.to_string().into_bytes()]);
    }
    args.push(b"STREAMS".to_vec());
    args.extend(streams.iter().map(|(key, _)| key.as_bytes().to_vec()));
    args.extend(streams.iter().map(|(_, id)| id.as_bytes().to_vec()));
    args
}

/// Borrow an owned argument list as the `&[&[u8]]` that `command` takes.
pub(crate) fn as_slices(args: &[Vec<u8>]) -> Vec<&[u8]> {
    args.iter().map(Vec::as_slice).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::Bulk(s.as_bytes().to_vec())
    }

    fn entry(id: &str, fields: &[(&str, &str)]) -> Value {
        Value::Array(vec![
            bulk(id),
            Value::Array(
                fields
                    .iter()
                    .flat_map(|(k, v)| [bulk(k), bulk(v)])
                    .collect(),
            ),
        ])
    }

    #[test]
    fn parses_an_entry_with_fields_in_order() {
        let e = parse_entry("T", &entry("1-0", &[("type", "move"), ("uci", "e2e4")])).unwrap();
        assert_eq!(e.id, "1-0");
        assert_eq!(
            e.fields,
            vec![
                ("type".to_string(), b"move".to_vec()),
                ("uci".to_string(), b"e2e4".to_vec())
            ]
        );
        assert_eq!(e.get("uci"), Some(&b"e2e4"[..]));
        assert_eq!(e.get("missing"), None);
    }

    #[test]
    fn an_entry_with_nil_fields_is_empty() {
        // A pending entry that was XDEL'd comes back as [id, nil].
        let e = parse_entry("T", &Value::Array(vec![bulk("5-0"), Value::Nil])).unwrap();
        assert_eq!(e.id, "5-0");
        assert!(e.fields.is_empty());
    }

    #[test]
    fn a_malformed_entry_is_an_error() {
        // Odd number of field/value items.
        let odd = Value::Array(vec![bulk("1-0"), Value::Array(vec![bulk("k")])]);
        assert!(matches!(
            parse_entry("T", &odd),
            Err(RedisError::Unexpected(_))
        ));
        // Not an array at all.
        assert!(parse_entry("T", &Value::Int(1)).is_err());
        // Missing the fields element.
        assert!(parse_entry("T", &Value::Array(vec![bulk("1-0")])).is_err());
    }

    #[test]
    fn parses_a_multi_stream_read_reply() {
        let reply = Value::Array(vec![
            Value::Array(vec![
                bulk("games"),
                Value::Array(vec![
                    entry("1-0", &[("a", "1")]),
                    entry("2-0", &[("a", "2")]),
                ]),
            ]),
            Value::Array(vec![bulk("chat"), Value::Array(vec![])]),
        ]);
        let got = parse_read_reply(&reply).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].key, "games");
        assert_eq!(
            got[0]
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["1-0", "2-0"]
        );
        assert_eq!(got[0].entries[1].get("a"), Some(&b"2"[..]));
        assert_eq!(got[1].key, "chat");
        assert!(got[1].entries.is_empty());
    }

    #[test]
    fn a_nil_read_reply_is_empty() {
        assert_eq!(parse_read_reply(&Value::Nil).unwrap(), Vec::new());
        assert!(parse_read_reply(&Value::Int(3)).is_err());
    }

    #[test]
    fn parses_autoclaim_reply_redis6_and_redis7() {
        // Redis 6.2: [next, entries] — a deleted entry may appear as nil.
        let v6 = Value::Array(vec![
            bulk("0-0"),
            Value::Array(vec![entry("3-0", &[("k", "v")]), Value::Nil]),
        ]);
        let (next, claimed) = parse_autoclaim_reply(&v6).unwrap();
        assert_eq!(next, "0-0");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, "3-0");

        // Redis 7: [next, entries, deleted-ids].
        let v7 = Value::Array(vec![
            bulk("9-0"),
            Value::Array(vec![entry("4-0", &[("k", "w")])]),
            Value::Array(vec![bulk("2-0")]),
        ]);
        let (next, claimed) = parse_autoclaim_reply(&v7).unwrap();
        assert_eq!(next, "9-0");
        assert_eq!(claimed[0].get("k"), Some(&b"w"[..]));

        assert!(parse_autoclaim_reply(&Value::Array(vec![bulk("0-0")])).is_err());
    }

    #[test]
    fn parses_pending_summary_count() {
        let summary = Value::Array(vec![
            Value::Int(2),
            bulk("1-0"),
            bulk("2-0"),
            Value::Array(vec![Value::Array(vec![bulk("worker-1"), bulk("2")])]),
        ]);
        assert_eq!(parse_pending_count(&summary).unwrap(), 2);
        // Nothing pending: [0, nil, nil, nil].
        let empty = Value::Array(vec![Value::Int(0), Value::Nil, Value::Nil, Value::Nil]);
        assert_eq!(parse_pending_count(&empty).unwrap(), 0);
        assert!(parse_pending_count(&Value::Nil).is_err());
    }

    fn strs(args: &[Vec<u8>]) -> Vec<&str> {
        args.iter()
            .map(|a| std::str::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn xadd_args_with_and_without_maxlen() {
        let plain = xadd_args("s", "*", &[("a", b"1"), ("b", b"2")], None);
        assert_eq!(strs(&plain), ["XADD", "s", "*", "a", "1", "b", "2"]);
        let capped = xadd_args("s", "*", &[("a", b"1")], Some(1000));
        assert_eq!(
            strs(&capped),
            ["XADD", "s", "MAXLEN", "~", "1000", "*", "a", "1"]
        );
    }

    #[test]
    fn xreadgroup_args_put_all_keys_before_all_ids() {
        let args = xreadgroup_args("g", "c1", &[("s1", ">"), ("s2", "0")], Some(10), Some(5000));
        assert_eq!(
            strs(&args),
            [
                "XREADGROUP",
                "GROUP",
                "g",
                "c1",
                "COUNT",
                "10",
                "BLOCK",
                "5000",
                "STREAMS",
                "s1",
                "s2",
                ">",
                "0"
            ]
        );
        let bare = xreadgroup_args("g", "c1", &[("s", ">")], None, None);
        assert_eq!(
            strs(&bare),
            ["XREADGROUP", "GROUP", "g", "c1", "STREAMS", "s", ">"]
        );
    }

    #[test]
    fn recognises_busygroup() {
        let busy = RedisError::Server("BUSYGROUP Consumer Group name already exists".into());
        assert!(is_busygroup(&busy));
        assert!(!is_busygroup(&RedisError::Server("ERR no such key".into())));
        assert!(!is_busygroup(&RedisError::Closed));
    }
}
