//! The Kafka wire protocol: primitive types (fixed-width, varints, classic
//! and *flexible* — compact — strings / arrays with tagged fields), request
//! and response headers, API keys and error codes.
//!
//! The API variants, error-code constants and the reader / writer's
//! primitive methods carry the protocol's own names (`i16`, `string`,
//! `NOT_LEADER_OR_FOLLOWER` …) and are documented by the Kafka protocol
//! guide rather than one by one.
#![allow(missing_docs)]

use kernway_messaging::MsgError;

/// API keys used by the client, with the version it speaks (all inside what
/// Kafka 3.x and 4.x both accept; the flexible encodings from these on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Produce,
    Fetch,
    ListOffsets,
    Metadata,
    OffsetCommit,
    OffsetFetch,
    FindCoordinator,
    JoinGroup,
    Heartbeat,
    LeaveGroup,
    SyncGroup,
    SaslHandshake,
    ApiVersions,
    CreateTopics,
    DeleteTopics,
    InitProducerId,
    SaslAuthenticate,
}

impl Api {
    /// `(key, version, flexible)`.
    pub const fn spec(self) -> (i16, i16, bool) {
        match self {
            Api::Produce => (0, 9, true),
            Api::Fetch => (1, 12, true),
            Api::ListOffsets => (2, 7, true),
            Api::Metadata => (3, 12, true),
            Api::OffsetCommit => (8, 8, true),
            Api::OffsetFetch => (9, 7, true),
            Api::FindCoordinator => (10, 4, true),
            Api::JoinGroup => (11, 7, true),
            Api::Heartbeat => (12, 4, true),
            Api::LeaveGroup => (13, 4, true),
            Api::SyncGroup => (14, 5, true),
            Api::SaslHandshake => (17, 1, false),
            Api::ApiVersions => (18, 3, true),
            Api::CreateTopics => (19, 5, true),
            Api::DeleteTopics => (20, 4, true),
            Api::InitProducerId => (22, 2, true),
            Api::SaslAuthenticate => (36, 2, true),
        }
    }

    /// The name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Api::Produce => "Produce",
            Api::Fetch => "Fetch",
            Api::ListOffsets => "ListOffsets",
            Api::Metadata => "Metadata",
            Api::OffsetCommit => "OffsetCommit",
            Api::OffsetFetch => "OffsetFetch",
            Api::FindCoordinator => "FindCoordinator",
            Api::JoinGroup => "JoinGroup",
            Api::Heartbeat => "Heartbeat",
            Api::LeaveGroup => "LeaveGroup",
            Api::SyncGroup => "SyncGroup",
            Api::SaslHandshake => "SaslHandshake",
            Api::ApiVersions => "ApiVersions",
            Api::CreateTopics => "CreateTopics",
            Api::DeleteTopics => "DeleteTopics",
            Api::InitProducerId => "InitProducerId",
            Api::SaslAuthenticate => "SaslAuthenticate",
        }
    }

    /// Every API the client needs (checked against ApiVersions).
    pub const ALL: [Api; 17] = [
        Api::Produce,
        Api::Fetch,
        Api::ListOffsets,
        Api::Metadata,
        Api::OffsetCommit,
        Api::OffsetFetch,
        Api::FindCoordinator,
        Api::JoinGroup,
        Api::Heartbeat,
        Api::LeaveGroup,
        Api::SyncGroup,
        Api::SaslHandshake,
        Api::ApiVersions,
        Api::CreateTopics,
        Api::DeleteTopics,
        Api::InitProducerId,
        Api::SaslAuthenticate,
    ];
}

/// A Kafka error code, with its name and whether retrying can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KafkaCode(pub i16);

impl KafkaCode {
    pub const NONE: i16 = 0;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const LEADER_NOT_AVAILABLE: i16 = 5;
    pub const NOT_LEADER_OR_FOLLOWER: i16 = 6;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const TOPIC_ALREADY_EXISTS: i16 = 36;
    pub const SASL_AUTHENTICATION_FAILED: i16 = 58;
    pub const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 45;
    pub const DUPLICATE_SEQUENCE_NUMBER: i16 = 46;
    pub const UNKNOWN_PRODUCER_ID: i16 = 59;
    pub const MEMBER_ID_REQUIRED: i16 = 79;
    pub const FENCED_LEADER_EPOCH: i16 = 74;
    pub const UNKNOWN_LEADER_EPOCH: i16 = 75;
    pub const OFFSET_NOT_AVAILABLE: i16 = 78;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
    pub const NOT_ENOUGH_REPLICAS: i16 = 19;
    pub const UNKNOWN_TOPIC_ID: i16 = 100;

    /// The protocol's name for the code.
    pub fn name(self) -> &'static str {
        match self.0 {
            -1 => "UNKNOWN_SERVER_ERROR",
            0 => "NONE",
            1 => "OFFSET_OUT_OF_RANGE",
            2 => "CORRUPT_MESSAGE",
            3 => "UNKNOWN_TOPIC_OR_PARTITION",
            4 => "INVALID_FETCH_SIZE",
            5 => "LEADER_NOT_AVAILABLE",
            6 => "NOT_LEADER_OR_FOLLOWER",
            7 => "REQUEST_TIMED_OUT",
            10 => "MESSAGE_TOO_LARGE",
            13 => "NETWORK_EXCEPTION",
            14 => "COORDINATOR_LOAD_IN_PROGRESS",
            15 => "COORDINATOR_NOT_AVAILABLE",
            16 => "NOT_COORDINATOR",
            17 => "INVALID_TOPIC_EXCEPTION",
            18 => "RECORD_LIST_TOO_LARGE",
            19 => "NOT_ENOUGH_REPLICAS",
            20 => "NOT_ENOUGH_REPLICAS_AFTER_APPEND",
            22 => "ILLEGAL_GENERATION",
            23 => "INCONSISTENT_GROUP_PROTOCOL",
            24 => "INVALID_GROUP_ID",
            25 => "UNKNOWN_MEMBER_ID",
            26 => "INVALID_SESSION_TIMEOUT",
            27 => "REBALANCE_IN_PROGRESS",
            29 => "TOPIC_AUTHORIZATION_FAILED",
            30 => "GROUP_AUTHORIZATION_FAILED",
            31 => "CLUSTER_AUTHORIZATION_FAILED",
            35 => "UNSUPPORTED_VERSION",
            36 => "TOPIC_ALREADY_EXISTS",
            37 => "INVALID_PARTITIONS",
            38 => "INVALID_REPLICATION_FACTOR",
            41 => "NOT_CONTROLLER",
            42 => "INVALID_REQUEST",
            45 => "OUT_OF_ORDER_SEQUENCE_NUMBER",
            46 => "DUPLICATE_SEQUENCE_NUMBER",
            47 => "INVALID_PRODUCER_EPOCH",
            58 => "SASL_AUTHENTICATION_FAILED",
            59 => "UNKNOWN_PRODUCER_ID",
            69 => "GROUP_ID_NOT_FOUND",
            72 => "LISTENER_NOT_FOUND",
            74 => "FENCED_LEADER_EPOCH",
            75 => "UNKNOWN_LEADER_EPOCH",
            78 => "OFFSET_NOT_AVAILABLE",
            79 => "MEMBER_ID_REQUIRED",
            82 => "FENCED_INSTANCE_ID",
            87 => "INVALID_RECORD",
            100 => "UNKNOWN_TOPIC_ID",
            _ => "(unlisted)",
        }
    }

    /// Retrying (after a metadata / coordinator refresh) can succeed.
    pub fn retriable(self) -> bool {
        matches!(self.0, 3 | 5 | 6 | 7 | 13 | 14 | 15 | 16 | 19 | 20 | 74 | 75 | 78 | 100)
    }

    /// As an error for `what`.
    pub fn err(self, what: &str) -> MsgError {
        let m = format!("{what}: Kafka error {} {}", self.0, self.name());
        match self.0 {
            58 | 29 | 30 | 31 => MsgError::Auth(m),
            _ => MsgError::Broker(m),
        }
    }
}

fn short(what: &str) -> MsgError {
    MsgError::Protocol(format!("Kafka: truncated {what}"))
}

/// Writes Kafka values.
#[derive(Default)]
pub struct W {
    pub b: Vec<u8>,
    /// Flexible encoding (compact strings / arrays, tagged fields).
    pub flexible: bool,
}

impl W {
    pub fn new(flexible: bool) -> Self {
        W { b: Vec::new(), flexible }
    }
    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.b.push(v as u8);
        self
    }
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.b.push(u8::from(v));
        self
    }
    pub fn uvarint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
        self
    }
    /// Zigzag varint (records).
    pub fn varint(&mut self, v: i64) -> &mut Self {
        self.uvarint(((v << 1) ^ (v >> 63)) as u64)
    }
    /// An array / string length: compact (`n + 1`, 0 = null) or classic.
    fn len(&mut self, n: Option<usize>, classic_i16: bool) -> &mut Self {
        if self.flexible {
            self.uvarint(n.map_or(0, |n| n as u64 + 1))
        } else if classic_i16 {
            self.i16(n.map_or(-1, |n| n as i16))
        } else {
            self.i32(n.map_or(-1, |n| n as i32))
        }
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.len(Some(s.len()), true);
        self.b.extend_from_slice(s.as_bytes());
        self
    }
    pub fn nstring(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(s) => self.string(s),
            None => self.len(None, true),
        }
    }
    pub fn bytes(&mut self, s: &[u8]) -> &mut Self {
        self.len(Some(s.len()), false);
        self.b.extend_from_slice(s);
        self
    }
    pub fn nbytes(&mut self, s: Option<&[u8]>) -> &mut Self {
        match s {
            Some(s) => self.bytes(s),
            None => self.len(None, false),
        }
    }
    pub fn array_len(&mut self, n: usize) -> &mut Self {
        self.len(Some(n), false)
    }
    /// The empty tagged-field section (flexible only).
    pub fn tags(&mut self) -> &mut Self {
        if self.flexible {
            self.b.push(0);
        }
        self
    }
    /// A 16-byte UUID (all zero: none).
    pub fn uuid(&mut self, u: &[u8; 16]) -> &mut Self {
        self.b.extend_from_slice(u);
        self
    }
}

/// Reads Kafka values.
pub struct R<'a> {
    pub b: &'a [u8],
    pub pos: usize,
    pub flexible: bool,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8], flexible: bool) -> Self {
        R { b, pos: 0, flexible }
    }
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], MsgError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| short(what))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    pub fn i8(&mut self) -> Result<i8, MsgError> {
        Ok(self.take(1, "int8")?[0] as i8)
    }
    pub fn bool(&mut self) -> Result<bool, MsgError> {
        Ok(self.i8()? != 0)
    }
    pub fn i16(&mut self) -> Result<i16, MsgError> {
        Ok(i16::from_be_bytes(self.take(2, "int16")?.try_into().unwrap_or_default()))
    }
    pub fn i32(&mut self) -> Result<i32, MsgError> {
        Ok(i32::from_be_bytes(self.take(4, "int32")?.try_into().unwrap_or_default()))
    }
    pub fn u32(&mut self) -> Result<u32, MsgError> {
        Ok(u32::from_be_bytes(self.take(4, "uint32")?.try_into().unwrap_or_default()))
    }
    pub fn i64(&mut self) -> Result<i64, MsgError> {
        Ok(i64::from_be_bytes(self.take(8, "int64")?.try_into().unwrap_or_default()))
    }
    pub fn uvarint(&mut self) -> Result<u64, MsgError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1, "varint")?[0];
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(MsgError::Protocol("Kafka: varint too long".into()))
    }
    pub fn varint(&mut self) -> Result<i64, MsgError> {
        let u = self.uvarint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    /// A length: `None` for null.
    fn len(&mut self, classic_i16: bool) -> Result<Option<usize>, MsgError> {
        let n: i64 = if self.flexible {
            self.uvarint()? as i64 - 1
        } else if classic_i16 {
            self.i16()?.into()
        } else {
            self.i32()?.into()
        };
        if n < 0 {
            return Ok(None);
        }
        // Bounded by the caller: strings / bytes by `take`, arrays by `array_len`.
        Ok(Some(n as usize))
    }
    pub fn string(&mut self) -> Result<String, MsgError> {
        Ok(self.nstring()?.unwrap_or_default())
    }
    pub fn nstring(&mut self) -> Result<Option<String>, MsgError> {
        match self.len(true)? {
            None => Ok(None),
            Some(n) => Ok(Some(String::from_utf8_lossy(self.take(n, "string")?).into_owned())),
        }
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, MsgError> {
        Ok(self.nbytes()?.unwrap_or_default())
    }
    pub fn nbytes(&mut self) -> Result<Option<Vec<u8>>, MsgError> {
        match self.len(false)? {
            None => Ok(None),
            Some(n) => Ok(Some(self.take(n, "bytes")?.to_vec())),
        }
    }
    /// An array's element count (null as 0), bounded by the bytes left
    /// (every element takes at least one byte).
    pub fn array_len(&mut self) -> Result<usize, MsgError> {
        let n = self.len(false)?.unwrap_or(0);
        if n > self.remaining() {
            return Err(MsgError::Protocol(format!("Kafka: array of {n} elements in {} bytes", self.remaining())));
        }
        Ok(n)
    }
    /// Skip a tagged-field section (flexible only).
    pub fn tags(&mut self) -> Result<(), MsgError> {
        if self.flexible {
            let n = self.uvarint()?;
            for _ in 0..n {
                self.uvarint()?; // tag
                let size = self.uvarint()? as usize;
                self.take(size, "tagged field")?;
            }
        }
        Ok(())
    }
    pub fn uuid(&mut self) -> Result<[u8; 16], MsgError> {
        Ok(self.take(16, "uuid")?.try_into().unwrap_or_default())
    }
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], MsgError> {
        self.take(n, "bytes")
    }
}

/// A request frame: size, header (v2 for flexible requests, v1 otherwise),
/// then the body.
pub fn request(api: Api, correlation: i32, client_id: &str, body: &[u8]) -> Vec<u8> {
    let (key, version, flexible) = api.spec();
    let mut h = W::new(false);
    h.i16(key).i16(version).i32(correlation).nstring(Some(client_id));
    if flexible {
        h.b.push(0); // header tagged fields
    }
    let mut out = Vec::with_capacity(4 + h.b.len() + body.len());
    out.extend_from_slice(&((h.b.len() + body.len()) as i32).to_be_bytes());
    out.extend_from_slice(&h.b);
    out.extend_from_slice(body);
    out
}

/// Split a response: correlation id, then the body after the header (v1
/// with tagged fields for flexible responses — but ApiVersions answers
/// with header v0 always).
pub fn response_body(api: Api, frame: &[u8]) -> Result<(i32, &[u8]), MsgError> {
    let (_, _, flexible) = api.spec();
    let mut r = R::new(frame, false);
    let corr = r.i32()?;
    if flexible && api != Api::ApiVersions {
        r.flexible = true;
        r.tags()?;
    }
    Ok((corr, &frame[r.pos..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_compact_values_round_trip() {
        for v in [0i64, 1, -1, 63, -64, 64, 300, -300, i32::MAX as i64, i64::MIN, i64::MAX] {
            let mut w = W::new(false);
            w.varint(v);
            assert_eq!(R::new(&w.b, false).varint().unwrap(), v, "{v}");
        }
        // Zigzag: -1 is 1, 1 is 2 (one byte each).
        let mut w = W::new(false);
        w.varint(-1).varint(1);
        assert_eq!(w.b, [1, 2]);
        for flexible in [false, true] {
            let mut w = W::new(flexible);
            w.string("Việt").nstring(None).bytes(&[1, 2]).nbytes(None).array_len(3).tags();
            let mut r = R::new(&w.b, flexible);
            assert_eq!(r.string().unwrap(), "Việt");
            assert_eq!(r.nstring().unwrap(), None);
            assert_eq!(r.bytes().unwrap(), [1, 2]);
            assert_eq!(r.nbytes().unwrap(), None);
            // Three elements claimed, but only a tag byte left: refused.
            assert!(r.array_len().is_err() || flexible);
        }
        // Compact string: length + 1.
        let mut w = W::new(true);
        w.string("ab");
        assert_eq!(w.b, [3, b'a', b'b']);
    }

    #[test]
    fn hostile_input_is_an_error() {
        assert!(R::new(&[0xff; 11], false).uvarint().is_err(), "a varint longer than 10 bytes");
        assert!(R::new(&[0, 5, b'a'], false).string().is_err(), "string longer than the frame");
        assert!(R::new(&[0x7f, 0xff, 0xff, 0xff], false).array_len().is_err(), "2^31 elements in 0 bytes");
        assert!(R::new(&[1, 9, 200], true).tags().is_err(), "tagged field longer than the frame");
    }

    #[test]
    fn request_and_response_headers() {
        let r = request(Api::Metadata, 7, "kw", &[9, 9]);
        // size, key 3, version 12, correlation 7, client id "kw" (classic), tags, body.
        assert_eq!(r, [0, 0, 0, 15, 0, 3, 0, 12, 0, 0, 0, 7, 0, 2, b'k', b'w', 0, 9, 9]);
        let (corr, body) = response_body(Api::Metadata, &[0, 0, 0, 7, 0, 42]).unwrap();
        assert_eq!((corr, body), (7, &[42u8][..]));
        let (_, body) = response_body(Api::ApiVersions, &[0, 0, 0, 7, 0, 42]).unwrap();
        assert_eq!(body, [0, 42], "ApiVersions responses have no header tags");
    }
}
