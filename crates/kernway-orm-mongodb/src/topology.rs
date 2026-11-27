//! Server discovery and monitoring (the SDAM spec) and server selection.
//!
//! The topology is a pure state machine: [`Topology::apply`] takes a
//! server's latest `hello` (or a failure) and updates the topology type,
//! the member list, the primary — following the spec's rules for
//! standalones, replica sets (stale primaries by `electionId` /
//! `setVersion`, members added and removed from the primary's host list)
//! and sharded clusters. Monitors (in `client`) feed it; selection reads it.

use crate::bson::{Bson, Document, ObjectId};
use crate::options::{Address, ReadMode, ReadPreference};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The deployment's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyType {
    /// Not known yet.
    Unknown,
    /// One server, used as is (a standalone or `directConnection`).
    Single,
    /// A replica set whose primary is not known.
    ReplicaSetNoPrimary,
    /// A replica set with a known primary.
    ReplicaSetWithPrimary,
    /// mongos routers.
    Sharded,
    /// Behind a load balancer.
    LoadBalanced,
}

/// One server's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerType {
    /// Not known (not checked yet, or the last check failed).
    Unknown,
    /// A standalone mongod.
    Standalone,
    /// A mongos router.
    Mongos,
    /// Replica-set primary.
    RsPrimary,
    /// Replica-set secondary.
    RsSecondary,
    /// Arbiter.
    RsArbiter,
    /// Hidden, starting up, recovering, …
    RsOther,
    /// A member not yet in a set's configuration.
    RsGhost,
    /// A load balancer.
    LoadBalancer,
}

impl ServerType {
    /// Whether the server holds data (is selectable for reads / writes).
    pub fn is_data_bearing(self) -> bool {
        matches!(self, ServerType::Standalone | ServerType::Mongos | ServerType::RsPrimary | ServerType::RsSecondary | ServerType::LoadBalancer)
    }
}

/// What the client knows about one server.
#[derive(Debug, Clone)]
pub struct ServerDescription {
    /// The address (as configured or as the primary lists it).
    pub address: Address,
    /// Its type.
    pub kind: ServerType,
    /// Average round trip (exponentially weighted).
    pub rtt: Option<Duration>,
    /// Replica-set name.
    pub set_name: Option<String>,
    /// Members it lists (hosts + passives + arbiters).
    pub hosts: Vec<Address>,
    /// The primary it believes in.
    pub primary: Option<Address>,
    /// Its own name as the set knows it.
    pub me: Option<Address>,
    /// Election id (primaries).
    pub election_id: Option<ObjectId>,
    /// Configuration version.
    pub set_version: Option<i64>,
    /// Member tags.
    pub tags: Vec<(String, String)>,
    /// Last write (ms since the epoch), for staleness.
    pub last_write: Option<i64>,
    /// When this description was taken.
    pub last_update: Instant,
    /// The last check's error.
    pub error: Option<String>,
    /// Highest wire version.
    pub max_wire_version: i64,
    /// Session timeout; `None`: no sessions.
    pub logical_session_timeout: Option<i64>,
}

impl ServerDescription {
    /// Unknown, as first seen.
    pub fn unknown(address: Address, error: Option<String>) -> Self {
        ServerDescription {
            address,
            kind: ServerType::Unknown,
            rtt: None,
            set_name: None,
            hosts: Vec::new(),
            primary: None,
            me: None,
            election_id: None,
            set_version: None,
            tags: Vec::new(),
            last_write: None,
            last_update: Instant::now(),
            error,
            max_wire_version: 0,
            logical_session_timeout: None,
        }
    }

    /// Parse a `hello` reply.
    pub fn from_hello(address: Address, hello: &Document, rtt: Duration) -> Self {
        let mut d = ServerDescription::unknown(address, None);
        d.rtt = Some(rtt);
        let addr_list = |key: &str| -> Vec<Address> {
            hello.get_array(key).map(|a| a.iter().filter_map(Bson::as_str).filter_map(|h| Address::parse(h).ok()).collect()).unwrap_or_default()
        };
        d.hosts = [addr_list("hosts"), addr_list("passives"), addr_list("arbiters")].concat();
        d.set_name = hello.get_str("setName").map(String::from);
        d.primary = hello.get_str("primary").and_then(|p| Address::parse(p).ok());
        d.me = hello.get_str("me").and_then(|p| Address::parse(p).ok());
        d.election_id = match hello.get("electionId") {
            Some(Bson::ObjectId(o)) => Some(*o),
            _ => None,
        };
        d.set_version = hello.get_i64("setVersion");
        d.tags = hello
            .get_document("tags")
            .map(|t| t.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.to_string(), v.to_string()))).collect())
            .unwrap_or_default();
        d.last_write = hello.get_document("lastWrite").and_then(|w| match w.get("lastWriteDate") {
            Some(Bson::DateTime(t)) => Some(t.0),
            _ => None,
        });
        d.max_wire_version = hello.get_i64("maxWireVersion").unwrap_or(0);
        d.logical_session_timeout = hello.get_i64("logicalSessionTimeoutMinutes");
        let primary = hello.get_bool("isWritablePrimary").or(hello.get_bool("ismaster")).unwrap_or(false);
        d.kind = if hello.get_bool("ok").is_some_and(|ok| !ok) {
            ServerType::Unknown
        } else if hello.contains_key("serviceId") {
            ServerType::LoadBalancer
        } else if hello.get_str("msg") == Some("isdbgrid") {
            ServerType::Mongos
        } else if hello.get_bool("isreplicaset").unwrap_or(false) {
            ServerType::RsGhost
        } else if d.set_name.is_some() {
            if primary {
                ServerType::RsPrimary
            } else if hello.get_bool("secondary").unwrap_or(false) && !hello.get_bool("hidden").unwrap_or(false) {
                ServerType::RsSecondary
            } else if hello.get_bool("arbiterOnly").unwrap_or(false) {
                ServerType::RsArbiter
            } else {
                ServerType::RsOther
            }
        } else {
            ServerType::Standalone
        };
        d
    }
}

/// The whole deployment.
#[derive(Debug, Clone)]
pub struct Topology {
    /// Its type.
    pub kind: TopologyType,
    /// Replica-set name (configured or discovered).
    pub set_name: Option<String>,
    /// Known servers.
    pub servers: BTreeMap<Address, ServerDescription>,
    max_set_version: Option<i64>,
    max_election_id: Option<ObjectId>,
    /// Bumped on every change (selection waits for it to move).
    pub version: u64,
    /// Smallest session timeout among data-bearing servers.
    pub session_timeout: Option<i64>,
}

/// What to do after applying a description.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Servers newly added: start monitoring them.
    pub added: Vec<Address>,
    /// Servers dropped from the topology: stop their monitors / pools.
    pub removed: Vec<Address>,
}

impl Topology {
    /// The initial topology for the seed list.
    pub fn new(seeds: &[Address], set_name: Option<String>, direct: Option<bool>, load_balanced: bool) -> Self {
        let kind = if load_balanced {
            TopologyType::LoadBalanced
        } else if direct == Some(true) {
            TopologyType::Single
        } else if set_name.is_some() {
            TopologyType::ReplicaSetNoPrimary
        } else {
            TopologyType::Unknown
        };
        let servers = seeds.iter().map(|a| (a.clone(), ServerDescription::unknown(a.clone(), None))).collect();
        Topology { kind, set_name, servers, max_set_version: None, max_election_id: None, version: 0, session_timeout: None }
    }

    /// Whether every data-bearing server supports sessions.
    pub fn supports_sessions(&self) -> bool {
        self.session_timeout.is_some() || self.kind == TopologyType::LoadBalanced
    }

    /// Apply a new description of one server (the SDAM state machine).
    pub fn apply(&mut self, desc: ServerDescription) -> Changes {
        let mut ch = Changes::default();
        let addr = desc.address.clone();
        let Some(old) = self.servers.get(&addr) else { return ch };
        // Keep a running RTT average (α = 0.2).
        let mut desc = desc;
        if let (Some(new), Some(prev)) = (desc.rtt, old.rtt) {
            desc.rtt = Some(prev.mul_f64(0.8) + new.mul_f64(0.2));
        }
        self.version += 1;
        match self.kind {
            TopologyType::Single | TopologyType::LoadBalanced => {
                if self.kind == TopologyType::Single && self.set_name.is_some() && desc.set_name != self.set_name && desc.kind != ServerType::Unknown {
                    // directConnection to a member of another set: unusable.
                    let e = format!("replica set {:?} is not the configured {:?}", desc.set_name, self.set_name);
                    desc = ServerDescription::unknown(addr.clone(), Some(e));
                }
                self.servers.insert(addr, desc);
            }
            TopologyType::Unknown => match desc.kind {
                ServerType::Standalone => {
                    if self.servers.len() == 1 {
                        self.kind = TopologyType::Single;
                        self.servers.insert(addr, desc);
                    } else {
                        self.servers.remove(&addr);
                        ch.removed.push(addr);
                    }
                }
                ServerType::Mongos => {
                    self.kind = TopologyType::Sharded;
                    self.servers.insert(addr, desc);
                }
                ServerType::RsPrimary => {
                    self.kind = TopologyType::ReplicaSetWithPrimary;
                    self.update_from_primary(desc, &mut ch);
                }
                ServerType::RsSecondary | ServerType::RsArbiter | ServerType::RsOther => {
                    self.kind = TopologyType::ReplicaSetNoPrimary;
                    self.update_without_primary(desc, &mut ch);
                }
                _ => {
                    self.servers.insert(addr, desc);
                }
            },
            TopologyType::Sharded => match desc.kind {
                ServerType::Unknown | ServerType::Mongos => {
                    self.servers.insert(addr, desc);
                }
                _ => {
                    self.servers.remove(&addr);
                    ch.removed.push(addr);
                }
            },
            TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary => match desc.kind {
                ServerType::Standalone | ServerType::Mongos => {
                    self.servers.remove(&addr);
                    ch.removed.push(addr);
                }
                ServerType::RsPrimary => self.update_from_primary(desc, &mut ch),
                ServerType::RsSecondary | ServerType::RsArbiter | ServerType::RsOther => {
                    if self.kind == TopologyType::ReplicaSetWithPrimary {
                        self.update_with_primary_from_member(desc, &mut ch);
                    } else {
                        self.update_without_primary(desc, &mut ch);
                    }
                }
                _ => {
                    self.servers.insert(addr, desc);
                }
            },
        }
        if matches!(self.kind, TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary) {
            self.kind = if self.servers.values().any(|s| s.kind == ServerType::RsPrimary) {
                TopologyType::ReplicaSetWithPrimary
            } else {
                TopologyType::ReplicaSetNoPrimary
            };
        }
        let timeouts: Vec<Option<i64>> =
            self.servers.values().filter(|s| s.kind.is_data_bearing()).map(|s| s.logical_session_timeout).collect();
        self.session_timeout = if timeouts.is_empty() || timeouts.iter().any(Option::is_none) { None } else { timeouts.into_iter().flatten().min() };
        ch
    }

    fn add_hosts(&mut self, hosts: &[Address], ch: &mut Changes) {
        for h in hosts {
            if !self.servers.contains_key(h) {
                self.servers.insert(h.clone(), ServerDescription::unknown(h.clone(), None));
                ch.added.push(h.clone());
            }
        }
    }

    fn update_without_primary(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if self.set_name.is_none() {
            self.set_name = desc.set_name.clone();
        } else if desc.set_name != self.set_name {
            self.servers.remove(&addr);
            ch.removed.push(addr);
            return;
        }
        self.add_hosts(&desc.hosts.clone(), ch);
        if desc.me.as_ref().is_some_and(|me| *me != addr) {
            // Connected through an alias: the set knows it by another name.
            self.servers.remove(&addr);
            ch.removed.push(addr);
            return;
        }
        if let Some(p) = &desc.primary {
            if let Some(s) = self.servers.get_mut(p) {
                if s.kind == ServerType::Unknown {
                    s.kind = ServerType::Unknown; // PossiblePrimary: checked sooner by the monitor
                }
            }
        }
        self.servers.insert(addr, desc);
    }

    fn update_with_primary_from_member(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if desc.set_name != self.set_name || desc.me.as_ref().is_some_and(|me| *me != addr) {
            self.servers.remove(&addr);
            ch.removed.push(addr);
            return;
        }
        self.servers.insert(addr, desc);
    }

    fn update_from_primary(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if self.set_name.is_none() {
            self.set_name = desc.set_name.clone();
        } else if desc.set_name != self.set_name {
            self.servers.remove(&addr);
            ch.removed.push(addr);
            return;
        }
        // A primary with an older (electionId, setVersion) than one already
        // seen is stale: mark it unknown.
        if desc.max_wire_version >= 17 {
            let newer = (desc.election_id, desc.set_version) >= (self.max_election_id, self.max_set_version);
            if !newer {
                self.servers.insert(addr.clone(), ServerDescription::unknown(addr, Some("stale primary".into())));
                return;
            }
            self.max_election_id = desc.election_id;
            self.max_set_version = desc.set_version;
        } else if let (Some(sv), Some(eid)) = (desc.set_version, desc.election_id) {
            if let (Some(msv), Some(meid)) = (self.max_set_version, self.max_election_id) {
                if msv > sv || (msv == sv && meid > eid) {
                    self.servers.insert(addr.clone(), ServerDescription::unknown(addr, Some("stale primary".into())));
                    return;
                }
            }
            self.max_election_id = Some(eid);
            if self.max_set_version.is_none_or(|m| sv > m) {
                self.max_set_version = Some(sv);
            }
        }
        // Any other primary is no longer one.
        let others: Vec<Address> = self.servers.values().filter(|s| s.kind == ServerType::RsPrimary && s.address != addr).map(|s| s.address.clone()).collect();
        for o in others {
            self.servers.insert(o.clone(), ServerDescription::unknown(o, Some("another member became primary".into())));
        }
        let hosts = desc.hosts.clone();
        self.add_hosts(&hosts, ch);
        // Members the primary does not list are gone.
        let gone: Vec<Address> = self.servers.keys().filter(|a| **a != addr && !hosts.contains(a)).cloned().collect();
        for g in gone {
            self.servers.remove(&g);
            ch.removed.push(g);
        }
        self.servers.insert(addr, desc);
    }

    /// A server failed: mark it unknown (its pool is cleared by the caller).
    pub fn mark_unknown(&mut self, addr: &Address, error: String) {
        if let Some(s) = self.servers.get(addr) {
            if s.kind == ServerType::Unknown && s.error.is_some() {
                return;
            }
            let _ = self.apply(ServerDescription::unknown(addr.clone(), Some(error)));
        }
    }

    /// Servers suitable for `sel`, inside the latency window.
    pub fn select(&self, sel: &Selection, local_threshold: Duration, heartbeat: Duration) -> Vec<Address> {
        let known = |s: &&ServerDescription| s.kind != ServerType::Unknown;
        let candidates: Vec<&ServerDescription> = match self.kind {
            TopologyType::Unknown => Vec::new(),
            TopologyType::Single | TopologyType::LoadBalanced => self.servers.values().filter(known).filter(|s| s.kind.is_data_bearing()).collect(),
            TopologyType::Sharded => self.servers.values().filter(|s| s.kind == ServerType::Mongos).collect(),
            TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary => match sel {
                Selection::Write => self.servers.values().filter(|s| s.kind == ServerType::RsPrimary).collect(),
                Selection::Read(rp) => self.read_candidates(rp, heartbeat),
                Selection::Address(a) => self.servers.get(a).filter(|s| s.kind.is_data_bearing()).into_iter().collect(),
            },
        };
        let candidates: Vec<&ServerDescription> = match sel {
            Selection::Address(a) => candidates.into_iter().filter(|s| s.address == *a).collect(),
            _ => candidates,
        };
        let fastest = candidates.iter().filter_map(|s| s.rtt).min().unwrap_or_default();
        candidates.into_iter().filter(|s| s.rtt.unwrap_or_default() <= fastest + local_threshold).map(|s| s.address.clone()).collect()
    }

    fn read_candidates(&self, rp: &ReadPreference, heartbeat: Duration) -> Vec<&ServerDescription> {
        let primary: Vec<&ServerDescription> = self.servers.values().filter(|s| s.kind == ServerType::RsPrimary).collect();
        let secondaries = || -> Vec<&ServerDescription> {
            let mut s: Vec<&ServerDescription> = self.servers.values().filter(|s| s.kind == ServerType::RsSecondary).collect();
            if let Some(max) = rp.max_staleness {
                s.retain(|d| self.staleness(d, heartbeat).is_none_or(|st| st <= max));
            }
            filter_tags(s, &rp.tag_sets)
        };
        match rp.mode {
            ReadMode::Primary => primary,
            ReadMode::PrimaryPreferred => {
                if primary.is_empty() {
                    secondaries()
                } else {
                    primary
                }
            }
            ReadMode::Secondary => secondaries(),
            ReadMode::SecondaryPreferred => {
                let s = secondaries();
                if s.is_empty() {
                    primary
                } else {
                    s
                }
            }
            ReadMode::Nearest => {
                let mut all: Vec<&ServerDescription> = self.servers.values().filter(|s| matches!(s.kind, ServerType::RsPrimary | ServerType::RsSecondary)).collect();
                if let Some(max) = rp.max_staleness {
                    all.retain(|d| d.kind == ServerType::RsPrimary || self.staleness(d, heartbeat).is_none_or(|st| st <= max));
                }
                filter_tags(all, &rp.tag_sets)
            }
        }
    }

    /// A secondary's estimated staleness (the max-staleness spec formula).
    fn staleness(&self, s: &ServerDescription, heartbeat: Duration) -> Option<Duration> {
        let lw = s.last_write?;
        let ms = |d: Duration| d.as_millis() as i64;
        let est = if let Some(p) = self.servers.values().find(|p| p.kind == ServerType::RsPrimary) {
            let plw = p.last_write?;
            let s_upd = s.last_update.elapsed();
            let p_upd = p.last_update.elapsed();
            // (S.lastUpdate − S.lastWrite) − (P.lastUpdate − P.lastWrite) + heartbeat;
            // lastUpdate = now − elapsed, so S.lastUpdate − P.lastUpdate = p_upd − s_upd.
            (ms(p_upd) - ms(s_upd)) + (plw - lw) + ms(heartbeat)
        } else {
            let smax = self.servers.values().filter(|x| x.kind == ServerType::RsSecondary).filter_map(|x| x.last_write).max()?;
            smax - lw + ms(heartbeat)
        };
        Some(Duration::from_millis(est.max(0) as u64))
    }

    /// One line per server, for error messages.
    pub fn summary(&self) -> String {
        let servers: Vec<String> = self
            .servers
            .values()
            .map(|s| match &s.error {
                Some(e) => format!("{} {:?} ({e})", s.address, s.kind),
                None => format!("{} {:?}", s.address, s.kind),
            })
            .collect();
        format!("{:?} [{}]", self.kind, servers.join(", "))
    }
}

fn filter_tags<'a>(servers: Vec<&'a ServerDescription>, tag_sets: &[Vec<(String, String)>]) -> Vec<&'a ServerDescription> {
    if tag_sets.is_empty() {
        return servers;
    }
    for set in tag_sets {
        let matching: Vec<&ServerDescription> = servers.iter().copied().filter(|s| set.iter().all(|t| s.tags.contains(t))).collect();
        if !matching.is_empty() {
            return matching;
        }
    }
    Vec::new()
}

/// What an operation needs from a server.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    /// A writable server.
    Write,
    /// A server matching the read preference.
    Read(ReadPreference),
    /// This server (a cursor's, a pinned transaction's).
    Address(Address),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    fn a(p: u16) -> Address {
        Address::Tcp("h".into(), p)
    }

    fn hello(fields: Document) -> Document {
        let mut d = doc! { "ok" => 1.0, "maxWireVersion" => 25, "logicalSessionTimeoutMinutes" => 30 };
        for (k, v) in fields {
            d.insert(k, v);
        }
        d
    }

    fn rs_hello(me: u16, primary: bool, hosts: &[u16]) -> Document {
        hello(doc! {
            "setName" => "rs0", "isWritablePrimary" => primary, "secondary" => !primary, "me" => format!("h:{me}"),
            "hosts" => hosts.iter().map(|p| Bson::from(format!("h:{p}"))).collect::<Vec<_>>(),
            "electionId" => ObjectId([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]), "setVersion" => 1
        })
    }

    #[test]
    fn standalone_and_sharded_discovery() {
        let mut t = Topology::new(&[a(1)], None, None, false);
        t.apply(ServerDescription::from_hello(a(1), &hello(doc! { "isWritablePrimary" => true }), Duration::from_millis(1)));
        assert_eq!(t.kind, TopologyType::Single);
        assert_eq!(t.select(&Selection::Write, Duration::from_millis(15), Duration::from_secs(10)), vec![a(1)]);

        let mut t = Topology::new(&[a(1), a(2)], None, None, false);
        t.apply(ServerDescription::from_hello(a(1), &hello(doc! { "msg" => "isdbgrid" }), Duration::from_millis(1)));
        assert_eq!(t.kind, TopologyType::Sharded);
        let ch = t.apply(ServerDescription::from_hello(a(2), &hello(doc! { "isWritablePrimary" => true }), Duration::from_millis(1)));
        assert_eq!(ch.removed, vec![a(2)], "a standalone is not part of a sharded cluster");
    }

    #[test]
    fn replica_set_discovery_and_failover() {
        let mut t = Topology::new(&[a(1)], Some("rs0".into()), None, false);
        let ch = t.apply(ServerDescription::from_hello(a(1), &rs_hello(1, false, &[1, 2, 3]), Duration::from_millis(1)));
        assert_eq!(ch.added, vec![a(2), a(3)]);
        assert_eq!(t.kind, TopologyType::ReplicaSetNoPrimary);
        assert!(t.select(&Selection::Write, Duration::from_millis(15), Duration::from_secs(10)).is_empty());
        t.apply(ServerDescription::from_hello(a(2), &rs_hello(2, true, &[1, 2, 3]), Duration::from_millis(1)));
        assert_eq!(t.kind, TopologyType::ReplicaSetWithPrimary);
        assert_eq!(t.select(&Selection::Write, Duration::from_millis(15), Duration::from_secs(10)), vec![a(2)]);
        // Reads by preference.
        let sec = Selection::Read(ReadPreference::new(ReadMode::Secondary));
        assert_eq!(t.select(&sec, Duration::from_millis(15), Duration::from_secs(10)), vec![a(1)]);
        // A new primary with a newer election demotes the old one.
        let mut newer = rs_hello(3, true, &[1, 2, 3]);
        newer.insert("electionId", ObjectId([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]));
        t.apply(ServerDescription::from_hello(a(3), &newer, Duration::from_millis(1)));
        assert_eq!(t.select(&Selection::Write, Duration::from_millis(15), Duration::from_secs(10)), vec![a(3)]);
        assert_eq!(t.servers[&a(2)].kind, ServerType::Unknown);
        // The stale old primary reporting again is ignored.
        t.apply(ServerDescription::from_hello(a(2), &rs_hello(2, true, &[1, 2, 3]), Duration::from_millis(1)));
        assert_eq!(t.servers[&a(2)].kind, ServerType::Unknown);
        // The primary drops a member from its list: it is removed.
        let mut shrunk = newer.clone();
        shrunk.insert("hosts", vec![Bson::from("h:1"), Bson::from("h:3")]);
        let ch = t.apply(ServerDescription::from_hello(a(3), &shrunk, Duration::from_millis(1)));
        assert_eq!(ch.removed, vec![a(2)]);
        // A member of another set is removed.
        let mut other = rs_hello(1, false, &[1, 3]);
        other.insert("setName", "rs9");
        let ch = t.apply(ServerDescription::from_hello(a(1), &other, Duration::from_millis(1)));
        assert_eq!(ch.removed, vec![a(1)]);
    }

    #[test]
    fn max_staleness_skips_lagging_secondaries() {
        let mut t = Topology::new(&[a(1)], Some("rs0".into()), None, false);
        let with_write = |me: u16, primary: bool, ms: i64| {
            let mut h = rs_hello(me, primary, &[1, 2, 3]);
            h.insert("lastWrite", doc! { "lastWriteDate" => crate::bson::DateTime(ms) });
            ServerDescription::from_hello(a(me), &h, Duration::from_millis(1))
        };
        t.apply(with_write(1, true, 1_000_000));
        t.apply(with_write(2, false, 1_000_000 - 5_000));
        t.apply(with_write(3, false, 1_000_000 - 200_000));
        let mut rp = ReadPreference::new(ReadMode::Secondary);
        rp.max_staleness = Some(Duration::from_secs(90));
        assert_eq!(t.select(&Selection::Read(rp), Duration::from_secs(1), Duration::from_secs(10)), vec![a(2)], "200 s behind is too stale");
    }

    #[test]
    fn tags_and_latency_window() {
        let mut t = Topology::new(&[a(1)], Some("rs0".into()), None, false);
        t.apply(ServerDescription::from_hello(a(1), &rs_hello(1, true, &[1, 2, 3]), Duration::from_millis(1)));
        let mut s2 = rs_hello(2, false, &[1, 2, 3]);
        s2.insert("tags", doc! { "dc" => "ny" });
        t.apply(ServerDescription::from_hello(a(2), &s2, Duration::from_millis(2)));
        let mut s3 = rs_hello(3, false, &[1, 2, 3]);
        s3.insert("tags", doc! { "dc" => "sf" });
        t.apply(ServerDescription::from_hello(a(3), &s3, Duration::from_millis(200)));
        let mut rp = ReadPreference::new(ReadMode::Secondary);
        rp.tag_sets = vec![vec![("dc".into(), "sf".into())]];
        assert_eq!(t.select(&Selection::Read(rp), Duration::from_millis(15), Duration::from_secs(10)), vec![a(3)]);
        // Nearest: the slow member is outside the 15 ms window.
        let near = t.select(&Selection::Read(ReadPreference::new(ReadMode::Nearest)), Duration::from_millis(15), Duration::from_secs(10));
        assert_eq!(near, vec![a(1), a(2)]);
        let mut rp = ReadPreference::new(ReadMode::SecondaryPreferred);
        rp.tag_sets = vec![vec![("dc".into(), "tokyo".into())]];
        assert_eq!(t.select(&Selection::Read(rp), Duration::from_millis(15), Duration::from_secs(10)), vec![a(1)], "falls back to the primary");
    }
}
