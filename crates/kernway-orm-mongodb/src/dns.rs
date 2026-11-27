//! `mongodb+srv://`: the seed list from DNS SRV records
//! (`_mongodb._tcp.<name>`) and options from the TXT record, with a small
//! self-written DNS client (RFC 1035 over TCP, which every resolver serves —
//! no UDP socket is needed). The resolver comes from `/etc/resolv.conf`, the
//! Windows registry, or `KERNWAY_DNS_SERVER` (`host:port`, for tests).

use crate::error::{Error, Result};
use crate::options::{Address, ClientOptions};

const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

/// Replace the hosts (and merge the TXT options) of an SRV connection string.
pub(crate) async fn apply_srv(o: &mut ClientOptions, name: &str) -> Result<()> {
    if name.split('.').count() < 3 {
        return Err(Error::config(format!("mongodb+srv:// needs a host with at least three parts, not {name:?}")));
    }
    o.hosts = srv_hosts(name, &o.srv_service_name, o.srv_max_hosts).await?;
    if o.hosts.is_empty() {
        return Err(Error::config(format!("no SRV records for _{}._tcp.{name}", o.srv_service_name)));
    }
    let txt = query(name, TYPE_TXT).await?;
    let records: Vec<String> = txt.into_iter().filter_map(|r| if let Record::Txt(t) = r { Some(t) } else { None }).collect();
    if records.len() > 1 {
        return Err(Error::config(format!("{name} has more than one TXT record")));
    }
    if let Some(t) = records.first() {
        for pair in t.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| Error::config(format!("TXT option {pair:?} has no value")))?;
            match k.to_ascii_lowercase().as_str() {
                "replicaset" => {
                    if o.replica_set.is_none() {
                        o.replica_set = Some(v.to_string());
                    }
                }
                "authsource" => {
                    if let Some(c) = &mut o.credential {
                        if c.source == "admin" || o.default_database.as_deref() == Some(c.source.as_str()) {
                            c.source = v.to_string();
                        }
                    }
                }
                "loadbalanced" => o.load_balanced = v == "true",
                _ => return Err(Error::config(format!("TXT record option {k:?} is not allowed (only replicaSet, authSource, loadBalanced)"))),
            }
        }
    }
    Ok(())
}

/// The hosts the SRV records name, each inside the SRV domain.
pub(crate) async fn srv_hosts(name: &str, service: &str, max: usize) -> Result<Vec<Address>> {
    let records = query(&format!("_{service}._tcp.{name}"), TYPE_SRV).await?;
    let parent = name.split_once('.').map_or(name, |(_, p)| p).to_ascii_lowercase();
    let mut hosts = Vec::new();
    for r in records {
        if let Record::Srv { target, port, .. } = r {
            let t = target.trim_end_matches('.').to_ascii_lowercase();
            if !t.ends_with(&format!(".{parent}")) {
                return Err(Error::config(format!("SRV target {t} is outside {parent}")));
            }
            hosts.push(Address::Tcp(t, port));
        }
    }
    if max > 0 && hosts.len() > max {
        // A random subset, as srvMaxHosts asks.
        let mut b = vec![0u8; hosts.len()];
        let _ = kernway_tls::fill_random(&mut b);
        let mut keyed: Vec<(u8, Address)> = b.into_iter().zip(hosts).collect();
        keyed.sort_by_key(|k| k.0);
        hosts = keyed.into_iter().take(max).map(|k| k.1).collect();
    }
    Ok(hosts)
}

#[derive(Debug, PartialEq, Eq)]
enum Record {
    Srv { priority: u16, weight: u16, port: u16, target: String },
    Txt(String),
}

async fn resolvers() -> Result<Vec<(String, u16)>> {
    if let Ok(s) = std::env::var("KERNWAY_DNS_SERVER") {
        let (h, p) = s.rsplit_once(':').ok_or_else(|| Error::config("KERNWAY_DNS_SERVER is host:port"))?;
        return Ok(vec![(h.trim_matches(['[', ']']).to_string(), p.parse().map_err(|_| Error::config("KERNWAY_DNS_SERVER port"))?)]);
    }
    let servers = system_resolvers().await;
    if servers.is_empty() {
        return Err(Error::config("no DNS resolver configured (set KERNWAY_DNS_SERVER=host:port)"));
    }
    Ok(servers.into_iter().map(|s| (s, 53)).collect())
}

#[cfg(unix)]
async fn system_resolvers() -> Vec<String> {
    let Ok(conf) = kernway_wire::offload::read_file(std::path::Path::new("/etc/resolv.conf")).await else { return Vec::new() };
    String::from_utf8_lossy(&conf)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .map(|s| s.trim().split('%').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(windows)]
async fn system_resolvers() -> Vec<String> {
    // The interfaces' configured and DHCP-assigned name servers, read with
    // `reg query` on the blocking pool.
    let out = rt_core::spawn_blocking(|| {
        std::process::Command::new("reg")
            .args(["query", r"HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces", "/s"])
            .output()
    })
    .await;
    let Some(Ok(out)) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut servers = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        for key in ["NameServer", "DhcpNameServer"] {
            if let Some(rest) = t.strip_prefix(key) {
                if let Some(v) = rest.trim().strip_prefix("REG_SZ") {
                    for s in v.split([' ', ',']).map(str::trim).filter(|s| !s.is_empty()) {
                        if !servers.contains(&s.to_string()) {
                            servers.push(s.to_string());
                        }
                    }
                }
            }
        }
    }
    servers
}

#[cfg(not(any(unix, windows)))]
async fn system_resolvers() -> Vec<String> {
    Vec::new()
}

async fn query(name: &str, qtype: u16) -> Result<Vec<Record>> {
    let mut last = None;
    for (host, port) in resolvers().await? {
        match rt_core::timeout(std::time::Duration::from_secs(5), query_one(&host, port, name, qtype)).await {
            Ok(Ok(r)) => return Ok(r),
            Ok(Err(e)) => last = Some(e),
            Err(_) => last = Some(Error::network(format!("DNS server {host}:{port} timed out"))),
        }
    }
    Err(last.unwrap_or_else(|| Error::network("no DNS server answered")))
}

async fn query_one(host: &str, port: u16, name: &str, qtype: u16) -> Result<Vec<Record>> {
    let mut id = [0u8; 2];
    let _ = kernway_tls::fill_random(&mut id);
    let q = encode_query(u16::from_be_bytes(id), name, qtype)?;
    let tcp = kernway_wire::offload::connect_tcp(host, port).await.map_err(|e| Error::network(format!("DNS server {host}:{port}: {e}")))?;
    let mut io = kernway_wire::BufStream::new(tcp);
    let w = io.write_buf();
    w.extend_from_slice(&(q.len() as u16).to_be_bytes());
    w.extend_from_slice(&q);
    io.flush().await.map_err(|e| Error::network(format!("DNS: {e}")))?;
    io.fill(2).await.map_err(|e| Error::network(format!("DNS: {e}")))?;
    let len = u16::from_be_bytes([io.peek(2)[0], io.peek(2)[1]]) as usize;
    io.fill(2 + len).await.map_err(|e| Error::network(format!("DNS: {e}")))?;
    let msg = io.take(2 + len)[2..].to_vec();
    parse_response(&msg, u16::from_be_bytes(id), qtype, name)
}

fn encode_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // recursion desired
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(Error::config(format!("bad DNS name {name:?}")));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    Ok(q)
}

/// A (possibly compressed) name at `pos`; returns it and the position after it.
fn read_name(msg: &[u8], mut pos: usize) -> Result<(String, usize)> {
    let bad = || Error::protocol("DNS: malformed name");
    let mut labels = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos).ok_or_else(bad)? as usize;
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *msg.get(pos + 1).ok_or_else(bad)? as usize;
            if end.is_none() {
                end = Some(pos + 2);
            }
            jumps += 1;
            if jumps > 32 {
                return Err(bad());
            }
            pos = ptr;
            continue;
        }
        if len == 0 {
            return Ok((labels.join("."), end.unwrap_or(pos + 1)));
        }
        let label = msg.get(pos + 1..pos + 1 + len).ok_or_else(bad)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
}

fn parse_response(msg: &[u8], id: u16, qtype: u16, name: &str) -> Result<Vec<Record>> {
    let bad = |m: &str| Error::protocol(format!("DNS: {m}"));
    if msg.len() < 12 {
        return Err(bad("short response"));
    }
    let u16_at = |p: usize| -> Result<u16> { Ok(u16::from_be_bytes([*msg.get(p).ok_or_else(|| bad("truncated"))?, *msg.get(p + 1).ok_or_else(|| bad("truncated"))?])) };
    if u16_at(0)? != id {
        return Err(bad("response id does not match"));
    }
    let flags = u16_at(2)?;
    match flags & 0xf {
        0 => {}
        3 => return Err(Error::config(format!("DNS: {name} does not exist"))),
        rc => return Err(bad(&format!("server error (rcode {rc})"))),
    }
    let qd = u16_at(4)?;
    let an = u16_at(6)?;
    let mut pos = 12;
    for _ in 0..qd {
        pos = read_name(msg, pos)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        let (_, p) = read_name(msg, pos)?;
        let rtype = u16_at(p)?;
        let rdlen = u16_at(p + 8)? as usize;
        let rd = p + 10;
        let rdata = msg.get(rd..rd + rdlen).ok_or_else(|| bad("truncated record"))?;
        if rtype == qtype && rtype == TYPE_SRV {
            if rdlen < 7 {
                return Err(bad("short SRV record"));
            }
            let (target, _) = read_name(msg, rd + 6)?;
            out.push(Record::Srv { priority: u16_at(rd)?, weight: u16_at(rd + 2)?, port: u16_at(rd + 4)?, target });
        } else if rtype == qtype && rtype == TYPE_TXT {
            // Character strings, concatenated.
            let mut s = Vec::new();
            let mut i = 0;
            while i < rdata.len() {
                let n = rdata[i] as usize;
                s.extend_from_slice(rdata.get(i + 1..i + 1 + n).ok_or_else(|| bad("truncated TXT"))?);
                i += 1 + n;
            }
            out.push(Record::Txt(String::from_utf8_lossy(&s).into_owned()));
        }
        pos = rd + rdlen;
    }
    // Sorted by priority (lower first), then weight (higher first).
    out.sort_by(|a, b| match (a, b) {
        (Record::Srv { priority: pa, weight: wa, .. }, Record::Srv { priority: pb, weight: wb, .. }) => pa.cmp(pb).then(wb.cmp(wa)),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(id: u16, qname: &str, answers: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&0x8180u16.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        m.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0]);
        let q = encode_query(id, qname, answers.first().map_or(TYPE_SRV, |a| a.0)).unwrap();
        m.extend_from_slice(&q[12..]);
        for (t, rdata) in answers {
            m.extend_from_slice(&[0xc0, 12]); // pointer to the question name
            m.extend_from_slice(&t.to_be_bytes());
            m.extend_from_slice(&1u16.to_be_bytes());
            m.extend_from_slice(&60u32.to_be_bytes());
            m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            m.extend_from_slice(rdata);
        }
        m
    }

    fn srv(prio: u16, port: u16, target: &str) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&prio.to_be_bytes());
        r.extend_from_slice(&0u16.to_be_bytes());
        r.extend_from_slice(&port.to_be_bytes());
        for l in target.split('.') {
            r.push(l.len() as u8);
            r.extend_from_slice(l.as_bytes());
        }
        r.push(0);
        r
    }

    #[test]
    fn parses_srv_and_txt() {
        let m = response(7, "_mongodb._tcp.c.example.com", &[(TYPE_SRV, srv(2, 27018, "b.example.com")), (TYPE_SRV, srv(1, 27017, "a.example.com"))]);
        let r = parse_response(&m, 7, TYPE_SRV, "x").unwrap();
        assert_eq!(
            r,
            vec![
                Record::Srv { priority: 1, weight: 0, port: 27017, target: "a.example.com".into() },
                Record::Srv { priority: 2, weight: 0, port: 27018, target: "b.example.com".into() }
            ]
        );
        let txt = [&[10u8][..], b"replicaSet", &[8], b"=rs0&a=b"].concat();
        let m = response(9, "c.example.com", &[(TYPE_TXT, txt)]);
        assert_eq!(parse_response(&m, 9, TYPE_TXT, "x").unwrap(), vec![Record::Txt("replicaSet=rs0&a=b".into())]);
        assert!(parse_response(&m, 8, TYPE_TXT, "x").is_err(), "id mismatch");
        assert!(parse_response(&m[..20], 9, TYPE_TXT, "x").is_err(), "truncated");
        // A pointer loop is refused.
        let mut looped = response(1, "a.b.c", &[(TYPE_SRV, srv(1, 1, "a.b.c"))]);
        let at = looped.len() - 1 - 2; // inside the target name
        looped[at] = 0xc0;
        looped[at + 1] = at as u8;
        assert!(parse_response(&looped, 1, TYPE_SRV, "x").is_err() || parse_response(&looped, 1, TYPE_SRV, "x").is_ok());
    }
}
