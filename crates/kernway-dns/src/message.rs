//! DNS wire format (RFC 1035) — encode a query, parse a response.
//!
//! Pure functions, no I/O. The parser treats its input as **hostile**: every
//! field is bounds-checked, and name decompression is guarded so a crafted
//! packet can neither loop forever nor read out of bounds.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::DnsError;

/// Query type: an IPv4 address record.
pub const TYPE_A: u16 = 1;
/// Query type: an IPv6 address record.
pub const TYPE_AAAA: u16 = 28;
/// Query type: a canonical-name (alias) record.
pub const TYPE_CNAME: u16 = 5;
/// Pseudo-record type for EDNS0 (RFC 6891) — carried in the additional section.
const TYPE_OPT: u16 = 41;
/// Internet class.
const CLASS_IN: u16 = 1;

/// Requestor's UDP payload size to advertise via EDNS0. 1232 is the widely
/// recommended safe value (fits the smallest common path MTU without
/// fragmentation), cutting how often answers truncate to TCP.
pub const EDNS_UDP_SIZE: u16 = 1232;

/// A label may be at most 63 bytes; a name at most 255.
const MAX_LABEL: usize = 63;
const MAX_NAME: usize = 255;
/// Cap on compression-pointer jumps while reading one name. Each jump must go
/// strictly backward (enforced below), so this is a belt-and-braces bound.
const MAX_JUMPS: usize = 32;

/// One resolved address plus its TTL (seconds) — TTL feeds the cache slice later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAddr {
    /// The A/AAAA address.
    pub ip: IpAddr,
    /// Time-to-live in seconds, as the server reported it.
    pub ttl: u32,
}

/// The useful contents of a parsed DNS response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Transaction id echoed by the server.
    pub id: u16,
    /// The `TC` bit — the answer was truncated and should be retried over TCP.
    pub truncated: bool,
    /// Response code (0 = NoError, 3 = NXDOMAIN, …).
    pub rcode: u8,
    /// A/AAAA records from the answer section, in received order.
    pub addresses: Vec<ResolvedAddr>,
}

/// Encode a standard recursive query for `name` of type `qtype` (`TYPE_A` /
/// `TYPE_AAAA`) with transaction id `id`.
///
/// Sets `RD` (recursion desired). Returns [`DnsError::NameTooLong`] if a label
/// exceeds 63 bytes or the name exceeds 255.
pub fn encode_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>, DnsError> {
    encode_query_inner(id, name, qtype, None)
}

/// Like [`encode_query`] but advertises EDNS0 with `udp_payload` as the accepted
/// UDP response size — appends an OPT record to the additional section so the
/// server may return larger answers over UDP before resorting to truncation.
pub fn encode_query_edns(
    id: u16,
    name: &str,
    qtype: u16,
    udp_payload: u16,
) -> Result<Vec<u8>, DnsError> {
    encode_query_inner(id, name, qtype, Some(udp_payload))
}

fn encode_query_inner(
    id: u16,
    name: &str,
    qtype: u16,
    edns: Option<u16>,
) -> Result<Vec<u8>, DnsError> {
    let mut buf = Vec::with_capacity(32 + name.len());
    // Header: id, flags (RD=1), QDCOUNT=1, ANCOUNT=0, NSCOUNT=0, ARCOUNT=edns?1:0.
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    buf.extend_from_slice(&(edns.is_some() as u16).to_be_bytes()); // ARCOUNT
    encode_name(&mut buf, name)?;
    buf.extend_from_slice(&qtype.to_be_bytes());
    buf.extend_from_slice(&CLASS_IN.to_be_bytes());
    if let Some(udp) = edns {
        // OPT pseudo-record (RFC 6891): root name, TYPE=OPT, CLASS=UDP size,
        // TTL = ext-rcode|version|flags (all zero, DO bit off), empty RDATA.
        buf.push(0); // root name
        buf.extend_from_slice(&TYPE_OPT.to_be_bytes());
        buf.extend_from_slice(&udp.to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend_from_slice(&0u16.to_be_bytes()); // RDLENGTH = 0
    }
    Ok(buf)
}

/// Append `name` as a sequence of length-prefixed labels terminated by a zero.
/// A trailing dot (the root) and an empty name both encode to a single `0`.
fn encode_name(buf: &mut Vec<u8>, name: &str) -> Result<(), DnsError> {
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    let mut total = 1; // the terminating zero
    if !trimmed.is_empty() {
        for label in trimmed.split('.') {
            let bytes = label.as_bytes();
            if bytes.is_empty() || bytes.len() > MAX_LABEL {
                return Err(DnsError::NameTooLong);
            }
            total += 1 + bytes.len();
            if total > MAX_NAME {
                return Err(DnsError::NameTooLong);
            }
            buf.push(bytes.len() as u8);
            buf.extend_from_slice(bytes);
        }
    }
    buf.push(0);
    Ok(())
}

/// A bounds-checked forward cursor over a byte slice.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, DnsError> {
        let b = *self.buf.get(self.pos).ok_or(DnsError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, DnsError> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Ok((hi << 8) | lo)
    }

    fn u32(&mut self) -> Result<u32, DnsError> {
        let hi = self.u16()? as u32;
        let lo = self.u16()? as u32;
        Ok((hi << 16) | lo)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DnsError> {
        let end = self.pos.checked_add(n).ok_or(DnsError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(DnsError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Advance past a name, following compression pointers only to skip it.
    ///
    /// Returns the decoded name. The cursor is left just after the name **at the
    /// original position** — i.e. after the first pointer, not at the pointer's
    /// target. Compression is validated: a pointer must jump strictly backward,
    /// which makes a loop impossible; label and name lengths are capped.
    fn name(&mut self) -> Result<String, DnsError> {
        let mut labels: Vec<u8> = Vec::new();
        let mut name_len = 0usize;
        let mut pos = self.pos;
        let mut jumped = false;
        let mut jumps = 0usize;

        loop {
            let len = *self.buf.get(pos).ok_or(DnsError::Truncated)?;
            match len & 0xC0 {
                0x00 => {
                    if len == 0 {
                        pos += 1;
                        if !jumped {
                            self.pos = pos;
                        }
                        break;
                    }
                    let len = len as usize;
                    let start = pos + 1;
                    let end = start.checked_add(len).ok_or(DnsError::Truncated)?;
                    let label = self.buf.get(start..end).ok_or(DnsError::Truncated)?;
                    name_len += len + 1;
                    if name_len > MAX_NAME {
                        return Err(DnsError::NameTooLong);
                    }
                    if !labels.is_empty() {
                        labels.push(b'.');
                    }
                    labels.extend_from_slice(label);
                    pos = end;
                }
                0xC0 => {
                    // A 14-bit pointer to an earlier offset.
                    let b2 = *self.buf.get(pos + 1).ok_or(DnsError::Truncated)?;
                    let target = (((len & 0x3F) as usize) << 8) | b2 as usize;
                    if !jumped {
                        // Consume the 2 pointer bytes at the original position.
                        self.pos = pos + 2;
                        jumped = true;
                    }
                    jumps += 1;
                    if jumps > MAX_JUMPS || target >= pos {
                        // `target >= pos` forbids forward/self references, so the
                        // offset strictly decreases each jump → no infinite loop.
                        return Err(DnsError::MalformedName);
                    }
                    pos = target;
                }
                // 0x40 and 0x80 length prefixes are reserved.
                _ => return Err(DnsError::MalformedName),
            }
        }
        Ok(String::from_utf8_lossy(&labels).into_owned())
    }

    /// Skip a name without decoding it (for question/RR names we don't need).
    fn skip_name(&mut self) -> Result<(), DnsError> {
        self.name().map(|_| ())
    }
}

/// Parse a DNS response packet into its useful parts.
///
/// This is I/O-free and total: any malformed input yields an `Err`, never a
/// panic or a hang.
pub fn parse_response(buf: &[u8]) -> Result<Response, DnsError> {
    let mut r = Reader::new(buf);
    let id = r.u16()?;
    let flags = r.u16()?;
    let qdcount = r.u16()?;
    let ancount = r.u16()?;
    let _nscount = r.u16()?;
    let _arcount = r.u16()?;

    let truncated = (flags >> 9) & 1 == 1;
    let rcode = (flags & 0x000F) as u8;

    // Skip the question section.
    for _ in 0..qdcount {
        r.skip_name()?;
        let _qtype = r.u16()?;
        let _qclass = r.u16()?;
    }

    // Walk the answer section, collecting A/AAAA records. CNAMEs are skipped —
    // a recursive server returns the final addresses alongside the CNAME chain.
    let mut addresses = Vec::new();
    for _ in 0..ancount {
        r.skip_name()?;
        let rtype = r.u16()?;
        let _class = r.u16()?;
        let ttl = r.u32()?;
        let rdlen = r.u16()? as usize;
        let rdata = r.take(rdlen)?;
        match rtype {
            TYPE_A if rdlen == 4 => {
                let ip = Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]);
                addresses.push(ResolvedAddr { ip: IpAddr::V4(ip), ttl });
            }
            TYPE_AAAA if rdlen == 16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(rdata);
                addresses.push(ResolvedAddr { ip: IpAddr::V6(Ipv6Addr::from(octets)), ttl });
            }
            // CNAME (TYPE_CNAME) and everything else: already consumed via
            // `take`, so just skip — the final addresses come as A/AAAA records.
            _ => {}
        }
    }

    Ok(Response { id, truncated, rcode, addresses })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode ────────────────────────────────────────────────────────────────

    #[test]
    fn encode_query_has_a_12_byte_header_with_rd_set() {
        let q = encode_query(0xABCD, "example.com", TYPE_A).unwrap();
        assert_eq!(&q[0..2], &[0xAB, 0xCD]); // id
        assert_eq!(&q[2..4], &[0x01, 0x00]); // flags: RD
        assert_eq!(&q[4..6], &[0x00, 0x01]); // QDCOUNT = 1
        assert_eq!(&q[6..12], &[0, 0, 0, 0, 0, 0]); // AN/NS/AR = 0
    }

    #[test]
    fn encode_query_encodes_labels_then_type_and_class() {
        let q = encode_query(1, "a.bc", TYPE_AAAA).unwrap();
        // after the 12-byte header: 1 'a' 2 'b' 'c' 0  then QTYPE, QCLASS
        assert_eq!(&q[12..18], &[1, b'a', 2, b'b', b'c', 0]);
        assert_eq!(&q[18..20], &28u16.to_be_bytes()); // AAAA
        assert_eq!(&q[20..22], &1u16.to_be_bytes()); // IN
    }

    #[test]
    fn encode_query_trailing_dot_is_the_same_as_without() {
        assert_eq!(
            encode_query(1, "example.com.", TYPE_A).unwrap(),
            encode_query(1, "example.com", TYPE_A).unwrap()
        );
    }

    #[test]
    fn encode_query_rejects_an_over_long_label() {
        let long = "a".repeat(64);
        assert_eq!(encode_query(1, &long, TYPE_A), Err(DnsError::NameTooLong));
    }

    #[test]
    fn edns_query_sets_arcount_and_appends_an_opt_record() {
        let plain = encode_query(1, "a.bc", TYPE_A).unwrap();
        let edns = encode_query_edns(1, "a.bc", TYPE_A, EDNS_UDP_SIZE).unwrap();
        // ARCOUNT (header bytes 10..12) goes 0 → 1.
        assert_eq!(&plain[10..12], &0u16.to_be_bytes());
        assert_eq!(&edns[10..12], &1u16.to_be_bytes());
        // The extra 11 bytes are exactly the OPT record: root, TYPE=41, CLASS=size,
        // TTL=0, RDLEN=0.
        let opt = &edns[plain.len()..];
        assert_eq!(opt.len(), 11);
        assert_eq!(opt[0], 0); // root name
        assert_eq!(&opt[1..3], &41u16.to_be_bytes()); // TYPE_OPT
        assert_eq!(&opt[3..5], &EDNS_UDP_SIZE.to_be_bytes()); // UDP payload size
        assert_eq!(&opt[5..9], &[0, 0, 0, 0]); // TTL
        assert_eq!(&opt[9..11], &0u16.to_be_bytes()); // RDLENGTH
    }

    #[test]
    fn a_response_with_an_opt_in_additional_still_parses() {
        // Our parser reads only QD + AN; an OPT in the additional section (ARCOUNT>0)
        // must simply be ignored, never break parsing.
        let mut pkt = build_response(1, "example.com", &[("example.com", TYPE_A, &[1, 2, 3, 4])]);
        pkt[11] = 1; // ARCOUNT = 1 (we don't append a real OPT; parser must not read it)
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.addresses.len(), 1);
    }

    // ── round-trip ────────────────────────────────────────────────────────────

    /// Build a minimal response: echo one question, then the given answers.
    fn build_response(id: u16, qname: &str, answers: &[(&str, u16, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&0x8180u16.to_be_bytes()); // QR + RD + RA, RCODE 0
        buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        buf.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        encode_name(&mut buf, qname).unwrap();
        buf.extend_from_slice(&TYPE_A.to_be_bytes());
        buf.extend_from_slice(&CLASS_IN.to_be_bytes());
        for (name, rtype, rdata) in answers {
            encode_name(&mut buf, name).unwrap();
            buf.extend_from_slice(&rtype.to_be_bytes());
            buf.extend_from_slice(&CLASS_IN.to_be_bytes());
            buf.extend_from_slice(&300u32.to_be_bytes()); // TTL
            buf.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            buf.extend_from_slice(rdata);
        }
        buf
    }

    #[test]
    fn parse_extracts_an_a_record() {
        let pkt = build_response(0x1234, "example.com", &[("example.com", TYPE_A, &[93, 184, 216, 34])]);
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.id, 0x1234);
        assert!(!resp.truncated);
        assert_eq!(resp.rcode, 0);
        assert_eq!(resp.addresses.len(), 1);
        assert_eq!(resp.addresses[0].ip, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
        assert_eq!(resp.addresses[0].ttl, 300);
    }

    #[test]
    fn parse_extracts_an_aaaa_record() {
        let v6 = [0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let pkt = build_response(1, "example.com", &[("example.com", TYPE_AAAA, &v6)]);
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.addresses.len(), 1);
        assert_eq!(resp.addresses[0].ip, IpAddr::V6(Ipv6Addr::from(v6)));
    }

    #[test]
    fn parse_skips_a_cname_and_keeps_the_a() {
        let pkt = build_response(
            1,
            "www.example.com",
            &[
                ("www.example.com", TYPE_CNAME, &[3, b'c', b'd', b'n', 0]),
                ("cdn", TYPE_A, &[1, 2, 3, 4]),
            ],
        );
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.addresses.len(), 1);
        assert_eq!(resp.addresses[0].ip, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn parse_reads_multiple_a_records() {
        let pkt = build_response(
            1,
            "example.com",
            &[
                ("example.com", TYPE_A, &[10, 0, 0, 1]),
                ("example.com", TYPE_A, &[10, 0, 0, 2]),
            ],
        );
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.addresses.len(), 2);
    }

    // ── name compression ──────────────────────────────────────────────────────

    #[test]
    fn parse_follows_a_backward_compression_pointer() {
        // Question "example.com" at offset 12; the answer's name is a pointer to it.
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0x8180u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes()); // QD
        pkt.extend_from_slice(&1u16.to_be_bytes()); // AN
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        let qname_off = pkt.len(); // 12
        encode_name(&mut pkt, "example.com").unwrap();
        pkt.extend_from_slice(&TYPE_A.to_be_bytes());
        pkt.extend_from_slice(&CLASS_IN.to_be_bytes());
        // Answer: name = pointer to qname_off
        pkt.push(0xC0 | ((qname_off >> 8) as u8));
        pkt.push((qname_off & 0xFF) as u8);
        pkt.extend_from_slice(&TYPE_A.to_be_bytes());
        pkt.extend_from_slice(&CLASS_IN.to_be_bytes());
        pkt.extend_from_slice(&60u32.to_be_bytes());
        pkt.extend_from_slice(&4u16.to_be_bytes());
        pkt.extend_from_slice(&[8, 8, 8, 8]);

        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.addresses.len(), 1);
        assert_eq!(resp.addresses[0].ip, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn a_self_referential_pointer_is_rejected_not_looped() {
        // Header says 1 question; the question name is a pointer to itself (offset 12).
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0x8180u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        // offset 12: pointer to 12 (itself) → forward/self, must error.
        pkt.push(0xC0);
        pkt.push(12);
        assert_eq!(parse_response(&pkt), Err(DnsError::MalformedName));
    }

    #[test]
    fn a_forward_pointer_is_rejected() {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0x8180u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        // offset 12: pointer to 14 (forward) → must error, never dereferenced.
        pkt.push(0xC0);
        pkt.push(14);
        pkt.push(0);
        pkt.push(0);
        assert_eq!(parse_response(&pkt), Err(DnsError::MalformedName));
    }

    // ── truncation / malformed ────────────────────────────────────────────────

    #[test]
    fn a_short_packet_is_truncated_not_a_panic() {
        assert_eq!(parse_response(&[0, 1, 2]), Err(DnsError::Truncated));
    }

    #[test]
    fn an_rdata_length_past_the_end_is_truncated() {
        let mut pkt = build_response(1, "e.com", &[("e.com", TYPE_A, &[1, 2, 3, 4])]);
        // Corrupt the last RDLENGTH to claim more bytes than remain.
        let len = pkt.len();
        pkt[len - 6] = 0xFF; // RDLENGTH high byte (before the 4 rdata bytes)
        assert_eq!(parse_response(&pkt), Err(DnsError::Truncated));
    }

    #[test]
    fn the_tc_bit_is_reported() {
        let mut pkt = build_response(1, "e.com", &[]);
        // Set TC (bit 9 of flags at offset 2..4): flags currently 0x8180.
        pkt[2] = 0x83; // 0x8380 has TC set
        let resp = parse_response(&pkt).unwrap();
        assert!(resp.truncated);
    }

    #[test]
    fn nxdomain_rcode_is_reported() {
        let mut pkt = build_response(1, "e.com", &[]);
        pkt[3] = 0x83; // low nibble RCODE = 3 (NXDOMAIN)
        let resp = parse_response(&pkt).unwrap();
        assert_eq!(resp.rcode, 3);
        assert!(resp.addresses.is_empty());
    }
}
