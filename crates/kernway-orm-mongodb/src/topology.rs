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

/// The wire protocol versions this driver speaks: MongoDB 4.2 (8) to 8.2 (27).
pub const CLIENT_MIN_WIRE_VERSION: i64 = 8;
/// See [`CLIENT_MIN_WIRE_VERSION`].
pub const CLIENT_MAX_WIRE_VERSION: i64 = 27;

/// A server's `topologyVersion`: which of two replies about it is newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologyVersion {
    /// The server process (changes on restart).
    pub process_id: ObjectId,
    /// Bumped on every state change of that process.
    pub counter: i64,
}

impl TopologyVersion {
    /// From a reply's `topologyVersion` field.
    pub fn from_reply(d: &Document) -> Option<Self> {
        let tv = d.get_document("topologyVersion")?;
        let process_id = match tv.get("processId") {
            Some(Bson::ObjectId(o)) => *o,
            _ => return None,
        };
        Some(TopologyVersion { process_id, counter: tv.get_i64("counter")? })
    }
}

/// `compareTopologyVersion` of the SDAM spec: `Less` unless both are known,
/// from the same process, and `current`'s counter is not below `new`'s.
fn compare_topology_version(current: Option<TopologyVersion>, new: Option<TopologyVersion>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (current, new) {
        (Some(c), Some(n)) if c.process_id == n.process_id => c.counter.cmp(&n.counter),
        _ => Ordering::Less,
    }
}

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
    /// Lowest wire version.
    pub min_wire_version: i64,
    /// Highest wire version.
    pub max_wire_version: i64,
    /// The `topologyVersion` of its latest reply (or state-change error).
    pub topology_version: Option<TopologyVersion>,
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
            min_wire_version: 0,
            max_wire_version: 0,
            topology_version: None,
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
        d.min_wire_version = hello.get_i64("minWireVersion").unwrap_or(0);
        d.max_wire_version = hello.get_i64("maxWireVersion").unwrap_or(0);
        d.topology_version = TopologyVersion::from_reply(hello);
        d.logical_session_timeout = hello.get_i64("logicalSessionTimeoutMinutes");
        let primary = hello.get_bool("isWritablePrimary").or(hello.get_bool("ismaster")).unwrap_or(false);
        let ok = match hello.get("ok") {
            Some(Bson::Double(f)) => *f == 1.0,
            Some(Bson::Int32(i)) => *i == 1,
            Some(Bson::Int64(i)) => *i == 1,
            Some(Bson::Boolean(b)) => *b,
            _ => false,
        };
        d.kind = if !ok {
            ServerType::Unknown
        } else if hello.contains_key("serviceId") {
            ServerType::LoadBalancer
        } else if hello.get_str("msg") == Some("isdbgrid") {
            ServerType::Mongos
        } else if hello.get_bool("isreplicaset").unwrap_or(false) {
            ServerType::RsGhost
        } else if d.set_name.is_some() {
            if hello.get_bool("hidden").unwrap_or(false) {
                ServerType::RsOther
            } else if primary {
                ServerType::RsPrimary
            } else if hello.get_bool("secondary").unwrap_or(false) {
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
    /// Why the deployment cannot be used: a server's wire versions do not
    /// overlap this driver's (`None`: compatible).
    pub compatibility_error: Option<String>,
    /// Each server's connection-pool generation, bumped when its pool is
    /// cleared; errors from older connections are ignored.
    generations: BTreeMap<Address, u64>,
    seeds: usize,
}

/// What to do after applying a description.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Servers newly added: start monitoring them.
    pub added: Vec<Address>,
    /// Servers dropped from the topology: stop their monitors / pools.
    pub removed: Vec<Address>,
}

/// An operation's failure, as SDAM error handling sees it.
#[derive(Debug, Clone)]
pub struct ApplicationError {
    /// The server it happened on.
    pub address: Address,
    /// The pool generation of the connection it happened on (`None`: current).
    pub generation: Option<u64>,
    /// Whether the connection had finished its handshake (hello and auth).
    pub completed_handshake: bool,
    /// What went wrong.
    pub kind: ApplicationErrorKind,
}

/// The kinds of [`ApplicationError`].
#[derive(Debug, Clone)]
pub enum ApplicationErrorKind {
    /// A network error other than a timeout.
    Network,
    /// A network timeout.
    Timeout,
    /// A command (or write-concern) error.
    Command {
        /// Its code, if any.
        code: Option<i32>,
        /// Its message.
        message: String,
        /// The reply's `topologyVersion`.
        topology_version: Option<TopologyVersion>,
        /// Its labels (`SystemOverloadedError` is not a server failure).
        labels: Vec<String>,
    },
}

/// What SDAM error handling decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ErrorOutcome {
    /// The server's pool was cleared (its generation bumped).
    pub cleared_pool: bool,
    /// Its monitor should check it at once.
    pub request_check: bool,
}

const RECOVERING_CODES: [i32; 5] = [11600, 11602, 13436, 189, 91];
const NOT_WRITABLE_PRIMARY_CODES: [i32; 3] = [10107, 13435, 10058];
const SHUTDOWN_CODES: [i32; 2] = [11600, 91];

/// "node is recovering" (by code; by message only when there is no code).
pub fn is_recovering(code: Option<i32>, message: &str) -> bool {
    match code {
        Some(c) => RECOVERING_CODES.contains(&c),
        None => message.contains("not master or secondary") || message.contains("node is recovering"),
    }
}

/// "not writable primary" (by code; by message only when there is no code).
pub fn is_not_writable_primary(code: Option<i32>, message: &str) -> bool {
    match code {
        Some(c) => NOT_WRITABLE_PRIMARY_CODES.contains(&c),
        None => !is_recovering(None, message) && message.contains("not master"),
    }
}

/// A state-change error: the server is up but not what the client thinks.
pub fn is_state_change_error(code: Option<i32>, message: &str) -> bool {
    is_recovering(code, message) || is_not_writable_primary(code, message)
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
        let mut servers: BTreeMap<Address, ServerDescription> = seeds.iter().map(|a| (a.clone(), ServerDescription::unknown(a.clone(), None))).collect();
        if load_balanced {
            // The load balancer is never checked: it is a LoadBalancer from the start.
            for s in servers.values_mut() {
                s.kind = ServerType::LoadBalancer;
            }
        }
        let generations = seeds.iter().map(|a| (a.clone(), 0)).collect();
        Topology {
            kind,
            set_name,
            servers,
            max_set_version: None,
            max_election_id: None,
            version: 0,
            session_timeout: None,
            compatibility_error: None,
            generations,
            seeds: seeds.len(),
        }
    }

    /// Whether every data-bearing server supports sessions.
    pub fn supports_sessions(&self) -> bool {
        self.session_timeout.is_some() || self.kind == TopologyType::LoadBalanced
    }

    /// Whether every known server's wire versions overlap this driver's.
    pub fn compatible(&self) -> bool {
        self.compatibility_error.is_none()
    }

    /// The largest `setVersion` a primary reported (part of the stale-primary check).
    pub fn max_set_version(&self) -> Option<i64> {
        self.max_set_version
    }

    /// The largest `electionId` a primary reported.
    pub fn max_election_id(&self) -> Option<ObjectId> {
        self.max_election_id
    }

    /// The server's connection-pool generation (0 until its pool is first cleared).
    pub fn pool_generation(&self, addr: &Address) -> u64 {
        self.generations.get(addr).copied().unwrap_or(0)
    }

    fn clear_pool(&mut self, addr: &Address) {
        *self.generations.entry(addr.clone()).or_insert(0) += 1;
    }

    /// Apply a server check's outcome (the SDAM state machine,
    /// `onServerDescriptionChanged`). A reply about a server no longer in
    /// the topology, or older (by `topologyVersion`) than what is known, is
    /// ignored.
    pub fn apply(&mut self, desc: ServerDescription) -> Changes {
        let mut ch = Changes::default();
        let addr = desc.address.clone();
        let Some(old) = self.servers.get(&addr) else { return ch };
        if compare_topology_version(old.topology_version, desc.topology_version) == std::cmp::Ordering::Greater {
            return ch;
        }
        // Round-trip time: an exponentially weighted average (α = 0.2) while
        // the server stays known; none while it is Unknown.
        let mut desc = desc;
        if desc.kind == ServerType::Unknown {
            desc.rtt = None;
        } else if let (Some(new), Some(prev)) = (desc.rtt, old.rtt) {
            desc.rtt = Some(prev.mul_f64(0.8) + new.mul_f64(0.2));
        }
        self.version += 1;
        match self.kind {
            TopologyType::Single => {
                if self.set_name.is_some() && desc.kind != ServerType::Unknown && desc.set_name != self.set_name {
                    // directConnection with replicaSet: a member of another set is unusable.
                    let e = format!("replica set {:?} is not the configured {:?}", desc.set_name, self.set_name);
                    desc = ServerDescription::unknown(addr.clone(), Some(e));
                }
                self.servers.insert(addr, desc);
            }
            TopologyType::LoadBalanced => {
                // A load balancer stays a LoadBalancer (never checked by a monitor).
                desc.kind = ServerType::LoadBalancer;
                self.servers.insert(addr, desc);
            }
            TopologyType::Unknown => {
                let kind = desc.kind;
                self.servers.insert(addr.clone(), desc.clone());
                match kind {
                    ServerType::Standalone => {
                        // updateUnknownWithStandalone
                        if self.seeds == 1 {
                            self.kind = TopologyType::Single;
                        } else {
                            self.remove(&addr, &mut ch);
                        }
                    }
                    ServerType::Mongos => self.kind = TopologyType::Sharded,
                    ServerType::RsPrimary => {
                        self.kind = TopologyType::ReplicaSetWithPrimary;
                        self.update_rs_from_primary(desc, &mut ch);
                        self.check_if_has_primary();
                    }
                    ServerType::RsSecondary | ServerType::RsArbiter | ServerType::RsOther => {
                        self.kind = TopologyType::ReplicaSetNoPrimary;
                        self.update_rs_without_primary(desc, &mut ch);
                    }
                    _ => {}
                }
            }
            TopologyType::Sharded => {
                let kind = desc.kind;
                self.servers.insert(addr.clone(), desc);
                if !matches!(kind, ServerType::Unknown | ServerType::Mongos) {
                    self.remove(&addr, &mut ch);
                }
            }
            TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary => {
                let kind = desc.kind;
                self.servers.insert(addr.clone(), desc.clone());
                match kind {
                    ServerType::Standalone | ServerType::Mongos => self.remove(&addr, &mut ch),
                    ServerType::RsPrimary => self.update_rs_from_primary(desc, &mut ch),
                    ServerType::RsSecondary | ServerType::RsArbiter | ServerType::RsOther => {
                        if self.kind == TopologyType::ReplicaSetWithPrimary {
                            self.update_rs_with_primary_from_member(desc, &mut ch);
                        } else {
                            self.update_rs_without_primary(desc, &mut ch);
                        }
                    }
                    _ => {}
                }
                self.check_if_has_primary();
            }
        }
        self.after_update();
        ch
    }

    /// Compatibility and the deployment's session timeout, after any change.
    fn after_update(&mut self) {
        self.compatibility_error = self.servers.values().filter(|s| s.kind != ServerType::Unknown).find_map(|s| {
            if s.min_wire_version > CLIENT_MAX_WIRE_VERSION {
                Some(format!(
                    "Server at {} requires wire version {}, but this version of kernway-orm-mongodb only supports up to {CLIENT_MAX_WIRE_VERSION}.",
                    s.address, s.min_wire_version
                ))
            } else if s.max_wire_version < CLIENT_MIN_WIRE_VERSION {
                Some(format!(
                    "Server at {} reports wire version {}, but this version of kernway-orm-mongodb requires at least {CLIENT_MIN_WIRE_VERSION} (MongoDB 4.2).",
                    s.address, s.max_wire_version
                ))
            } else {
                None
            }
        });
        let timeouts: Vec<Option<i64>> = self.servers.values().filter(|s| s.kind.is_data_bearing()).map(|s| s.logical_session_timeout).collect();
        self.session_timeout = if timeouts.is_empty() || timeouts.iter().any(Option::is_none) { None } else { timeouts.into_iter().flatten().min() };
    }

    fn remove(&mut self, addr: &Address, ch: &mut Changes) {
        if self.servers.remove(addr).is_some() {
            self.generations.remove(addr);
            ch.removed.push(addr.clone());
        }
    }

    fn check_if_has_primary(&mut self) {
        self.kind = if self.servers.values().any(|s| s.kind == ServerType::RsPrimary) {
            TopologyType::ReplicaSetWithPrimary
        } else {
            TopologyType::ReplicaSetNoPrimary
        };
    }

    fn add_hosts(&mut self, hosts: &[Address], ch: &mut Changes) {
        for h in hosts {
            if !self.servers.contains_key(h) {
                self.servers.insert(h.clone(), ServerDescription::unknown(h.clone(), None));
                self.generations.insert(h.clone(), 0);
                ch.added.push(h.clone());
            }
        }
    }

    fn update_rs_without_primary(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if self.set_name.is_none() {
            self.set_name = desc.set_name.clone();
        } else if desc.set_name != self.set_name {
            self.remove(&addr, ch);
            return;
        }
        self.add_hosts(&desc.hosts, ch);
        // (PossiblePrimary is synonymous with Unknown for an asynchronous client.)
        if desc.me.as_ref().is_some_and(|me| *me != addr) {
            // Reached through an alias: the set knows it by another name.
            self.remove(&addr, ch);
        }
    }

    fn update_rs_with_primary_from_member(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if desc.set_name != self.set_name || desc.me.as_ref().is_some_and(|me| *me != addr) {
            self.remove(&addr, ch);
        }
    }

    fn update_rs_from_primary(&mut self, desc: ServerDescription, ch: &mut Changes) {
        let addr = desc.address.clone();
        if self.set_name.is_none() {
            self.set_name = desc.set_name.clone();
        } else if desc.set_name != self.set_name {
            self.remove(&addr, ch);
            return;
        }
        let stale = |td: &Topology| {
            format!(
                "primary marked stale due to electionId/setVersion mismatch, ({:?}, {:?}) is stale compared to ({:?}, {:?})",
                desc.election_id.map(|o| o.to_hex()),
                desc.set_version,
                td.max_election_id.map(|o| o.to_hex()),
                td.max_set_version
            )
        };
        if desc.max_wire_version >= 17 {
            // MongoDB 6.0+: electionId first, then setVersion; null is lowest.
            if (desc.election_id, desc.set_version) >= (self.max_election_id, self.max_set_version) {
                self.max_election_id = desc.election_id;
                self.max_set_version = desc.set_version;
            } else {
                let e = stale(self);
                self.servers.insert(addr.clone(), ServerDescription::unknown(addr, Some(e)));
                return;
            }
        } else {
            // Older servers: setVersion first, then electionId.
            if let (Some(sv), Some(eid)) = (desc.set_version, desc.election_id) {
                if let (Some(msv), Some(meid)) = (self.max_set_version, self.max_election_id) {
                    if msv > sv || (msv == sv && meid > eid) {
                        let e = stale(self);
                        self.servers.insert(addr.clone(), ServerDescription::unknown(addr, Some(e)));
                        return;
                    }
                }
                self.max_election_id = Some(eid);
            }
            if let Some(sv) = desc.set_version {
                if self.max_set_version.is_none_or(|m| sv > m) {
                    self.max_set_version = Some(sv);
                }
            }
        }
        // Any other primary is no longer one (its pool is kept).
        let others: Vec<Address> = self.servers.values().filter(|s| s.kind == ServerType::RsPrimary && s.address != addr).map(|s| s.address.clone()).collect();
        for o in others {
            self.servers.insert(o.clone(), ServerDescription::unknown(o, Some("primary marked stale due to discovery of newer primary".into())));
        }
        self.add_hosts(&desc.hosts, ch);
        // Members the primary does not list are gone.
        let gone: Vec<Address> = self.servers.keys().filter(|a| !desc.hosts.contains(a)).cloned().collect();
        for g in gone {
            self.remove(&g, ch);
        }
    }

    /// A monitor's check failed (a network error): the server becomes
    /// Unknown and its pool is cleared (the server-monitoring spec).
    pub fn check_failed(&mut self, addr: &Address, error: String) -> Changes {
        if !self.servers.contains_key(addr) {
            return Changes::default();
        }
        self.clear_pool(addr);
        self.apply(ServerDescription::unknown(addr.clone(), Some(error)))
    }

    /// SDAM error handling for an operation's failure (`handleError`):
    /// ignores errors from older pool generations or older
    /// `topologyVersion`s, and timeouts; marks the server Unknown on
    /// state-change errors, network errors and handshake (auth) failures;
    /// clears the pool on the latter two and on "node is shutting down".
    pub fn handle_application_error(&mut self, e: &ApplicationError) -> ErrorOutcome {
        let mut out = ErrorOutcome::default();
        let Some(current) = self.servers.get(&e.address) else { return out };
        if e.generation.is_some_and(|g| g < self.pool_generation(&e.address)) {
            return out; // from a connection the pool already discarded
        }
        let lb = self.kind == TopologyType::LoadBalanced;
        match &e.kind {
            ApplicationErrorKind::Command { code, message, topology_version, .. } if is_state_change_error(*code, message) => {
                if compare_topology_version(current.topology_version, *topology_version) != std::cmp::Ordering::Less {
                    return out; // the server's state has moved on since
                }
                if !lb {
                    let mut unknown = ServerDescription::unknown(e.address.clone(), Some(message.clone()));
                    unknown.topology_version = *topology_version;
                    self.apply(unknown);
                }
                if code.is_some_and(|c| SHUTDOWN_CODES.contains(&c)) {
                    self.clear_pool(&e.address);
                    out.cleared_pool = true;
                }
                out.request_check = true;
            }
            kind => {
                // A timeout may be a slow operation, not a dead server: never
                // acted on (the SDAM tests: before or after the handshake).
                // A command error matters only before the handshake completes
                // (e.g. authentication).
                match kind {
                    ApplicationErrorKind::Timeout => return out,
                    ApplicationErrorKind::Command { .. } if e.completed_handshake => return out,
                    _ => {}
                }
                if let ApplicationErrorKind::Command { labels, .. } = kind {
                    if labels.iter().any(|l| l == "SystemOverloadedError") {
                        return out;
                    }
                }
                if !lb {
                    let why = match kind {
                        ApplicationErrorKind::Network => "network error".to_string(),
                        ApplicationErrorKind::Timeout => "network timeout".to_string(),
                        ApplicationErrorKind::Command { message, .. } => message.clone(),
                    };
                    self.apply(ServerDescription::unknown(e.address.clone(), Some(why)));
                }
                self.clear_pool(&e.address);
                out.cleared_pool = true;
            }
        }
        out
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
        self.select_avoiding(sel, local_threshold, heartbeat, &[])
    }

    /// [`Self::select`], avoiding `deprioritized` servers (those an
    /// operation's earlier attempt failed on) unless nothing else is
    /// suitable — the server selection spec's algorithm, steps 3 and 5.
    pub fn select_avoiding(&self, sel: &Selection, local_threshold: Duration, heartbeat: Duration, deprioritized: &[Address]) -> Vec<Address> {
        let mut suitable = self.suitable(sel, heartbeat, deprioritized);
        if suitable.is_empty() && !deprioritized.is_empty() {
            suitable = self.suitable(sel, heartbeat, &[]);
        }
        latency_window(suitable, local_threshold)
    }

    /// The suitable servers for `sel` (before the latency window), leaving out `skip`.
    pub fn suitable(&self, sel: &Selection, heartbeat: Duration, skip: &[Address]) -> Vec<&ServerDescription> {
        let usable = |s: &&ServerDescription| !skip.contains(&s.address);
        match self.kind {
            TopologyType::Unknown => Vec::new(),
            // A direct connection: the one server, whatever its type, once it is known.
            TopologyType::Single => self.servers.values().filter(usable).filter(|s| s.kind != ServerType::Unknown).collect(),
            TopologyType::LoadBalanced => self.servers.values().filter(usable).filter(|s| s.kind == ServerType::LoadBalancer).collect(),
            TopologyType::Sharded => self.servers.values().filter(usable).filter(|s| s.kind == ServerType::Mongos).collect(),
            TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary => match sel {
                Selection::Write => self.servers.values().filter(usable).filter(|s| s.kind == ServerType::RsPrimary).collect(),
                Selection::Read(rp) => self.read_candidates(rp, heartbeat, skip),
                Selection::Address(a) => self.servers.get(a).filter(|s| s.kind.is_data_bearing()).into_iter().collect(),
            },
        }
        .into_iter()
        .filter(|s| match sel {
            Selection::Address(a) => s.address == *a,
            _ => true,
        })
        .collect()
    }

    /// Why `rp` cannot be used against this topology (the server selection
    /// spec's maxStalenessSeconds rules), or `None`.
    pub fn read_preference_error(&self, rp: &ReadPreference, heartbeat: Duration) -> Option<String> {
        let max = rp.max_staleness?;
        if rp.mode == ReadMode::Primary {
            return Some("maxStalenessSeconds cannot be combined with readPreference primary".into());
        }
        if matches!(self.kind, TopologyType::ReplicaSetNoPrimary | TopologyType::ReplicaSetWithPrimary) {
            let floor = (heartbeat + IDLE_WRITE_PERIOD).max(SMALLEST_MAX_STALENESS);
            if max < floor {
                return Some(format!("maxStalenessSeconds must be at least {} seconds (heartbeatFrequencyMS + 10 s, and at least 90 s)", floor.as_secs()));
            }
        }
        None
    }

    fn read_candidates(&self, rp: &ReadPreference, heartbeat: Duration, skip: &[Address]) -> Vec<&ServerDescription> {
        let of = |k: ServerType| -> Vec<&ServerDescription> { self.servers.values().filter(|s| s.kind == k && !skip.contains(&s.address)).collect() };
        let secondaries = || filter_tags(self.fresh(of(ServerType::RsSecondary), rp, heartbeat), &rp.tag_sets);
        match rp.mode {
            ReadMode::Primary => of(ServerType::RsPrimary),
            ReadMode::PrimaryPreferred => {
                let p = of(ServerType::RsPrimary);
                if p.is_empty() {
                    secondaries()
                } else {
                    p
                }
            }
            ReadMode::Secondary => secondaries(),
            ReadMode::SecondaryPreferred => {
                let s = secondaries();
                if s.is_empty() {
                    of(ServerType::RsPrimary)
                } else {
                    s
                }
            }
            ReadMode::Nearest => {
                let mut all = of(ServerType::RsPrimary);
                all.extend(of(ServerType::RsSecondary));
                filter_tags(self.fresh(all, rp, heartbeat), &rp.tag_sets)
            }
        }
    }

    /// Drop the secondaries staler than `rp.max_staleness` (others have none).
    fn fresh<'a>(&'a self, mut s: Vec<&'a ServerDescription>, rp: &ReadPreference, heartbeat: Duration) -> Vec<&'a ServerDescription> {
        if let Some(max) = rp.max_staleness {
            s.retain(|d| d.kind != ServerType::RsSecondary || self.staleness(d, heartbeat).is_none_or(|st| st <= max));
        }
        s
    }

    /// A secondary's estimated staleness (the max-staleness spec formula).
    fn staleness(&self, s: &ServerDescription, heartbeat: Duration) -> Option<Duration> {
        let lw = s.last_write?;
        let ms = |d: Duration| d.as_millis() as i64;
        // Signed milliseconds from b to a.
        let diff = |a: Instant, b: Instant| if a >= b { ms(a - b) } else { -ms(b - a) };
        let est = if let Some(p) = self.servers.values().find(|p| p.kind == ServerType::RsPrimary) {
            let plw = p.last_write?;
            // (S.lastUpdateTime − S.lastWriteDate) − (P.lastUpdateTime − P.lastWriteDate) + heartbeat
            diff(s.last_update, p.last_update) - lw + plw + ms(heartbeat)
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

/// The max-staleness spec's idleWritePeriodMS.
const IDLE_WRITE_PERIOD: Duration = Duration::from_secs(10);
/// The max-staleness spec's smallestMaxStalenessSeconds.
const SMALLEST_MAX_STALENESS: Duration = Duration::from_secs(90);

/// The suitable servers within `local_threshold` of the fastest one.
fn latency_window(suitable: Vec<&ServerDescription>, local_threshold: Duration) -> Vec<Address> {
    let fastest = suitable.iter().filter_map(|s| s.rtt).min().unwrap_or_default();
    suitable.into_iter().filter(|s| s.rtt.unwrap_or_default() <= fastest + local_threshold).map(|s| s.address.clone()).collect()
}

/// Pick one server from the latency window (the server selection spec,
/// steps 6–7): two at random, the one running fewer operations wins.
/// `random(n)` returns a number below `n`.
pub fn choose_in_window(window: &[Address], operation_count: impl Fn(&Address) -> u64, mut random: impl FnMut(usize) -> usize) -> Option<&Address> {
    match window.len() {
        0 => None,
        1 => Some(&window[0]),
        n => {
            let i = random(n);
            let mut j = random(n - 1);
            if j >= i {
                j += 1;
            }
            let (a, b) = (&window[i], &window[j]);
            Some(if operation_count(b) < operation_count(a) { b } else { a })
        }
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
