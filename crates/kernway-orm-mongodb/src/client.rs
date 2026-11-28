//! The client: topology monitors, per-server connection pools, server
//! selection, sessions, and running a command with every rule around it
//! (`lsid`, `$clusterTime` gossip, causal consistency, transactions,
//! retryable reads and writes, pool clearing on state changes).

use crate::bson::{Binary, Bson, Document, Timestamp};
use crate::conn::{ConnectSpec, Connection};
use crate::error::{Error, ErrorKind, Result};
use crate::options::{Address, ClientOptions, ReadConcern, ReadMode, ReadPreference, WriteConcern};
use crate::topology::{ApplicationError, Selection, ServerDescription, ServerType, Topology, TopologyType};
use crate::wire::Sequence;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Opens connections for one server's pool, tagged with the pool generation.
pub(crate) struct ServerConnect {
    address: Address,
    spec: Rc<ConnectSpec>,
    generation: Rc<Cell<u64>>,
}

impl kernway_wire::Connect for ServerConnect {
    type Conn = Connection;
    type Error = Error;

    async fn connect(&self) -> Result<Connection> {
        let mut c = Connection::open(&self.address, &self.spec, true).await?;
        c.generation = self.generation.get();
        Ok(c)
    }
}

pub(crate) struct ServerPool {
    pool: Rc<kernway_wire::Pool<ServerConnect>>,
    generation: Rc<Cell<u64>>,
}

pub(crate) type Pooled = kernway_wire::Pooled<ServerConnect>;

impl ServerPool {
    /// A connection of the current generation.
    async fn acquire(&self) -> Result<Pooled> {
        loop {
            let mut g = self.pool.acquire().await?;
            if g.generation == self.generation.get() && !g.is_broken() {
                return Ok(g);
            }
            g.poison();
        }
    }

    /// Discard every connection opened before now.
    fn clear(&self) {
        self.generation.set(self.generation.get() + 1);
    }
}

/// A server session (`lsid`), reused across operations.
#[derive(Debug, Clone)]
pub(crate) struct ServerSession {
    pub id: Document,
    pub txn_number: i64,
    last_use: Instant,
    dirty: bool,
}

impl ServerSession {
    fn new() -> Self {
        let mut uuid = [0u8; 16];
        let _ = kernway_tls::fill_random(&mut uuid);
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        ServerSession { id: Document::new().with("id", Binary { subtype: 4, bytes: uuid.to_vec() }), txn_number: 0, last_use: Instant::now(), dirty: false }
    }
}

pub(crate) struct ClientInner {
    pub options: ClientOptions,
    spec: Rc<ConnectSpec>,
    monitor_spec: Rc<ConnectSpec>,
    pub topology: RefCell<Topology>,
    pools: RefCell<HashMap<Address, Rc<ServerPool>>>,
    sessions: RefCell<Vec<ServerSession>>,
    cluster_time: RefCell<Option<Document>>,
    check_now: Cell<u64>,
    /// When the SRV records were last read (mongodb+srv:// polling).
    srv_checked: Cell<Option<Instant>>,
    /// Operations in flight per server (`operationCount`: selection prefers
    /// the less busy of two servers in the latency window).
    in_flight: RefCell<HashMap<Address, u64>>,
}

/// One operation counted against its server until dropped.
pub(crate) struct InFlight {
    inner: Rc<ClientInner>,
    address: Address,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(n) = self.inner.in_flight.borrow_mut().get_mut(&self.address) {
            *n = n.saturating_sub(1);
        }
    }
}

/// A MongoDB client: cheap to clone, `!Send` — one per shard, like the
/// rest of the thread-per-core stack. Monitors run as tasks on the shard
/// that created it and stop when the last clone is dropped.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Rc<ClientInner>,
}

/// Transaction state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxnState {
    None,
    Starting,
    InProgress,
    Committed,
    Aborted,
}

/// Transaction settings.
#[derive(Debug, Clone, Default)]
pub struct TransactionOptions {
    /// Read concern inside the transaction (default: the client's).
    pub read_concern: Option<ReadConcern>,
    /// Write concern of the commit (default: the client's).
    pub write_concern: Option<WriteConcern>,
    /// Must be primary (the default) for transactions.
    pub read_preference: Option<ReadPreference>,
    /// `maxCommitTimeMS`.
    pub max_commit_time: Option<Duration>,
}

pub(crate) struct SessionState {
    pub server: Option<ServerSession>,
    pub causal: bool,
    pub operation_time: Option<Timestamp>,
    pub cluster_time: Option<Document>,
    pub txn: TxnState,
    pub txn_options: TransactionOptions,
    /// The mongos a sharded transaction is pinned to.
    pub pinned: Option<Address>,
    pub implicit: bool,
    /// A snapshot session's read timestamp.
    pub snapshot_time: Option<Timestamp>,
    pub snapshot: bool,
}

/// An explicit session: causally consistent reads, and transactions.
#[derive(Clone)]
pub struct Session {
    pub(crate) client: Client,
    pub(crate) state: Rc<RefCell<SessionState>>,
}

/// How an operation may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retry {
    No,
    Read,
    Write,
}

/// One command to run.
pub(crate) struct Op<'a> {
    pub db: &'a str,
    pub command: Document,
    pub seqs: &'a [Sequence<'a>],
    pub selection: Selection,
    pub retry: Retry,
    /// Attach the read concern (reads), the write concern (writes).
    pub read_concern: bool,
    pub write_concern: bool,
    pub session: Option<&'a Session>,
}

/// A command's reply and where it ran.
pub(crate) struct Ran {
    pub reply: Document,
    pub address: Address,
    /// The implicit session, kept for cursors that need it for getMore.
    pub implicit: Option<Session>,
}

impl Client {
    /// Connect with a connection string. The client discovers the
    /// deployment in the background; this returns once the seeds are
    /// being monitored (a bad URL fails at once; an unreachable server
    /// when the first operation finds no server).
    pub async fn connect(uri: &str) -> Result<Self> {
        Self::with_options(ClientOptions::parse(uri)?).await
    }

    /// Connect with explicit options.
    pub async fn with_options(mut options: ClientOptions) -> Result<Self> {
        // The Connection String spec asks for a WARN-level message per warning;
        // they hold no text from the URI.
        for w in &options.warnings {
            kernway_log::warn!(target: "kernway_orm_mongodb", "connection string: {w}");
        }
        if let Some(name) = options.srv.clone() {
            crate::dns::apply_srv(&mut options, &name).await?;
        }
        let spec = Rc::new(ConnectSpec::from_options(&options).await?);
        let mut mspec = (*spec).clone();
        mspec.credential = None;
        mspec.compressors.clear();
        let topology = Topology::new(&options.hosts, options.replica_set.clone(), options.direct_connection, options.load_balanced);
        let inner = Rc::new(ClientInner {
            options,
            spec,
            monitor_spec: Rc::new(mspec),
            topology: RefCell::new(topology),
            pools: RefCell::new(HashMap::new()),
            sessions: RefCell::new(Vec::new()),
            cluster_time: RefCell::new(None),
            check_now: Cell::new(0),
            srv_checked: Cell::new(Some(Instant::now())),
            in_flight: RefCell::new(HashMap::new()),
        });
        let client = Client { inner };
        if client.inner.options.load_balanced {
            // No monitoring behind a load balancer: the one address is the server.
            let addr = client.inner.options.hosts[0].clone();
            let mut d = ServerDescription::unknown(addr.clone(), None);
            d.kind = ServerType::LoadBalancer;
            d.rtt = Some(Duration::ZERO);
            d.logical_session_timeout = Some(30);
            client.inner.topology.borrow_mut().apply(d);
        } else {
            let seeds: Vec<Address> = client.inner.topology.borrow().servers.keys().cloned().collect();
            for a in seeds {
                client.start_monitor(a);
            }
        }
        Ok(client)
    }

    /// The options.
    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    /// A snapshot of the topology.
    pub fn topology(&self) -> Topology {
        self.inner.topology.borrow().clone()
    }

    fn start_monitor(&self, address: Address) {
        let weak = Rc::downgrade(&self.inner);
        rt_core::spawn(monitor(weak, address));
    }

    fn pool(&self, address: &Address) -> Rc<ServerPool> {
        let mut pools = self.inner.pools.borrow_mut();
        pools
            .entry(address.clone())
            .or_insert_with(|| {
                let generation = Rc::new(Cell::new(0));
                let connect = ServerConnect { address: address.clone(), spec: self.inner.spec.clone(), generation: generation.clone() };
                Rc::new(ServerPool { pool: kernway_wire::Pool::new(connect, match self.inner.options.max_pool_size {
                    0 => usize::MAX, // maxPoolSize=0: no limit
                    n => n,
                }), generation })
            })
            .clone()
    }

    /// Ask every monitor to check now (after an error, or while selecting).
    pub(crate) fn request_check(&self) {
        self.inner.check_now.set(self.inner.check_now.get() + 1);
    }

    /// A server for `sel`, waiting up to `serverSelectionTimeoutMS`.
    pub(crate) async fn select(&self, sel: &Selection) -> Result<Address> {
        self.select_avoiding(sel, &[]).await
    }

    /// Count an operation against `address` until the guard is dropped.
    pub(crate) fn in_flight(&self, address: &Address) -> InFlight {
        *self.inner.in_flight.borrow_mut().entry(address.clone()).or_insert(0) += 1;
        InFlight { inner: self.inner.clone(), address: address.clone() }
    }

    /// The server selection spec's algorithm: fail at once on an
    /// incompatible topology or an unusable read preference; prefer servers
    /// not in `deprioritized` (an earlier attempt's); of two random servers
    /// in the latency window take the one with fewer operations in flight;
    /// otherwise wait for the topology to change.
    pub(crate) async fn select_avoiding(&self, sel: &Selection, deprioritized: &[Address]) -> Result<Address> {
        let o = &self.inner.options;
        let deadline = Instant::now() + o.server_selection_timeout;
        let mut first = true;
        loop {
            {
                let t = self.inner.topology.borrow();
                if let Some(e) = &t.compatibility_error {
                    return Err(Error::new(ErrorKind::ServerSelection, e.clone()));
                }
                if let Selection::Read(rp) = sel {
                    if let Some(e) = t.read_preference_error(rp, o.heartbeat_frequency) {
                        return Err(Error::config(e));
                    }
                }
                let found = t.select_avoiding(sel, o.local_threshold, o.heartbeat_frequency, deprioritized);
                let counts = self.inner.in_flight.borrow();
                let random = |n: usize| {
                    let mut b = [0u8; 8];
                    let _ = kernway_tls::fill_random(&mut b);
                    (u64::from_le_bytes(b) % n as u64) as usize
                };
                if let Some(a) = crate::topology::choose_in_window(&found, |a| counts.get(a).copied().unwrap_or(0), random) {
                    return Ok(a.clone());
                }
                if Instant::now() >= deadline {
                    return Err(Error::new(
                        ErrorKind::ServerSelection,
                        format!("no server for {sel:?} within {:?}: {}", o.server_selection_timeout, t.summary()),
                    ));
                }
            }
            if first {
                self.request_check();
                first = false;
            }
            rt_core::sleep(Duration::from_millis(10)).await;
            self.maybe_rescan_srv().await;
        }
    }

    async fn maybe_rescan_srv(&self) {
        let o = &self.inner.options;
        let Some(name) = o.srv.clone() else { return };
        let due = self.inner.srv_checked.get().is_none_or(|t| t.elapsed() > Duration::from_secs(60));
        let kind = self.inner.topology.borrow().kind;
        if !due || !matches!(kind, TopologyType::Sharded | TopologyType::Unknown) {
            return;
        }
        self.inner.srv_checked.set(Some(Instant::now()));
        if let Ok(hosts) = crate::dns::srv_hosts(&name, &o.srv_service_name, o.srv_max_hosts).await {
            let added: Vec<Address> = {
                let mut t = self.inner.topology.borrow_mut();
                let mut added = Vec::new();
                for h in &hosts {
                    if !t.servers.contains_key(h) {
                        t.servers.insert(h.clone(), ServerDescription::unknown(h.clone(), None));
                        added.push(h.clone());
                    }
                }
                t.servers.retain(|a, _| hosts.contains(a));
                t.version += 1;
                added
            };
            for a in added {
                self.start_monitor(a);
            }
        }
    }

    pub(crate) async fn connection(&self, address: &Address) -> Result<Pooled> {
        let pool = self.pool(address);
        match pool.acquire().await {
            Ok(c) => Ok(c),
            Err(e) => {
                // Opening the connection (hello, authentication) failed.
                self.server_failed(address, &e, None, false);
                Err(e)
            }
        }
    }

    /// An operation failed on `address`: SDAM error handling decides
    /// (`Topology::handle_application_error`) whether the server becomes
    /// Unknown and its pool is cleared — on a connection of pool
    /// `generation` (`None`: while opening one), with or without a
    /// completed handshake.
    pub(crate) fn server_failed(&self, address: &Address, e: &Error, generation: Option<u64>, completed_handshake: bool) {
        let Some(kind) = e.sdam_kind() else { return };
        let err = ApplicationError { address: address.clone(), generation, completed_handshake, kind };
        let out = self.inner.topology.borrow_mut().handle_application_error(&err);
        if out.cleared_pool {
            if let Some(p) = self.inner.pools.borrow().get(address) {
                p.clear();
            }
        }
        if out.request_check {
            self.request_check();
        }
    }

    fn checkout_session(&self) -> ServerSession {
        let timeout = self.inner.topology.borrow().session_timeout.unwrap_or(30);
        let limit = Duration::from_secs((timeout.max(2) as u64 - 1) * 60);
        let mut pool = self.inner.sessions.borrow_mut();
        while let Some(s) = pool.pop() {
            if s.last_use.elapsed() < limit {
                return s;
            }
        }
        ServerSession::new()
    }

    pub(crate) fn checkin_session(&self, mut s: ServerSession) {
        if s.dirty {
            return;
        }
        s.last_use = Instant::now();
        self.inner.sessions.borrow_mut().push(s);
    }

    /// Start an explicit session (causally consistent by default).
    pub fn start_session(&self) -> Session {
        self.new_session(false, true, false)
    }

    /// A snapshot session (reads at one point in time; no writes).
    pub fn start_snapshot_session(&self) -> Session {
        self.new_session(false, false, true)
    }

    pub(crate) fn new_session(&self, implicit: bool, causal: bool, snapshot: bool) -> Session {
        Session {
            client: self.clone(),
            state: Rc::new(RefCell::new(SessionState {
                server: None,
                causal,
                operation_time: None,
                cluster_time: None,
                txn: TxnState::None,
                txn_options: TransactionOptions::default(),
                pinned: None,
                implicit,
                snapshot_time: None,
                snapshot,
            })),
        }
    }

    fn gossip(&self, reply: &Document, session: Option<&Session>) {
        if let Some(Bson::Document(ct)) = reply.get("$clusterTime") {
            let newer = |old: &Option<Document>| match (old.as_ref().and_then(|d| d.get("clusterTime")), ct.get("clusterTime")) {
                (Some(Bson::Timestamp(a)), Some(Bson::Timestamp(b))) => b > a,
                _ => true,
            };
            if newer(&self.inner.cluster_time.borrow()) {
                *self.inner.cluster_time.borrow_mut() = Some(ct.clone());
            }
            if let Some(s) = session {
                let mut st = s.state.borrow_mut();
                if newer(&st.cluster_time) {
                    st.cluster_time = Some(ct.clone());
                }
            }
        }
        if let (Some(s), Some(Bson::Timestamp(t))) = (session, reply.get("operationTime")) {
            let mut st = s.state.borrow_mut();
            if st.operation_time.is_none_or(|o| *t > o) {
                st.operation_time = Some(*t);
            }
        }
        if let (Some(s), Some(Bson::Timestamp(t))) = (session, reply.get("atClusterTime").or(reply.get_document("cursor").and_then(|c| c.get("atClusterTime")))) {
            let mut st = s.state.borrow_mut();
            if st.snapshot && st.snapshot_time.is_none() {
                st.snapshot_time = Some(*t);
            }
        }
    }

    /// Run one operation with every rule around it.
    pub(crate) async fn execute(&self, op: Op<'_>) -> Result<Ran> {
        let supports_sessions = {
            let t = self.inner.topology.borrow();
            t.supports_sessions() || t.kind == TopologyType::Unknown
        };
        // An implicit session when the caller gave none.
        let implicit = match op.session {
            None if supports_sessions => Some(self.new_session(true, false, false)),
            _ => None,
        };
        let session = op.session.or(implicit.as_ref());
        let in_txn = session.is_some_and(|s| matches!(s.state.borrow().txn, TxnState::Starting | TxnState::InProgress));
        let cmd_name = op.command.first_key().unwrap_or("").to_string();
        let is_txn_end = cmd_name == "commitTransaction" || cmd_name == "abortTransaction";
        let mut retry = if in_txn && !is_txn_end { Retry::No } else { op.retry };
        let o = &self.inner.options;
        if retry == Retry::Write && (!o.retry_writes || o.write_concern.is_unacknowledged()) {
            retry = Retry::No;
        }
        if retry == Retry::Read && !o.retry_reads {
            retry = Retry::No;
        }
        let mut selection = op.selection.clone();
        if let Some(s) = session {
            if let Some(p) = s.state.borrow().pinned.clone() {
                selection = Selection::Address(p);
            }
        }
        let mut attempt = 0;
        let mut txn_number = None;
        // A retry avoids the server the first attempt failed on, if it can.
        let mut deprioritized: Vec<Address> = Vec::new();
        let mut last: Option<Address> = None;
        loop {
            attempt += 1;
            if let Some(prev) = last.take() {
                deprioritized.push(prev);
            }
            let address = self.select_avoiding(&selection, &deprioritized).await?;
            let _in_flight = self.in_flight(&address);
            last = Some(address.clone());
            let (kind, wire) = {
                let t = self.inner.topology.borrow();
                let s = t.servers.get(&address);
                (s.map_or(ServerType::Unknown, |s| s.kind), s.map_or(0, |s| s.max_wire_version))
            };
            if retry == Retry::Write && (kind == ServerType::Standalone || session.is_none()) {
                retry = Retry::No;
            }
            let mut conn = match self.connection(&address).await {
                Ok(c) => c,
                Err(e) if retry != Retry::No && attempt == 1 && e.is_retryable() => continue,
                Err(e) => return Err(e),
            };
            let mut cmd = op.command.clone();
            // Session, transaction and retry fields.
            if let Some(s) = session {
                let mut st = s.state.borrow_mut();
                let srv = st.server.get_or_insert_with(|| self.checkout_session());
                cmd.insert("lsid", srv.id.clone());
                if in_txn || is_txn_end {
                    cmd.insert("txnNumber", Bson::Int64(srv.txn_number));
                    cmd.insert("autocommit", false);
                    if st.txn == TxnState::Starting {
                        cmd.insert("startTransaction", true);
                        let rc = st.txn_options.read_concern.clone().or_else(|| o.read_concern.clone());
                        let mut rcd = rc.map(|r| Document::new().with("level", r.0)).unwrap_or_default();
                        if st.causal {
                            if let Some(t) = st.operation_time {
                                rcd.insert("afterClusterTime", t);
                            }
                        }
                        if !rcd.is_empty() {
                            cmd.insert("readConcern", rcd);
                        }
                    }
                } else if retry == Retry::Write {
                    if txn_number.is_none() {
                        srv.txn_number += 1;
                        txn_number = Some(srv.txn_number);
                    }
                    cmd.insert("txnNumber", Bson::Int64(txn_number.unwrap_or(1)));
                }
                if !in_txn && op.read_concern {
                    let mut rcd = o.read_concern.clone().map(|r| Document::new().with("level", r.0)).unwrap_or_default();
                    if st.snapshot {
                        rcd.insert("level", "snapshot");
                        if let Some(t) = st.snapshot_time {
                            rcd.insert("atClusterTime", t);
                        }
                    } else if st.causal {
                        if let Some(t) = st.operation_time {
                            rcd.insert("afterClusterTime", t);
                        }
                    }
                    if !rcd.is_empty() && !cmd.contains_key("readConcern") {
                        cmd.insert("readConcern", rcd);
                    }
                }
            } else if op.read_concern {
                if let Some(rc) = &o.read_concern {
                    cmd.insert("readConcern", Document::new().with("level", rc.0.clone()));
                }
            }
            if op.write_concern && !in_txn && !cmd.contains_key("writeConcern") {
                let wc = if is_txn_end {
                    session.and_then(|s| s.state.borrow().txn_options.write_concern.clone()).unwrap_or_else(|| o.write_concern.clone())
                } else {
                    o.write_concern.clone()
                };
                if !wc.is_default() {
                    cmd.insert("writeConcern", wc.to_document());
                }
            }
            // $clusterTime gossip (not to standalones).
            if kind != ServerType::Standalone && wire >= 6 {
                let ct = {
                    let mine = self.inner.cluster_time.borrow().clone();
                    let theirs = session.and_then(|s| s.state.borrow().cluster_time.clone());
                    match (mine, theirs) {
                        (Some(a), Some(b)) => {
                            let ta = a.get("clusterTime").cloned();
                            let tb = b.get("clusterTime").cloned();
                            if matches!((ta, tb), (Some(Bson::Timestamp(x)), Some(Bson::Timestamp(y))) if y > x) { Some(b) } else { Some(a) }
                        }
                        (a, b) => a.or(b),
                    }
                };
                if let Some(ct) = ct {
                    cmd.insert("$clusterTime", ct);
                }
            }
            // Read preference for mongos, and for a direct connection.
            if let Selection::Read(rp) = &op.selection {
                let topo = self.inner.topology.borrow().kind;
                if topo == TopologyType::Single && kind != ServerType::Mongos {
                    // A direct connection reads from whatever member it is.
                    cmd.insert("$readPreference", ReadPreference::new(ReadMode::PrimaryPreferred).to_document());
                } else if rp.mode != ReadMode::Primary && kind != ServerType::Standalone {
                    // OP_MSG: a non-primary preference travels with the command
                    // (it is also how a secondary knows the read is allowed).
                    cmd.insert("$readPreference", rp.to_document());
                }
            }
            // Poisoned while the reply is outstanding: if this future is
            // dropped mid-flight (a timeout), the connection — with a reply
            // still on its way — is discarded, never handed out again.
            conn.poison();
            let generation = conn.generation;
            let result = conn.command_seq(op.db, cmd, op.seqs).await;
            if !conn.is_broken() {
                conn.mark_healthy();
            }
            drop(conn);
            match result {
                Ok(reply) => {
                    self.gossip(&reply, session);
                    if let Some(s) = session {
                        let mut st = s.state.borrow_mut();
                        if st.txn == TxnState::Starting {
                            st.txn = TxnState::InProgress;
                            if kind == ServerType::Mongos {
                                st.pinned = Some(address.clone());
                            }
                        }
                    }
                    let implicit = implicit.clone();
                    if let Some(i) = &implicit {
                        // Implicit sessions go back unless a cursor needs them.
                        let keeps = reply.get_document("cursor").and_then(|c| c.get_i64("id")).is_some_and(|id| id != 0);
                        if !keeps {
                            if let Some(s) = i.state.borrow_mut().server.take() {
                                self.checkin_session(s);
                            }
                        }
                    }
                    return Ok(Ran { reply, address, implicit });
                }
                Err(mut e) => {
                    if e.is_network() {
                        if let Some(s) = session {
                            if let Some(srv) = s.state.borrow_mut().server.as_mut() {
                                srv.dirty = true;
                            }
                        }
                    }
                    self.server_failed(&address, &e, Some(generation), true);
                    // Inside a transaction, network errors and server
                    // selection failures are transient; commit outcomes
                    // after a network error are unknown.
                    if in_txn && !is_txn_end && (e.is_network() || e.kind == ErrorKind::ServerSelection) && !e.has_label("TransientTransactionError") {
                        e.labels.push("TransientTransactionError".into());
                    }
                    if cmd_name == "commitTransaction" && (e.is_network() || e.is_retryable() || matches!(e.code(), Some(50 | 64)))
                        && !e.has_label("UnknownTransactionCommitResult")
                    {
                        e.labels.push("UnknownTransactionCommitResult".into());
                    }
                    if in_txn {
                        if let Some(s) = session {
                            if e.has_label("TransientTransactionError") {
                                s.state.borrow_mut().pinned = None;
                            }
                        }
                    }
                    if retry == Retry::Write && e.is_network() && !e.has_label("RetryableWriteError") {
                        e.labels.push("RetryableWriteError".into());
                    }
                    let retryable = match retry {
                        Retry::Read => e.is_retryable(),
                        Retry::Write => e.has_label("RetryableWriteError") || (e.is_network()),
                        Retry::No => false,
                    };
                    if retryable && attempt == 1 {
                        continue;
                    }
                    if let Some(i) = &implicit {
                        if let Some(s) = i.state.borrow_mut().server.take() {
                            self.checkin_session(s);
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Run a command on `db` on the primary (or per `read`), returning the reply.
    pub async fn run_command(&self, db: &str, command: Document) -> Result<Document> {
        let op = Op { db, command, seqs: &[], selection: Selection::Write, retry: Retry::No, read_concern: false, write_concern: false, session: None };
        Ok(self.execute(op).await?.reply)
    }

    /// [`run_command`](Self::run_command) on a server picked by `read`.
    pub async fn run_command_with(&self, db: &str, command: Document, read: &ReadPreference) -> Result<Document> {
        let op = Op { db, command, seqs: &[], selection: Selection::Read(read.clone()), retry: Retry::No, read_concern: false, write_concern: false, session: None };
        Ok(self.execute(op).await?.reply)
    }

    /// `ping` the deployment (selects a server: proves it is reachable).
    pub async fn ping(&self) -> Result<()> {
        self.run_command_with("admin", Document::new().with("ping", 1), &ReadPreference::new(ReadMode::Nearest)).await.map(|_| ())
    }

    /// Database names.
    pub async fn list_database_names(&self) -> Result<Vec<String>> {
        let r = self.run_command("admin", Document::new().with("listDatabases", 1).with("nameOnly", true)).await?;
        Ok(r.get_array("databases").map(|a| a.iter().filter_map(|d| d.as_document()?.get_str("name").map(String::from)).collect()).unwrap_or_default())
    }

    /// End the pooled server sessions on the server (best effort).
    pub async fn shutdown(&self) {
        let ids: Vec<Bson> = self.inner.sessions.borrow_mut().drain(..).map(|s| Bson::Document(s.id)).collect();
        if !ids.is_empty() && self.inner.topology.borrow().supports_sessions() {
            let _ = self.run_command_with("admin", Document::new().with("endSessions", Bson::Array(ids)), &ReadPreference::new(ReadMode::PrimaryPreferred)).await;
        }
    }
}

/// Watch one server: `hello` every `heartbeatFrequencyMS` (sooner when an
/// operation asks), feeding the topology. Holds the client weakly: stops
/// when the client is gone or the server left the topology.
async fn monitor(weak: Weak<ClientInner>, address: Address) {
    let mut conn: Option<Connection> = None;
    let mut seen_check = 0u64;
    loop {
        let (spec, freq) = {
            let Some(inner) = weak.upgrade() else { return };
            if !inner.topology.borrow().servers.contains_key(&address) {
                return;
            }
            (inner.monitor_spec.clone(), inner.options.heartbeat_frequency)
        };
        let known = conn.is_some();
        let mut outcome = check(&mut conn, &address, &spec).await;
        if outcome.is_err() && known {
            // A known server failed: retry once at once (a single blip).
            outcome = check(&mut conn, &address, &spec).await;
        }
        {
            let Some(inner) = weak.upgrade() else { return };
            let client = Client { inner };
            let changes = match outcome {
                Ok((hello, rtt)) => client.inner.topology.borrow_mut().apply(ServerDescription::from_hello(address.clone(), &hello, rtt)),
                Err(e) => {
                    // A failed check: Unknown, and the pool cleared (server monitoring spec).
                    if let Some(p) = client.inner.pools.borrow().get(&address) {
                        p.clear();
                    }
                    client.inner.topology.borrow_mut().check_failed(&address, e.to_string())
                }
            };
            for a in changes.added {
                client.start_monitor(a);
            }
            for r in changes.removed {
                client.inner.pools.borrow_mut().remove(&r);
            }
        }
        // Wait for the next heartbeat, or an operation's request (at most
        // every 500 ms).
        let started = Instant::now();
        loop {
            rt_core::sleep(Duration::from_millis(50)).await;
            let Some(inner) = weak.upgrade() else { return };
            let req = inner.check_now.get();
            let unknown = inner.topology.borrow().servers.get(&address).is_none_or(|s| s.kind == ServerType::Unknown);
            let elapsed = started.elapsed();
            if elapsed >= freq || (req != seen_check && elapsed >= Duration::from_millis(500)) || (unknown && elapsed >= Duration::from_millis(500) && req != seen_check) {
                seen_check = req;
                break;
            }
        }
    }
}

async fn check(conn: &mut Option<Connection>, address: &Address, spec: &ConnectSpec) -> Result<(Document, Duration)> {
    if conn.is_none() {
        let c = Connection::open(address, spec, false).await?;
        let rtt = c.hello().rtt;
        let raw = c.hello().raw.clone();
        *conn = Some(c);
        return Ok((raw, rtt));
    }
    let c = conn.as_mut().expect("checked above");
    let started = Instant::now();
    let r = c.command("admin", Document::new().with("hello", 1).with("helloOk", true)).await;
    match r {
        Ok(d) => Ok((d, started.elapsed())),
        Err(e) => {
            *conn = None;
            Err(e)
        }
    }
}

impl Session {
    /// The client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The session id document (`{id: UUID}`), once the session was used.
    pub fn id(&self) -> Option<Document> {
        self.state.borrow().server.as_ref().map(|s| s.id.clone())
    }

    /// The latest operation time seen (causal consistency).
    pub fn operation_time(&self) -> Option<Timestamp> {
        self.state.borrow().operation_time
    }

    /// Whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        matches!(self.state.borrow().txn, TxnState::Starting | TxnState::InProgress)
    }

    /// Start a transaction (MongoDB 4.0+ replica sets, 4.2+ sharded clusters).
    pub fn start_transaction(&self, options: TransactionOptions) -> Result<()> {
        let topology = self.client.inner.topology.borrow().kind;
        if topology == TopologyType::Single
            && self.client.inner.topology.borrow().servers.values().any(|s| s.kind == ServerType::Standalone)
        {
            return Err(Error::transaction("transactions need a replica set or a sharded cluster, not a standalone server"));
        }
        let mut st = self.state.borrow_mut();
        if st.snapshot {
            return Err(Error::transaction("a snapshot session does not run transactions"));
        }
        if matches!(st.txn, TxnState::Starting | TxnState::InProgress) {
            return Err(Error::transaction("a transaction is already in progress"));
        }
        if options.read_preference.as_ref().is_some_and(|r| r.mode != ReadMode::Primary) {
            return Err(Error::transaction("transactions read from the primary"));
        }
        let srv = st.server.get_or_insert_with(|| self.client.checkout_session());
        srv.txn_number += 1;
        st.txn = TxnState::Starting;
        st.txn_options = options;
        st.pinned = None;
        Ok(())
    }

    async fn end_transaction(&self, commit: bool) -> Result<()> {
        let state = self.state.borrow().txn;
        match (state, commit) {
            (TxnState::None, _) => return Err(Error::transaction("no transaction started")),
            (TxnState::Starting, _) => {
                // Nothing reached the server.
                self.state.borrow_mut().txn = if commit { TxnState::Committed } else { TxnState::Aborted };
                return Ok(());
            }
            (TxnState::Aborted, true) => return Err(Error::transaction("cannot commit an aborted transaction")),
            (TxnState::Aborted, false) => return Err(Error::transaction("the transaction was already aborted")),
            (TxnState::Committed, false) => return Err(Error::transaction("cannot abort a committed transaction")),
            _ => {}
        }
        let name = if commit { "commitTransaction" } else { "abortTransaction" };
        let mut cmd = Document::new().with(name, 1);
        if commit {
            if let Some(t) = self.state.borrow().txn_options.max_commit_time {
                cmd.insert("maxTimeMS", t.as_millis() as i64);
            }
            if state == TxnState::Committed {
                // A retried commit asks for majority.
                let mut wc = self.state.borrow().txn_options.write_concern.clone().unwrap_or_else(|| self.client.inner.options.write_concern.clone());
                wc.w = Some(Bson::String("majority".into()));
                wc.timeout = wc.timeout.or(Some(Duration::from_secs(10)));
                cmd.insert("writeConcern", wc.to_document());
            }
        }
        let op = Op { db: "admin", command: cmd, seqs: &[], selection: Selection::Write, retry: Retry::Write, read_concern: false, write_concern: true, session: Some(self) };
        let r = self.client.execute(op).await;
        {
            let mut st = self.state.borrow_mut();
            st.txn = if commit { TxnState::Committed } else { TxnState::Aborted };
            if !commit || r.is_ok() {
                st.pinned = None;
            }
        }
        match r {
            Ok(_) => Ok(()),
            // Abort is best effort.
            Err(_) if !commit => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Commit the transaction. A commit whose outcome is unknown (network
    /// error) carries `UnknownTransactionCommitResult` and may be retried.
    pub async fn commit_transaction(&self) -> Result<()> {
        self.end_transaction(true).await
    }

    /// Abort the transaction (best effort: errors are swallowed).
    pub async fn abort_transaction(&self) -> Result<()> {
        self.end_transaction(false).await
    }

    /// Run `work` in a transaction, retrying the whole callback on
    /// `TransientTransactionError` and the commit on
    /// `UnknownTransactionCommitResult`, for up to 120 seconds.
    pub async fn with_transaction<R, F>(&self, options: TransactionOptions, mut work: F) -> Result<R>
    where
        F: AsyncFnMut(&Session) -> Result<R>,
    {
        let started = Instant::now();
        let limit = Duration::from_secs(120);
        'outer: loop {
            self.start_transaction(options.clone())?;
            let value = match work(self).await {
                Ok(v) => v,
                Err(e) => {
                    if self.in_transaction() {
                        let _ = self.abort_transaction().await;
                    }
                    if e.has_label("TransientTransactionError") && started.elapsed() < limit {
                        continue 'outer;
                    }
                    return Err(e);
                }
            };
            if !self.in_transaction() {
                // The callback committed or aborted itself.
                return Ok(value);
            }
            loop {
                match self.commit_transaction().await {
                    Ok(()) => return Ok(value),
                    Err(e) if e.has_label("UnknownTransactionCommitResult") && started.elapsed() < limit && e.code() != Some(50) => {
                        self.state.borrow_mut().txn = TxnState::Committed;
                        continue;
                    }
                    Err(e) if e.has_label("TransientTransactionError") && started.elapsed() < limit => continue 'outer,
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// End the session: abort an open transaction, return the server session.
    pub async fn end(self) {
        if self.in_transaction() {
            let _ = self.abort_transaction().await;
        }
        if let Some(s) = self.state.borrow_mut().server.take() {
            self.client.checkin_session(s);
        }
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        // A dropped explicit session's server session is simply not reused
        // (it may be mid-transaction; the server reaps it after its timeout).
    }
}

/// A cursor over the results of a `find`, `aggregate`, `listCollections`,
/// …: batches fetched with `getMore` on the same server as needed. Dropping
/// it early kills the server-side cursor.
pub struct Cursor<T> {
    pub(crate) client: Client,
    pub(crate) address: Address,
    pub(crate) db: String,
    pub(crate) collection: String,
    pub(crate) id: i64,
    pub(crate) buffer: VecDeque<Document>,
    pub(crate) batch_size: Option<i64>,
    pub(crate) max_await: Option<Duration>,
    pub(crate) session: Option<Session>,
    pub(crate) post_batch_token: Option<Bson>,
    pub(crate) _t: std::marker::PhantomData<T>,
}

impl<T: serde::de::DeserializeOwned> Cursor<T> {
    pub(crate) fn from_reply(client: &Client, ran: Ran, session: Option<&Session>, batch_size: Option<i64>) -> Result<Self> {
        let c = ran.reply.get_document("cursor").ok_or_else(|| Error::protocol("reply without a cursor"))?;
        let ns = c.get_str("ns").unwrap_or("");
        let (db, coll) = ns.split_once('.').unwrap_or((ns, ""));
        let first = c.get_array("firstBatch").cloned().unwrap_or_default();
        let id = c.get_i64("id").unwrap_or(0);
        Ok(Cursor {
            client: client.clone(),
            address: ran.address,
            db: db.to_string(),
            collection: coll.to_string(),
            id,
            buffer: first.into_iter().filter_map(|d| if let Bson::Document(d) = d { Some(d) } else { None }).collect(),
            batch_size,
            max_await: None,
            session: session.cloned().or(ran.implicit),
            post_batch_token: c.get("postBatchResumeToken").cloned(),
            _t: std::marker::PhantomData,
        })
    }

    /// Whether the server has no more batches (the buffer may still hold some).
    pub fn is_exhausted(&self) -> bool {
        self.id == 0
    }

    /// The server-side cursor id (0 once exhausted).
    pub fn id(&self) -> i64 {
        self.id
    }

    pub(crate) async fn get_more(&mut self) -> Result<()> {
        let mut cmd = Document::new().with("getMore", Bson::Int64(self.id)).with("collection", self.collection.clone());
        if let Some(b) = self.batch_size {
            cmd.insert("batchSize", b);
        }
        if let Some(m) = self.max_await {
            cmd.insert("maxTimeMS", m.as_millis() as i64);
        }
        let op = Op {
            db: &self.db,
            command: cmd,
            seqs: &[],
            selection: Selection::Address(self.address.clone()),
            retry: Retry::No,
            read_concern: false,
            write_concern: false,
            session: self.session.as_ref(),
        };
        let ran = match self.client.execute(op).await {
            Ok(r) => r,
            Err(e) => {
                if e.is_network() || e.code() == Some(43) {
                    self.id = 0;
                }
                return Err(e);
            }
        };
        let c = ran.reply.get_document("cursor").ok_or_else(|| Error::protocol("getMore reply without a cursor"))?;
        self.id = c.get_i64("id").unwrap_or(0);
        if let Some(t) = c.get("postBatchResumeToken") {
            self.post_batch_token = Some(t.clone());
        }
        for d in c.get_array("nextBatch").cloned().unwrap_or_default() {
            if let Bson::Document(d) = d {
                self.buffer.push_back(d);
            }
        }
        if self.id == 0 {
            self.release_session();
        }
        Ok(())
    }

    fn release_session(&mut self) {
        if let Some(s) = &self.session {
            let implicit = s.state.borrow().implicit;
            if implicit {
                if let Some(srv) = s.state.borrow_mut().server.take() {
                    self.client.checkin_session(srv);
                }
            }
        }
    }

    /// The next raw document, fetching a batch when needed.
    pub async fn next_document(&mut self) -> Option<Result<Document>> {
        loop {
            if let Some(d) = self.buffer.pop_front() {
                return Some(Ok(d));
            }
            if self.id == 0 {
                self.release_session();
                return None;
            }
            if let Err(e) = self.get_more().await {
                return Some(Err(e));
            }
        }
    }

    /// The next item.
    pub async fn next(&mut self) -> Option<Result<T>> {
        Some(self.next_document().await?.and_then(|d| crate::bson::from_document(&d).map_err(Error::from)))
    }

    /// Everything left.
    pub async fn collect(mut self) -> Result<Vec<T>> {
        let mut out = Vec::new();
        while let Some(x) = self.next().await {
            out.push(x?);
        }
        Ok(out)
    }

    /// Kill the server-side cursor now.
    pub async fn close(mut self) -> Result<()> {
        self.kill().await
    }

    async fn kill(&mut self) -> Result<()> {
        if self.id != 0 {
            let id = std::mem::replace(&mut self.id, 0);
            let cmd = Document::new().with("killCursors", self.collection.clone()).with("cursors", vec![Bson::Int64(id)]);
            let op = Op {
                db: &self.db,
                command: cmd,
                seqs: &[],
                selection: Selection::Address(self.address.clone()),
                retry: Retry::No,
                read_concern: false,
                write_concern: false,
                session: self.session.as_ref(),
            };
            self.client.execute(op).await?;
        }
        self.release_session();
        Ok(())
    }
}

impl<T> Drop for Cursor<T> {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        // Kill the server-side cursor in the background.
        let (client, address, db, coll, id, session) =
            (self.client.clone(), self.address.clone(), self.db.clone(), self.collection.clone(), self.id, self.session.clone());
        rt_core::spawn(async move {
            let cmd = Document::new().with("killCursors", coll).with("cursors", vec![Bson::Int64(id)]);
            let op = Op { db: &db, command: cmd, seqs: &[], selection: Selection::Address(address), retry: Retry::No, read_concern: false, write_concern: false, session: session.as_ref() };
            let _ = client.execute(op).await;
            if let Some(s) = session {
                if s.state.borrow().implicit {
                    if let Some(srv) = s.state.borrow_mut().server.take() {
                        client.checkin_session(srv);
                    }
                }
            }
        });
    }
}
