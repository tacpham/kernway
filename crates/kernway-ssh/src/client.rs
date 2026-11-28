//! The client: connection setup, user authentication (RFC 4252, RFC 4256) and the
//! connection protocol's session channels (RFC 4254) — flow control, `exec`, and the
//! `sftp` subsystem the SFTP client runs over.

use crate::hostkey::{HostKeyPolicy, PublicKey};
use crate::keys::PrivateKey;
use crate::cipher::CipherKind;
use crate::transport::{self, Negotiated, Offer, Transport, MSG_SERVICE_ACCEPT, MSG_SERVICE_REQUEST};
use crate::wire::{Reader, Writer};
use crate::SshError;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const MSG_USERAUTH_REQUEST: u8 = 50;
const MSG_USERAUTH_FAILURE: u8 = 51;
const MSG_USERAUTH_SUCCESS: u8 = 52;
const MSG_USERAUTH_BANNER: u8 = 53;
/// PK_OK / PASSWD_CHANGEREQ / INFO_REQUEST, depending on the method.
const MSG_USERAUTH_60: u8 = 60;
const MSG_USERAUTH_INFO_RESPONSE: u8 = 61;
const MSG_GLOBAL_REQUEST: u8 = 80;
const MSG_REQUEST_FAILURE: u8 = 82;
const MSG_CHANNEL_OPEN: u8 = 90;
const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const MSG_CHANNEL_DATA: u8 = 94;
const MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
const MSG_CHANNEL_EOF: u8 = 96;
const MSG_CHANNEL_CLOSE: u8 = 97;
const MSG_CHANNEL_REQUEST: u8 = 98;
const MSG_CHANNEL_SUCCESS: u8 = 99;
const MSG_CHANNEL_FAILURE: u8 = 100;

/// How much the server may send on a channel before we acknowledge it.
const WINDOW: u32 = 2 * 1024 * 1024;
/// The largest data packet we accept.
const MAX_PACKET: u32 = 32 * 1024;

/// One way to prove who the user is, tried in the order given.
#[derive(Clone)]
pub enum Auth {
    /// A password: the `password` method, or `keyboard-interactive` answering every
    /// prompt with it (servers that log in through PAM offer only that).
    Password(String),
    /// A private key (`publickey`).
    Key(PrivateKey),
    /// `keyboard-interactive` with these answers, one per prompt in order (a
    /// one-time code after the password, for example).
    KeyboardInteractive(Vec<String>),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Password(_) => f.write_str("Password(…)"),
            Auth::Key(k) => write!(f, "Key({k:?})"),
            Auth::KeyboardInteractive(a) => write!(f, "KeyboardInteractive({} answers)", a.len()),
        }
    }
}

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct SshConfig {
    host: String,
    port: u16,
    user: String,
    auth: Vec<Auth>,
    host_key: HostKeyPolicy,
    connect_timeout: Duration,
    rekey_bytes: u64,
    offer: Offer,
}

/// Keep the supported names from `wanted`, in its order; an empty result is an error.
fn subset(kind: &str, wanted: &[&str], supported: &[&'static str]) -> Result<Vec<&'static str>, SshError> {
    let out: Vec<&'static str> = wanted.iter().filter_map(|w| supported.iter().copied().find(|s| s == w)).collect();
    if out.is_empty() {
        return Err(SshError::Protocol(format!("none of the {kind} algorithms {wanted:?} is supported (supported: {})", supported.join(", "))));
    }
    Ok(out)
}

impl SshConfig {
    /// Connect to `host` (port 22) as `user`. Add at least one [`auth`](Self::auth)
    /// and choose a [`host_key`](Self::host_key) policy — the default checks
    /// `~/.ssh/known_hosts`.
    pub fn new(host: impl Into<String>, user: impl Into<String>) -> Self {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth: Vec::new(),
            host_key: HostKeyPolicy::KnownHosts(std::path::Path::new(&home).join(".ssh").join("known_hosts")),
            connect_timeout: Duration::from_secs(15),
            rekey_bytes: transport::DEFAULT_REKEY_BYTES,
            offer: Offer::default(),
        }
    }

    /// Offer only these key exchange methods, in this order (from `curve25519-sha256`,
    /// `curve25519-sha256@libssh.org`, `ecdh-sha2-nistp256`, `ecdh-sha2-nistp384`).
    pub fn kex_algorithms(mut self, names: &[&str]) -> Result<Self, SshError> {
        self.offer.kex = subset("key exchange", names, &transport::KEX)?;
        Ok(self)
    }

    /// Accept only these host key algorithms, in this order (from `ssh-ed25519`,
    /// `ecdsa-sha2-nistp256`, `ecdsa-sha2-nistp384`, `rsa-sha2-512`, `rsa-sha2-256`).
    pub fn host_key_algorithms(mut self, names: &[&str]) -> Result<Self, SshError> {
        self.offer.host_key = subset("host key", names, &transport::HOST_KEY_ALGS)?;
        Ok(self)
    }

    /// Offer only these ciphers, in this order (from `chacha20-poly1305@openssh.com`,
    /// `aes256-gcm@openssh.com`, `aes128-gcm@openssh.com`).
    pub fn ciphers(mut self, names: &[&str]) -> Result<Self, SshError> {
        let all: Vec<&'static str> = CipherKind::ALL.iter().map(|c| c.name()).collect();
        self.offer.ciphers = subset("cipher", names, &all)?;
        Ok(self)
    }

    /// The port.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Add an authentication method (tried in the order added).
    #[must_use]
    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth.push(auth);
        self
    }

    /// How to check the server's host key.
    #[must_use]
    pub fn host_key(mut self, policy: HostKeyPolicy) -> Self {
        self.host_key = policy;
        self
    }

    /// The limit on connecting, the key exchange and authentication together.
    #[must_use]
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = t;
        self
    }

    /// Start a new key exchange after this many bytes in either direction (default
    /// 1 GiB). The server may rekey earlier on its own; both are handled.
    #[must_use]
    pub fn rekey_after_bytes(mut self, bytes: u64) -> Self {
        self.rekey_bytes = bytes.max(16 * 1024);
        self
    }
}

/// The result of running a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// What the command wrote to its standard output.
    pub stdout: Vec<u8>,
    /// What it wrote to its standard error.
    pub stderr: Vec<u8>,
    /// Its exit status, when it exited normally.
    pub exit_status: Option<u32>,
    /// The signal that ended it (`TERM`, `KILL` …), when one did.
    pub exit_signal: Option<String>,
}

impl ExecOutput {
    /// Standard output as UTF-8 (lossy).
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

#[derive(Default)]
struct Channel {
    remote: u32,
    open: Option<Result<(), String>>,
    remote_window: u32,
    remote_max: u32,
    /// What we still allow the server to send before adjusting.
    local_window: u32,
    data: VecDeque<u8>,
    stderr: Vec<u8>,
    eof: bool,
    closed: bool,
    close_sent: bool,
    replies: VecDeque<bool>,
    exit_status: Option<u32>,
    exit_signal: Option<String>,
}

/// An authenticated SSH connection.
pub struct SshClient {
    t: Transport,
    channels: HashMap<u32, Channel>,
    next_id: u32,
    banner: Option<String>,
}

impl std::fmt::Debug for SshClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshClient").field("server", &self.t.server_version()).field("negotiated", &self.t.negotiated).finish_non_exhaustive()
    }
}

impl SshClient {
    /// Connect, run the key exchange, check the host key and log in.
    pub async fn connect(config: SshConfig) -> Result<SshClient, SshError> {
        let limit = config.connect_timeout;
        match rt_core::timeout(limit, Self::connect_inner(config)).await {
            Ok(r) => r,
            Err(_) => Err(SshError::Timeout),
        }
    }

    async fn connect_inner(config: SshConfig) -> Result<SshClient, SshError> {
        let known_hosts = match &config.host_key {
            HostKeyPolicy::KnownHosts(path) => match kernway_wire::offload::read_file(path).await {
                Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
                Err(e) => return Err(SshError::HostKey(format!("cannot read {}: {e}", path.display()))),
            },
            HostKeyPolicy::KnownHostsText(t) => Some(t.clone()),
            _ => None,
        };
        let sock = kernway_wire::offload::connect_tcp(&config.host, config.port).await?;
        sock.set_nodelay(true).ok();
        let t = Transport::start(sock, &config.host, config.port, config.host_key.clone(), known_hosts, config.rekey_bytes, config.offer.clone()).await?;
        let mut c = SshClient { t, channels: HashMap::new(), next_id: 0, banner: None };
        c.authenticate(&config.user, &config.auth).await?;
        Ok(c)
    }

    /// The server's host key (already checked against the policy).
    #[must_use]
    pub fn host_key(&self) -> &PublicKey {
        self.t.host_key.as_ref().expect("set by the key exchange")
    }

    /// The server's version string (`SSH-2.0-OpenSSH_9.9`).
    #[must_use]
    pub fn server_version(&self) -> String {
        self.t.server_version()
    }

    /// The algorithms the last key exchange agreed on.
    #[must_use]
    pub fn negotiated(&self) -> &Negotiated {
        self.t.negotiated.as_ref().expect("set by the key exchange")
    }

    /// How many key exchanges ran after the first one (by either side).
    #[must_use]
    pub fn rekeys(&self) -> u32 {
        self.t.rekeys
    }

    /// The banner the server showed before login, if any.
    #[must_use]
    pub fn banner(&self) -> Option<&str> {
        self.banner.as_deref()
    }

    // ── authentication ───────────────────────────────────────────────────

    async fn authenticate(&mut self, user: &str, auths: &[Auth]) -> Result<(), SshError> {
        let mut w = Writer::msg(MSG_SERVICE_REQUEST);
        w.string("ssh-userauth");
        self.t.send(&w.into_inner()).await?;
        let p = self.auth_recv().await?;
        if p[0] != MSG_SERVICE_ACCEPT {
            return Err(SshError::Protocol(format!("the server refused the user-auth service ({})", p[0])));
        }
        // "none" tells which methods the server takes (or lets the user straight in).
        let mut allowed = match self.auth_request(user, "none", |_| {}).await? {
            Ok(()) => return Ok(()),
            Err(methods) => methods,
        };
        for auth in auths {
            let outcome = match auth {
                Auth::Key(key) if allowed.iter().any(|m| m == "publickey") => self.auth_key(user, key).await?,
                Auth::Password(pw) => {
                    let mut r = Err(allowed.clone());
                    if allowed.iter().any(|m| m == "password") {
                        r = self.auth_password(user, pw).await?;
                    }
                    if r.is_err() && r.as_ref().err().is_some_and(|m: &Vec<String>| m.iter().any(|x| x == "keyboard-interactive")) {
                        r = self.auth_keyboard(user, &mut std::iter::repeat(pw.clone())).await?;
                    }
                    r
                }
                Auth::KeyboardInteractive(answers) if allowed.iter().any(|m| m == "keyboard-interactive") => {
                    self.auth_keyboard(user, &mut answers.clone().into_iter()).await?
                }
                _ => continue,
            };
            match outcome {
                Ok(()) => return Ok(()),
                Err(methods) => allowed = methods,
            }
        }
        Err(SshError::Auth { allowed })
    }

    /// The next authentication-layer message (banners are recorded and skipped).
    async fn auth_recv(&mut self) -> Result<Vec<u8>, SshError> {
        loop {
            let p = self.t.recv().await?;
            if p[0] == MSG_USERAUTH_BANNER {
                self.banner = Some(Reader::new(&p[1..]).utf8()?);
                continue;
            }
            if p[0] == MSG_GLOBAL_REQUEST {
                continue;
            }
            return Ok(p);
        }
    }

    /// Send a USERAUTH_REQUEST for `method` (with `fields` appended) and read the
    /// verdict: `Ok(Ok(()))` logged in, `Ok(Err(methods))` refused.
    async fn auth_request(&mut self, user: &str, method: &str, fields: impl FnOnce(&mut Writer)) -> Result<Result<(), Vec<String>>, SshError> {
        let mut w = Writer::msg(MSG_USERAUTH_REQUEST);
        w.string(user).string("ssh-connection").string(method);
        fields(&mut w);
        self.t.send(&w.into_inner()).await?;
        let p = self.auth_recv().await?;
        auth_verdict(&p)
    }

    async fn auth_password(&mut self, user: &str, pw: &str) -> Result<Result<(), Vec<String>>, SshError> {
        self.auth_request(user, "password", |w| {
            w.bool(false).string(pw);
        })
        .await
    }

    async fn auth_key(&mut self, user: &str, key: &PrivateKey) -> Result<Result<(), Vec<String>>, SshError> {
        let alg = key.algorithm(self.t.server_sig_algs.as_deref());
        let blob = key.public_key().to_blob();
        // RFC 4252 §7: sign the session id and the request itself.
        let mut data = Writer::new();
        data.string(self.t.session_id()).u8(MSG_USERAUTH_REQUEST).string(user).string("ssh-connection").string("publickey").bool(true).string(alg).string(&blob);
        let sig = key.sign(alg, &data.into_inner())?;
        self.auth_request(user, "publickey", |w| {
            w.bool(true).string(alg).string(&blob).string(&sig);
        })
        .await
    }

    async fn auth_keyboard(&mut self, user: &str, answers: &mut dyn Iterator<Item = String>) -> Result<Result<(), Vec<String>>, SshError> {
        let mut w = Writer::msg(MSG_USERAUTH_REQUEST);
        w.string(user).string("ssh-connection").string("keyboard-interactive").string("").string("");
        self.t.send(&w.into_inner()).await?;
        for _ in 0..16 {
            let p = self.auth_recv().await?;
            if p[0] != MSG_USERAUTH_60 {
                return auth_verdict(&p);
            }
            // INFO_REQUEST: name, instruction, language, prompts.
            let mut r = Reader::new(&p[1..]);
            r.string()?;
            r.string()?;
            r.string()?;
            let n = r.u32()?;
            let mut reply = Writer::msg(MSG_USERAUTH_INFO_RESPONSE);
            reply.u32(n);
            for _ in 0..n {
                r.string()?;
                r.bool()?;
                reply.string(answers.next().unwrap_or_default());
            }
            self.t.send(&reply.into_inner()).await?;
        }
        Err(SshError::Protocol("keyboard-interactive went on for too many rounds".into()))
    }

    // ── channels ─────────────────────────────────────────────────────────

    /// Read one message and apply it to the channels.
    async fn pump(&mut self) -> Result<(), SshError> {
        let p = self.t.recv().await?;
        let mut r = Reader::new(&p[1..]);
        match p[0] {
            MSG_GLOBAL_REQUEST => {
                r.string()?;
                if r.bool()? {
                    self.t.send(&[MSG_REQUEST_FAILURE]).await?;
                }
            }
            MSG_CHANNEL_OPEN => {
                // The server opening a channel to us (forwarding): refused.
                r.string()?;
                let theirs = r.u32()?;
                let mut w = Writer::msg(MSG_CHANNEL_OPEN_FAILURE);
                w.u32(theirs).u32(1).string("not accepted by this client").string("");
                self.t.send(&w.into_inner()).await?;
            }
            MSG_CHANNEL_OPEN_CONFIRMATION => {
                let c = self.chan(r.u32()?)?;
                c.remote = r.u32()?;
                c.remote_window = r.u32()?;
                c.remote_max = r.u32()?;
                c.open = Some(Ok(()));
            }
            MSG_CHANNEL_OPEN_FAILURE => {
                let c = self.chan(r.u32()?)?;
                let code = r.u32()?;
                let why = r.utf8()?;
                c.open = Some(Err(format!("{why} (code {code})")));
                c.closed = true;
            }
            MSG_CHANNEL_WINDOW_ADJUST => {
                let c = self.chan(r.u32()?)?;
                c.remote_window = c.remote_window.saturating_add(r.u32()?);
            }
            MSG_CHANNEL_DATA | MSG_CHANNEL_EXTENDED_DATA => {
                let id = r.u32()?;
                let ext = p[0] == MSG_CHANNEL_EXTENDED_DATA;
                if ext {
                    r.u32()?;
                }
                let data = r.string()?;
                let c = self.chan(id)?;
                let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
                if len > c.local_window {
                    return Err(SshError::Protocol("the server sent past the channel window".into()));
                }
                c.local_window -= len;
                if ext {
                    c.stderr.extend_from_slice(data);
                } else {
                    c.data.extend(data);
                }
            }
            MSG_CHANNEL_EOF => self.chan(r.u32()?)?.eof = true,
            MSG_CHANNEL_CLOSE => {
                let id = r.u32()?;
                let c = self.chan(id)?;
                c.eof = true;
                c.closed = true;
                if !c.close_sent {
                    c.close_sent = true;
                    let remote = c.remote;
                    let mut w = Writer::msg(MSG_CHANNEL_CLOSE);
                    w.u32(remote);
                    self.t.send(&w.into_inner()).await?;
                }
            }
            MSG_CHANNEL_REQUEST => {
                let id = r.u32()?;
                let name = r.string()?.to_vec();
                let want_reply = r.bool()?;
                let c = self.chan(id)?;
                match name.as_slice() {
                    b"exit-status" => c.exit_status = Some(r.u32()?),
                    b"exit-signal" => c.exit_signal = Some(r.utf8()?),
                    _ => {}
                }
                if want_reply {
                    let remote = c.remote;
                    let mut w = Writer::msg(MSG_CHANNEL_FAILURE);
                    w.u32(remote);
                    self.t.send(&w.into_inner()).await?;
                }
            }
            MSG_CHANNEL_SUCCESS | MSG_CHANNEL_FAILURE => {
                let ok = p[0] == MSG_CHANNEL_SUCCESS;
                self.chan(r.u32()?)?.replies.push_back(ok);
            }
            // Late authentication chatter, or a message we have no use for.
            _ => {}
        }
        Ok(())
    }

    fn chan(&mut self, id: u32) -> Result<&mut Channel, SshError> {
        self.channels.get_mut(&id).ok_or_else(|| SshError::Protocol(format!("a message for unknown channel {id}")))
    }

    /// Open a `session` channel.
    pub(crate) async fn open_session(&mut self) -> Result<u32, SshError> {
        let id = self.next_id;
        self.next_id += 1;
        self.channels.insert(id, Channel { local_window: WINDOW, ..Channel::default() });
        let mut w = Writer::msg(MSG_CHANNEL_OPEN);
        w.string("session").u32(id).u32(WINDOW).u32(MAX_PACKET);
        self.t.send(&w.into_inner()).await?;
        loop {
            if let Some(result) = self.chan(id)?.open.clone() {
                return match result {
                    Ok(()) => Ok(id),
                    Err(why) => {
                        self.channels.remove(&id);
                        Err(SshError::Channel(format!("the server refused a session channel: {why}")))
                    }
                };
            }
            self.pump().await?;
        }
    }

    /// A channel request that wants a reply; `Ok(false)` if the server refused it.
    pub(crate) async fn request(&mut self, id: u32, name: &str, fields: impl FnOnce(&mut Writer)) -> Result<bool, SshError> {
        let remote = self.chan(id)?.remote;
        let mut w = Writer::msg(MSG_CHANNEL_REQUEST);
        w.u32(remote).string(name).bool(true);
        fields(&mut w);
        self.t.send(&w.into_inner()).await?;
        loop {
            let c = self.chan(id)?;
            if let Some(ok) = c.replies.pop_front() {
                return Ok(ok);
            }
            if c.closed {
                return Ok(false);
            }
            self.pump().await?;
        }
    }

    /// Send `data` on a channel, within the server's window and packet size.
    pub(crate) async fn write(&mut self, id: u32, mut data: &[u8]) -> Result<(), SshError> {
        while !data.is_empty() {
            let c = self.chan(id)?;
            if c.closed {
                return Err(SshError::Channel("the channel was closed by the server".into()));
            }
            let room = c.remote_window.min(c.remote_max) as usize;
            if room == 0 {
                // Waiting for the server to take more. Keep taking what it sends
                // meanwhile — a command echoing its input (cat) stops reading until
                // its output is read, and would otherwise wait on us forever.
                self.pump().await?;
                self.replenish(id).await?;
                continue;
            }
            let n = room.min(data.len());
            c.remote_window -= n as u32;
            let remote = c.remote;
            let mut w = Writer::msg(MSG_CHANNEL_DATA);
            w.u32(remote).string(&data[..n]);
            self.t.send(&w.into_inner()).await?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Give the server back the window it has used, once that is half of it.
    async fn replenish(&mut self, id: u32) -> Result<(), SshError> {
        let c = self.chan(id)?;
        let used = WINDOW - c.local_window;
        if used >= WINDOW / 2 && !c.closed {
            c.local_window = WINDOW;
            let remote = c.remote;
            let mut w = Writer::msg(MSG_CHANNEL_WINDOW_ADJUST);
            w.u32(remote).u32(used);
            self.t.send(&w.into_inner()).await?;
        }
        Ok(())
    }

    /// Up to `max` bytes the server sent on a channel, waiting for some; empty at
    /// the end of its data.
    pub(crate) async fn read(&mut self, id: u32, max: usize) -> Result<Vec<u8>, SshError> {
        loop {
            let c = self.chan(id)?;
            if !c.data.is_empty() {
                let n = max.min(c.data.len());
                let out: Vec<u8> = c.data.drain(..n).collect();
                self.replenish(id).await?;
                return Ok(out);
            }
            if c.eof || c.closed {
                return Ok(Vec::new());
            }
            self.pump().await?;
        }
    }

    /// Say no more data will come from us.
    pub(crate) async fn send_eof(&mut self, id: u32) -> Result<(), SshError> {
        let c = self.chan(id)?;
        if c.closed {
            return Ok(());
        }
        let remote = c.remote;
        let mut w = Writer::msg(MSG_CHANNEL_EOF);
        w.u32(remote);
        self.t.send(&w.into_inner()).await
    }

    /// Close a channel and wait for the server's close.
    pub(crate) async fn close(&mut self, id: u32) -> Result<(), SshError> {
        let c = self.chan(id)?;
        if !c.close_sent {
            c.close_sent = true;
            let remote = c.remote;
            let mut w = Writer::msg(MSG_CHANNEL_CLOSE);
            w.u32(remote);
            self.t.send(&w.into_inner()).await?;
        }
        while !self.chan(id)?.closed {
            self.pump().await?;
        }
        self.channels.remove(&id);
        Ok(())
    }

    /// Run `command` on the server (its default shell interprets it) and collect
    /// its output and exit status.
    pub async fn exec(&mut self, command: &str) -> Result<ExecOutput, SshError> {
        self.exec_with_input(command, &[]).await
    }

    /// [`exec`](Self::exec), writing `input` to the command's standard input first.
    pub async fn exec_with_input(&mut self, command: &str, input: &[u8]) -> Result<ExecOutput, SshError> {
        let id = self.open_session().await?;
        if !self.request(id, "exec", |w| {
            w.string(command);
        })
        .await?
        {
            self.close(id).await?;
            return Err(SshError::Channel(format!("the server refused to run {command:?}")));
        }
        if !input.is_empty() {
            self.write(id, input).await?;
        }
        self.send_eof(id).await?;
        let mut out = ExecOutput::default();
        loop {
            let part = self.read(id, usize::MAX).await?;
            if part.is_empty() {
                break;
            }
            out.stdout.extend(part);
        }
        // The exit status may follow the EOF; wait for the close.
        while !self.chan(id)?.closed {
            self.pump().await?;
        }
        let c = self.chan(id)?;
        out.stderr = std::mem::take(&mut c.stderr);
        out.exit_status = c.exit_status;
        out.exit_signal = c.exit_signal.take();
        out.stdout.extend(c.data.drain(..));
        self.close(id).await?;
        Ok(out)
    }

    /// Say goodbye (SSH_MSG_DISCONNECT, "by application") and drop the connection.
    pub async fn disconnect(mut self) -> Result<(), SshError> {
        let mut w = Writer::msg(transport::MSG_DISCONNECT);
        w.u32(11).string("bye").string("");
        self.t.send(&w.into_inner()).await
    }
}

/// A USERAUTH_SUCCESS / USERAUTH_FAILURE (or the method's 60) as a verdict.
fn auth_verdict(p: &[u8]) -> Result<Result<(), Vec<String>>, SshError> {
    match p[0] {
        MSG_USERAUTH_SUCCESS => Ok(Ok(())),
        MSG_USERAUTH_FAILURE => Ok(Err(Reader::new(&p[1..]).name_list()?)),
        MSG_USERAUTH_60 => Err(SshError::Auth { allowed: vec!["(the server asks for a password change)".into()] }),
        m => Err(SshError::Protocol(format!("message {m} during authentication"))),
    }
}
