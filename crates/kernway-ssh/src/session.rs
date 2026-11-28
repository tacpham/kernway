//! One login, many channels: [`SshSession`] hands a connection to a task on the
//! current shard and lets any number of consoles, SFTP sessions, commands and
//! tunnels share it at once — as a client like Bitvise runs a terminal, a file
//! browser and port forwards over one connection.
//!
//! The task waits on the network and on commands from the handles together (the
//! transport's reads are cancel-safe). Each channel delivers its events to its own
//! mailbox; a channel whose reader falls behind keeps its data (and so stops the
//! server sending more on it) without holding up the others.

use crate::client::{ExecOutput, SshClient};
use crate::console::{Console, PtyConfig};
use crate::hostkey::PublicKey;
use crate::transport::Negotiated;
use crate::SshError;
use rt_core::{mailbox, MailboxReceiver, MailboxSender, TrySendError};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

/// What a channel reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEvent {
    /// Bytes from the other end (a program's stdout, a tunnel's data).
    Data(Vec<u8>),
    /// Bytes on standard error (session channels without a terminal).
    Stderr(Vec<u8>),
    /// The other end will send no more.
    Eof,
    /// The program ended: its exit status, or the signal that ended it.
    Exit {
        /// The exit status, when it exited normally.
        status: Option<u32>,
        /// The signal name, when one ended it.
        signal: Option<String>,
    },
    /// The channel is over; `Some` says why when the connection failed.
    Closed(Option<String>),
}

/// What a session channel starts.
#[derive(Debug, Clone)]
pub(crate) enum Start {
    Shell,
    Exec(String),
    Subsystem(String),
}

enum Open {
    Session { pty: Option<PtyConfig>, start: Start },
    DirectTcpip { host: String, port: u16, orig: String, orig_port: u16 },
}

enum Command {
    Open { what: Open, events: MailboxSender<ChannelEvent>, reply: MailboxSender<Result<u32, SshError>> },
    Data(u32, Vec<u8>),
    Eof(u32),
    Close(u32),
    Resize(u32, u32, u32),
    Signal(u32, String),
    RemoteForward { host: String, port: u16, accept: MailboxSender<Channel>, commands: MailboxSender<Command>, reply: MailboxSender<Result<u16, SshError>> },
    Disconnect,
}

/// Facts about the connection, fixed at login.
#[derive(Debug)]
struct Info {
    server_version: String,
    host_key: PublicKey,
    negotiated: Negotiated,
    closed: std::sync::atomic::AtomicBool,
}

/// A logged-in connection shared by many channels. Cheap to clone; the connection
/// ends when the last handle (session, channel, console, SFTP, forward) is gone, or
/// on [`disconnect`](Self::disconnect).
#[derive(Clone)]
pub struct SshSession {
    commands: MailboxSender<Command>,
    info: Arc<Info>,
}

impl std::fmt::Debug for SshSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSession").field("server", &self.info.server_version).finish_non_exhaustive()
    }
}

fn ended() -> SshError {
    SshError::Channel("the SSH session has ended".into())
}

fn send<T>(tx: &MailboxSender<T>, msg: T) -> Result<(), SshError> {
    tx.try_send(msg).map_err(|e| match e {
        TrySendError::Full(_) => SshError::Channel("the SSH session is not keeping up".into()),
        TrySendError::Closed(_) => ended(),
    })
}

/// Queue `msg`, waiting (briefly, repeatedly) while the queue is full — the
/// backpressure of a bulk writer.
async fn send_waiting<T>(tx: &MailboxSender<T>, mut msg: T) -> Result<(), SshError> {
    loop {
        match tx.try_send(msg) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(m)) => {
                msg = m;
                rt_core::sleep(Duration::from_millis(1)).await;
            }
            Err(TrySendError::Closed(_)) => return Err(ended()),
        }
    }
}

async fn reply<T>(rx: &mut MailboxReceiver<Result<T, SshError>>) -> Result<T, SshError> {
    rx.recv().await.unwrap_or_else(|| Err(ended()))
}

impl SshClient {
    /// Share this connection: a task on the current shard takes it over, and the
    /// returned session opens consoles, SFTP, commands and tunnels on it — all at once.
    pub fn into_session(self) -> SshSession {
        let info = Arc::new(Info {
            server_version: self.server_version(),
            host_key: self.host_key().clone(),
            negotiated: self.negotiated().clone(),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let (tx, rx) = mailbox(4096);
        rt_core::spawn(drive(self, rx, info.clone()));
        SshSession { commands: tx, info }
    }
}

impl SshSession {
    /// The server's version string.
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.info.server_version
    }

    /// The server's host key (checked at login).
    #[must_use]
    pub fn host_key(&self) -> &PublicKey {
        &self.info.host_key
    }

    /// The algorithms the login agreed on.
    #[must_use]
    pub fn negotiated(&self) -> &Negotiated {
        &self.info.negotiated
    }

    /// Whether the connection is gone.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.info.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn open(&self, what: Open) -> Result<Channel, SshError> {
        let (events_tx, events_rx) = mailbox(4096);
        let (reply_tx, mut reply_rx) = mailbox(1);
        send(&self.commands, Command::Open { what, events: events_tx, reply: reply_tx })?;
        let id = reply(&mut reply_rx).await?;
        Ok(Channel { writer: ChannelWriter { id, commands: self.commands.clone() }, events: events_rx })
    }

    pub(crate) async fn open_session_channel(&self, pty: Option<PtyConfig>, start: Start) -> Result<Channel, SshError> {
        self.open(Open::Session { pty, start }).await
    }

    /// An interactive login shell on a terminal.
    pub async fn console(&self, pty: PtyConfig) -> Result<Console, SshError> {
        Ok(Console::over(self.open_session_channel(Some(pty), Start::Shell).await?))
    }

    /// A command as a console (see [`SshClient::console_exec`]).
    pub async fn console_exec(&self, command: &str, pty: Option<PtyConfig>) -> Result<Console, SshError> {
        Ok(Console::over(self.open_session_channel(pty, Start::Exec(command.to_owned())).await?))
    }

    /// Run a command and collect its output (other channels carry on meanwhile).
    pub async fn exec(&self, command: &str) -> Result<ExecOutput, SshError> {
        self.exec_with_input(command, &[]).await
    }

    /// [`exec`](Self::exec), writing `input` to the command's stdin first.
    pub async fn exec_with_input(&self, command: &str, input: &[u8]) -> Result<ExecOutput, SshError> {
        let mut ch = self.open_session_channel(None, Start::Exec(command.to_owned())).await?;
        for part in input.chunks(64 * 1024) {
            ch.writer.write_waiting(part).await?;
        }
        ch.writer.send_eof()?;
        let mut out = ExecOutput::default();
        while let Some(e) = ch.next().await {
            match e {
                ChannelEvent::Data(d) => out.stdout.extend(d),
                ChannelEvent::Stderr(d) => out.stderr.extend(d),
                ChannelEvent::Eof => {}
                ChannelEvent::Exit { status, signal } => {
                    out.exit_status = status;
                    out.exit_signal = signal;
                }
                ChannelEvent::Closed(None) => break,
                ChannelEvent::Closed(Some(why)) => return Err(SshError::Channel(why)),
            }
        }
        Ok(out)
    }

    /// An SFTP session on this connection.
    pub async fn sftp(&self) -> Result<crate::sftp::Sftp, SshError> {
        let ch = self.open_session_channel(None, Start::Subsystem("sftp".into())).await?;
        crate::sftp::Sftp::over_channel(ch, self.info.server_version.contains("OpenSSH")).await
    }

    /// A raw channel to `host:port` as the server sees it (`direct-tcpip`) — what
    /// a local forward carries.
    pub async fn connect_tcp(&self, host: &str, port: u16) -> Result<Channel, SshError> {
        self.open_direct(host, port, "127.0.0.1", 0).await
    }

    pub(crate) async fn open_direct(&self, host: &str, port: u16, orig: &str, orig_port: u16) -> Result<Channel, SshError> {
        self.open(Open::DirectTcpip { host: host.to_owned(), port, orig: orig.to_owned(), orig_port }).await
    }

    /// A local forward (`ssh -L`): connections to `listen` here are carried to
    /// `host:port` as the server sees it. Returns once listening.
    pub async fn forward_local(&self, listen: SocketAddr, host: &str, port: u16) -> Result<Forward, SshError> {
        crate::forward::local(self.clone(), listen, crate::forward::Target::Fixed(host.to_owned(), port)).await
    }

    /// A dynamic forward (`ssh -D`): a SOCKS5 proxy on `listen` whose connections go
    /// out from the server.
    pub async fn forward_dynamic(&self, listen: SocketAddr) -> Result<Forward, SshError> {
        crate::forward::local(self.clone(), listen, crate::forward::Target::Socks).await
    }

    /// A remote forward (`ssh -R`): the server listens on `bind_host:bind_port` (0
    /// picks a port) and each connection there is carried to `to_host:to_port` here.
    pub async fn forward_remote(&self, bind_host: &str, bind_port: u16, to_host: &str, to_port: u16) -> Result<RemoteForward, SshError> {
        let (accept_tx, accept_rx) = mailbox(256);
        let (reply_tx, mut reply_rx) = mailbox(1);
        send(&self.commands, Command::RemoteForward { host: bind_host.to_owned(), port: bind_port, accept: accept_tx, commands: self.commands.clone(), reply: reply_tx })?;
        let port = reply(&mut reply_rx).await?;
        Ok(crate::forward::remote(port, accept_rx, to_host.to_owned(), to_port))
    }

    /// End the connection now (every channel on it closes).
    pub fn disconnect(&self) {
        let _ = self.commands.try_send(Command::Disconnect);
    }
}

/// The sending half of a channel. Cheap to clone.
#[derive(Clone)]
pub struct ChannelWriter {
    id: u32,
    commands: MailboxSender<Command>,
}

impl ChannelWriter {
    /// Queue bytes for the other end (fails at once when the session is overloaded;
    /// [`write_waiting`](Self::write_waiting) waits instead).
    pub fn write(&self, data: impl Into<Vec<u8>>) -> Result<(), SshError> {
        send(&self.commands, Command::Data(self.id, data.into()))
    }

    /// Queue bytes, waiting while the session's queue is full.
    pub async fn write_waiting(&self, data: &[u8]) -> Result<(), SshError> {
        send_waiting(&self.commands, Command::Data(self.id, data.to_vec())).await
    }

    /// No more data from this side.
    pub fn send_eof(&self) -> Result<(), SshError> {
        send(&self.commands, Command::Eof(self.id))
    }

    /// Close the channel (the other end's close arrives as [`ChannelEvent::Closed`]).
    pub fn close(&self) -> Result<(), SshError> {
        send(&self.commands, Command::Close(self.id))
    }

    /// A terminal's new size.
    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), SshError> {
        send(&self.commands, Command::Resize(self.id, cols, rows))
    }

    /// Deliver a signal to the program.
    pub fn signal(&self, name: &str) -> Result<(), SshError> {
        send(&self.commands, Command::Signal(self.id, name.to_owned()))
    }
}

/// A channel on a shared session: write through it, read its events.
pub struct Channel {
    writer: ChannelWriter,
    events: MailboxReceiver<ChannelEvent>,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Channel({})", self.writer.id)
    }
}

impl Channel {
    /// The sending half (to write from another task).
    #[must_use]
    pub fn writer(&self) -> ChannelWriter {
        self.writer.clone()
    }

    /// Split into the sending half and the receiving half.
    #[must_use]
    pub fn split(self) -> (ChannelWriter, MailboxReceiver<ChannelEvent>) {
        (self.writer, self.events)
    }

    /// The next event; `None` once [`ChannelEvent::Closed`] has been delivered.
    pub async fn next(&mut self) -> Option<ChannelEvent> {
        self.events.recv().await
    }

    pub(crate) fn writer_ref(&self) -> &ChannelWriter {
        &self.writer
    }
}

pub use crate::forward::{Forward, RemoteForward};

/// Where a channel's events go, and what has not fitted yet.
struct Route {
    events: MailboxSender<ChannelEvent>,
    held: VecDeque<ChannelEvent>,
    eof_sent: bool,
    finished: bool,
}

enum Woke {
    Network(Result<(), SshError>),
    Command(Option<Command>),
    Retry,
}

async fn drive(mut ssh: SshClient, mut commands: MailboxReceiver<Command>, info: Arc<Info>) {
    let mut routes: HashMap<u32, Route> = HashMap::new();
    // Remote forwards by bound port: who takes the connections, and the command
    // queue their channels write to. (A remote forward keeps the session up until
    // `disconnect`.)
    let mut forwards: Forwards = HashMap::new();
    let result: Result<(), SshError> = async {
        loop {
            while let Some(p) = ssh.t.recv_buffered().await? {
                ssh.dispatch(p).await?;
            }
            // Connections to a remote forward: a channel each, to its forward's owner.
            while let Some((id, _bound, bport, _orig, _oport)) = ssh.incoming.pop_front() {
                let (events_tx, events_rx) = mailbox(4096);
                routes.insert(id, Route { events: events_tx, held: VecDeque::new(), eof_sent: false, finished: false });
                let handed = match forwards.get(&bport) {
                    Some((accept, cmds)) => accept.try_send(Channel { writer: ChannelWriter { id, commands: cmds.clone() }, events: events_rx }).is_ok(),
                    None => false,
                };
                if !handed {
                    ssh.send_close(id).await?;
                }
            }
            let mut behind = false;
            for (&id, route) in &mut routes {
                if route.finished {
                    continue;
                }
                if route.held.is_empty() {
                    let (out, err, ended) = ssh.take_output(id).await?;
                    if !out.is_empty() {
                        route.held.push_back(ChannelEvent::Data(out));
                    }
                    if !err.is_empty() {
                        route.held.push_back(ChannelEvent::Stderr(err));
                    }
                    if !route.eof_sent && ssh.chan(id)?.eof {
                        route.eof_sent = true;
                        route.held.push_back(ChannelEvent::Eof);
                    }
                    if let Some((status, signal)) = ended {
                        if status.is_some() || signal.is_some() {
                            route.held.push_back(ChannelEvent::Exit { status, signal });
                        }
                        route.held.push_back(ChannelEvent::Closed(None));
                    }
                }
                while let Some(e) = route.held.pop_front() {
                    let last = matches!(e, ChannelEvent::Closed(_));
                    match route.events.try_send(e) {
                        Ok(()) if last => route.finished = true,
                        Ok(()) => {}
                        Err(TrySendError::Full(e)) => {
                            route.held.push_front(e);
                            behind = true;
                            break;
                        }
                        // Nobody listens any more: close it.
                        Err(TrySendError::Closed(_)) => {
                            route.held.clear();
                            route.finished = !ssh.chan(id)?.closed;
                            ssh.send_close(id).await?;
                            if ssh.chan(id)?.closed {
                                route.finished = true;
                            }
                            break;
                        }
                    }
                }
            }
            let done: Vec<u32> = routes.iter().filter(|(&id, r)| r.finished && ssh.chan(id).map_or(true, |c| c.closed)).map(|(&id, _)| id).collect();
            for id in done {
                routes.remove(&id);
                ssh.forget(id);
            }

            let woke = {
                let mut net = pin!(ssh.t.fill());
                let mut cmd = pin!(commands.recv());
                let mut pause = pin!(rt_core::sleep(Duration::from_millis(10)));
                std::future::poll_fn(|cx| {
                    if let Poll::Ready(r) = net.as_mut().poll(cx) {
                        return Poll::Ready(Woke::Network(r));
                    }
                    if let Poll::Ready(c) = cmd.as_mut().poll(cx) {
                        return Poll::Ready(Woke::Command(c));
                    }
                    if behind && pause.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Woke::Retry);
                    }
                    Poll::Pending
                })
                .await
            };
            match woke {
                Woke::Network(r) => r?,
                Woke::Retry => {}
                Woke::Command(None) | Woke::Command(Some(Command::Disconnect)) => {
                    let _ = ssh.send_disconnect().await;
                    return Ok(());
                }
                Woke::Command(Some(c)) => handle(&mut ssh, &mut routes, &mut forwards, c).await?,
            }
        }
    }
    .await;
    info.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    let why = result.err().map(|e| e.to_string());
    for route in routes.values() {
        let _ = route.events.try_send(ChannelEvent::Closed(Some(why.clone().unwrap_or_else(|| "the SSH session has ended".into()))));
    }
}

type Forwards = HashMap<u32, (MailboxSender<Channel>, MailboxSender<Command>)>;

async fn handle(ssh: &mut SshClient, routes: &mut HashMap<u32, Route>, forwards: &mut Forwards, c: Command) -> Result<(), SshError> {
    // Nothing may follow our CLOSE on a channel (the server frees it once both sides
    // closed, and would take any later message as a protocol error).
    let live = |ssh: &mut SshClient, routes: &HashMap<u32, Route>, id: u32| {
        routes.get(&id).is_some_and(|r| !r.finished) && ssh.chan(id).is_ok_and(|c| !c.closed && !c.close_sent)
    };
    match c {
        Command::Open { what, events, reply } => {
            let opened = open(ssh, what).await;
            match opened {
                Ok(id) => {
                    routes.insert(id, Route { events, held: VecDeque::new(), eof_sent: false, finished: false });
                    if reply.try_send(Ok(id)).is_err() {
                        ssh.send_close(id).await?;
                    }
                }
                // A refused channel is the caller's error; a broken connection is everyone's.
                Err(e @ SshError::Channel(_)) => {
                    let _ = reply.try_send(Err(e));
                }
                Err(e) => {
                    let _ = reply.try_send(Err(SshError::Channel(e.to_string())));
                    return Err(e);
                }
            }
        }
        Command::Data(id, d) if live(ssh, routes, id) => ssh.write(id, &d).await?,
        Command::Eof(id) if live(ssh, routes, id) => ssh.send_eof(id).await?,
        Command::Close(id) if routes.get(&id).is_some_and(|r| !r.finished) => ssh.send_close(id).await?,
        Command::Resize(id, cols, rows) if live(ssh, routes, id) => {
            ssh.request_no_reply(id, "window-change", |w| {
                w.u32(cols).u32(rows).u32(0).u32(0);
            })
            .await?;
        }
        Command::Signal(id, name) if live(ssh, routes, id) => {
            ssh.request_no_reply(id, "signal", |w| {
                w.string(&name);
            })
            .await?;
        }
        Command::RemoteForward { host, port, accept, commands, reply } => {
            ssh.accept_forwarded = true;
            match ssh
                .global_request("tcpip-forward", |w| {
                    w.string(&host).u32(u32::from(port));
                })
                .await?
            {
                Some(payload) => {
                    let bound = if port == 0 { crate::wire::Reader::new(&payload).u32().unwrap_or(0) } else { u32::from(port) };
                    forwards.insert(bound, (accept, commands));
                    let _ = reply.try_send(Ok(u16::try_from(bound).unwrap_or(0)));
                }
                None => {
                    let _ = reply.try_send(Err(SshError::Channel(format!("the server refused to listen on {host}:{port} (AllowTcpForwarding?)"))));
                }
            }
        }
        // For a channel already closed: nothing to do.
        _ => {}
    }
    Ok(())
}

async fn open(ssh: &mut SshClient, what: Open) -> Result<u32, SshError> {
    match what {
        Open::DirectTcpip { host, port, orig, orig_port } => ssh.open_direct_tcpip(&host, port, &orig, orig_port).await,
        Open::Session { pty, start } => {
            let id = ssh.open_session().await?;
            if let Some(p) = &pty {
                let ok = ssh
                    .request(id, "pty-req", |w| {
                        w.string(&p.term).u32(p.cols).u32(p.rows).u32(0).u32(0).string([0u8]);
                    })
                    .await?;
                if !ok {
                    ssh.close(id).await?;
                    return Err(SshError::Channel("the server refused a terminal (pty-req)".into()));
                }
            }
            let ok = match &start {
                Start::Shell => ssh.request(id, "shell", |_| {}).await?,
                Start::Exec(c) => {
                    ssh.request(id, "exec", |w| {
                        w.string(c);
                    })
                    .await?
                }
                Start::Subsystem(s) => {
                    ssh.request(id, "subsystem", |w| {
                        w.string(s);
                    })
                    .await?
                }
            };
            if !ok {
                ssh.close(id).await?;
                return Err(SshError::Channel(format!("the server refused to start {start:?}")));
            }
            Ok(id)
        }
    }
}
