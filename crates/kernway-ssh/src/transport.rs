//! The transport layer (RFC 4253): version exchange, binary packets, key exchange
//! (RFC 5656 ECDH, RFC 8731 curve25519) with strict-kex, server authentication, and
//! rekeying started by either side.

use crate::cipher::{CipherKind, Protection};
use crate::hostkey::{self, HostKeyPolicy, PublicKey};
use crate::wire::{Reader, Writer};
use crate::SshError;
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use rt_net::AsyncTcpStream;
use std::collections::VecDeque;

pub(crate) const MSG_DISCONNECT: u8 = 1;
const MSG_IGNORE: u8 = 2;
const MSG_UNIMPLEMENTED: u8 = 3;
const MSG_DEBUG: u8 = 4;
pub(crate) const MSG_SERVICE_REQUEST: u8 = 5;
pub(crate) const MSG_SERVICE_ACCEPT: u8 = 6;
const MSG_EXT_INFO: u8 = 7;
const MSG_KEXINIT: u8 = 20;
const MSG_NEWKEYS: u8 = 21;
const MSG_KEX_ECDH_INIT: u8 = 30;
const MSG_KEX_ECDH_REPLY: u8 = 31;

/// Our identification string (RFC 4253 §4.2), without the CR LF.
const VERSION: &str = "SSH-2.0-kernway_0.1";

pub(crate) const KEX: [&str; 4] = ["curve25519-sha256", "curve25519-sha256@libssh.org", "ecdh-sha2-nistp256", "ecdh-sha2-nistp384"];
/// Offered in the first KEXINIT only: ask for EXT_INFO (RFC 8308) and strict kex
/// (OpenSSH PROTOCOL §1.10, the CVE-2023-48795 "Terrapin" fix).
const KEX_FIRST_EXTRA: [&str; 2] = ["ext-info-c", "kex-strict-c-v00@openssh.com"];
pub(crate) const HOST_KEY_ALGS: [&str; 5] = ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "rsa-sha2-512", "rsa-sha2-256"];
/// Never used (every cipher we offer is AEAD), but a KEXINIT must list some.
const MACS: [&str; 2] = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-256"];

/// Rekey after this much traffic in either direction unless the caller says otherwise.
pub(crate) const DEFAULT_REKEY_BYTES: u64 = 1 << 30;

/// The algorithms the client offers, best first — a subset of what it supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Offer {
    pub(crate) kex: Vec<&'static str>,
    pub(crate) host_key: Vec<&'static str>,
    pub(crate) ciphers: Vec<&'static str>,
}

impl Default for Offer {
    fn default() -> Self {
        Self { kex: KEX.to_vec(), host_key: HOST_KEY_ALGS.to_vec(), ciphers: CipherKind::ALL.iter().map(|c| c.name()).collect() }
    }
}

/// The negotiated algorithms of one key exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// The key exchange method.
    pub kex: String,
    /// The host key signature algorithm.
    pub host_key: String,
    /// Client → server cipher.
    pub cipher_out: String,
    /// Server → client cipher.
    pub cipher_in: String,
}

pub(crate) struct Transport {
    sock: AsyncTcpStream,
    /// Socket bytes read but not yet consumed.
    rbuf: Vec<u8>,
    /// Where a socket read lands before it joins `rbuf` (so an abandoned read
    /// leaves `rbuf` untouched).
    scratch: Vec<u8>,
    seal: Protection,
    open: Protection,
    send_seq: u32,
    recv_seq: u32,
    rng: SystemRandom,
    server_version: Vec<u8>,
    session_id: Option<Vec<u8>>,
    strict: bool,
    /// Non-transport messages that arrived while a key exchange was running.
    pending: VecDeque<Vec<u8>>,
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    known_hosts: Option<String>,
    pub(crate) host_key: Option<PublicKey>,
    pub(crate) negotiated: Option<Negotiated>,
    pub(crate) server_sig_algs: Option<Vec<String>>,
    traffic: u64,
    rekey_bytes: u64,
    pub(crate) rekeys: u32,
    offer: Offer,
}

/// What a received packet's first byte says it is.
fn msg(p: &[u8]) -> u8 {
    p.first().copied().unwrap_or(0)
}

impl Transport {
    /// Connect over `sock`: exchange versions, run the first key exchange and check
    /// the server's host key against `policy` (`known_hosts` already read, if any).
    pub(crate) async fn start(
        sock: AsyncTcpStream,
        host: &str,
        port: u16,
        policy: HostKeyPolicy,
        known_hosts: Option<String>,
        rekey_bytes: u64,
        offer: Offer,
    ) -> Result<Self, SshError> {
        let mut t = Transport {
            sock,
            rbuf: Vec::new(),
            scratch: vec![0u8; 32 * 1024],
            seal: Protection::None,
            open: Protection::None,
            send_seq: 0,
            recv_seq: 0,
            rng: SystemRandom::new(),
            server_version: Vec::new(),
            session_id: None,
            strict: false,
            pending: VecDeque::new(),
            host: host.to_owned(),
            port,
            policy,
            known_hosts,
            host_key: None,
            negotiated: None,
            server_sig_algs: None,
            traffic: 0,
            rekey_bytes,
            rekeys: 0,
            offer,
        };
        t.sock.write_all(format!("{VERSION}\r\n").as_bytes()).await?;
        t.server_version = t.read_version().await?;
        t.kex(None).await?;
        Ok(t)
    }

    /// The server's identification string (`SSH-2.0-OpenSSH_9.9`).
    pub(crate) fn server_version(&self) -> String {
        String::from_utf8_lossy(&self.server_version).into_owned()
    }

    pub(crate) fn session_id(&self) -> &[u8] {
        self.session_id.as_deref().unwrap_or_default()
    }

    /// RFC 4253 §4.2: lines before the version line are allowed (a banner); the
    /// version line is `SSH-2.0-…` (or `SSH-1.99-…`), at most 255 bytes.
    async fn read_version(&mut self) -> Result<Vec<u8>, SshError> {
        for _ in 0..64 {
            let line = loop {
                if let Some(i) = self.rbuf.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = self.rbuf.drain(..=i).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    break line;
                }
                if self.rbuf.len() > 8192 {
                    return Err(SshError::Protocol("no SSH version line from the server".into()));
                }
                self.fill().await?;
            };
            if line.starts_with(b"SSH-") {
                if !(line.starts_with(b"SSH-2.0-") || line.starts_with(b"SSH-1.99-")) {
                    return Err(SshError::Protocol(format!("the server speaks {} (SSH 2 is required)", String::from_utf8_lossy(&line))));
                }
                if line.len() > 253 {
                    return Err(SshError::Protocol("an over-long version line".into()));
                }
                return Ok(line);
            }
        }
        Err(SshError::Protocol("no SSH version line from the server".into()))
    }

    /// Read more socket bytes into `rbuf`. Cancel-safe: the read lands in `scratch`
    /// and joins `rbuf` only once it completed, so dropping this future mid-way (a
    /// console waiting on input and the network at once) loses nothing.
    pub(crate) async fn fill(&mut self) -> Result<(), SshError> {
        let n = self.sock.read(&mut self.scratch).await?;
        self.rbuf.extend_from_slice(&self.scratch[..n]);
        if n == 0 {
            return Err(SshError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection")));
        }
        Ok(())
    }

    /// Protect and write one packet, as it stands (no rekey check).
    async fn write_packet(&mut self, payload: &[u8]) -> Result<(), SshError> {
        let wire = self.seal.seal(self.send_seq, payload, &self.rng)?;
        self.send_seq = self.send_seq.wrapping_add(1);
        self.traffic += wire.len() as u64;
        self.sock.write_all(&wire).await?;
        Ok(())
    }

    /// Read and open one packet, as it comes.
    async fn read_packet(&mut self) -> Result<Vec<u8>, SshError> {
        loop {
            if let Some(p) = self.try_decode()? {
                return Ok(p);
            }
            self.fill().await?;
        }
    }

    /// The next whole packet already in `rbuf`, opened — without reading the socket.
    fn try_decode(&mut self) -> Result<Option<Vec<u8>>, SshError> {
        if self.rbuf.len() < Protection::first_len() {
            return Ok(None);
        }
        let first: [u8; 4] = self.rbuf[..4].try_into().expect("4 bytes");
        let rest = self.open.remaining(self.recv_seq, first)?;
        if self.rbuf.len() < 4 + rest {
            return Ok(None);
        }
        let packet: Vec<u8> = self.rbuf.drain(..4 + rest).collect();
        self.traffic += packet.len() as u64;
        let payload = self.open.open(self.recv_seq, packet)?;
        self.recv_seq = self.recv_seq.wrapping_add(1);
        if payload.is_empty() {
            return Err(SshError::Protocol("an empty packet".into()));
        }
        Ok(Some(payload))
    }

    /// Send a message, rekeying first when the traffic limit has been reached.
    pub(crate) async fn send(&mut self, payload: &[u8]) -> Result<(), SshError> {
        if self.traffic >= self.rekey_bytes {
            // Boxed: the key exchange is a large future, and `send` is awaited everywhere.
            Box::pin(self.kex(None)).await?;
        }
        self.write_packet(payload).await
    }

    /// The next message for the layers above: transport messages (ignore, debug,
    /// extension info, a rekey the server starts, a disconnect) are handled here.
    pub(crate) async fn recv(&mut self) -> Result<Vec<u8>, SshError> {
        loop {
            if let Some(p) = self.recv_buffered().await? {
                return Ok(p);
            }
            self.fill().await?;
        }
    }

    /// [`recv`](Self::recv) from what is already buffered: `None` when no whole
    /// message is there (a key exchange the server starts still runs to the end).
    pub(crate) async fn recv_buffered(&mut self) -> Result<Option<Vec<u8>>, SshError> {
        if let Some(p) = self.pending.pop_front() {
            return Ok(Some(p));
        }
        loop {
            let Some(p) = self.try_decode()? else { return Ok(None) };
            match msg(&p) {
                MSG_DISCONNECT => return Err(disconnected(&p)),
                MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED => {}
                MSG_EXT_INFO => self.ext_info(&p)?,
                MSG_KEXINIT => {
                    Box::pin(self.kex(Some(p))).await?;
                    if let Some(p) = self.pending.pop_front() {
                        return Ok(Some(p));
                    }
                }
                _ => return Ok(Some(p)),
            }
        }
    }

    /// RFC 8308: the server's `server-sig-algs` (which RSA signature it takes).
    fn ext_info(&mut self, p: &[u8]) -> Result<(), SshError> {
        let mut r = Reader::new(&p[1..]);
        let n = r.u32()?;
        for _ in 0..n {
            let name = r.string()?;
            let value = r.string()?;
            if name == b"server-sig-algs" {
                let list = String::from_utf8_lossy(value);
                self.server_sig_algs = Some(list.split(',').map(str::to_owned).collect());
            }
        }
        Ok(())
    }

    fn kexinit(&self, first: bool) -> Result<Vec<u8>, SshError> {
        let mut cookie = [0u8; 16];
        self.rng.fill(&mut cookie).map_err(|_| SshError::Protocol("the system RNG failed".into()))?;
        let mut kex: Vec<&str> = self.offer.kex.clone();
        if first {
            kex.extend(KEX_FIRST_EXTRA);
        }
        let host_keys = self.host_key_order();
        let ciphers = self.offer.ciphers.clone();
        let mut w = Writer::msg(MSG_KEXINIT);
        w.raw(&cookie)
            .name_list(&kex)
            .name_list(&host_keys)
            .name_list(&ciphers)
            .name_list(&ciphers)
            .name_list(&MACS)
            .name_list(&MACS)
            .name_list(&["none"])
            .name_list(&["none"])
            .name_list(&[])
            .name_list(&[])
            .bool(false)
            .u32(0);
        Ok(w.into_inner())
    }

    /// Host key algorithms, those of key types `known_hosts` lists for this host first
    /// (as OpenSSH does): otherwise a host known by its ECDSA key would be refused as
    /// unknown after agreeing on Ed25519. After the first exchange, only the type the
    /// server already proved.
    fn host_key_order(&self) -> Vec<&'static str> {
        let offered = &self.offer.host_key;
        if let Some(k) = &self.host_key {
            return k.algorithms().iter().copied().filter(|a| offered.contains(a)).collect();
        }
        let known = self.known_hosts.as_deref().map(|t| hostkey::known_types(t, &self.host, self.port)).unwrap_or_default();
        let mut algs: Vec<&'static str> = offered.iter().copied().filter(|a| known.iter().any(|k| alg_key_type(a) == *k)).collect();
        let rest: Vec<&'static str> = offered.iter().copied().filter(|a| !algs.contains(a)).collect();
        algs.extend(rest);
        algs
    }

    /// One key exchange; `theirs` is the server's KEXINIT when it started this one.
    async fn kex(&mut self, theirs: Option<Vec<u8>>) -> Result<(), SshError> {
        let first = self.session_id.is_none();
        let ours = self.kexinit(first)?;
        self.write_packet(&ours).await?;
        let theirs = match theirs {
            Some(t) => t,
            None => self.kex_recv(first, MSG_KEXINIT).await?,
        };
        if first {
            // Strict kex: the server's KEXINIT must have been the very first packet.
            let names = Reader::new(&theirs[17..]).name_list().unwrap_or_default();
            self.strict = names.iter().any(|n| n == "kex-strict-s-v00@openssh.com");
            if self.strict && self.recv_seq != 1 {
                return Err(SshError::Protocol("strict kex: the server's KEXINIT was not its first packet".into()));
            }
        }
        let n = negotiate(&ours, &theirs)?;
        if follows_with_wrong_guess(&theirs, &n)? {
            // RFC 4253 §7: the server guessed wrong; drop its guessed packet.
            self.kex_recv(first, 0).await?;
        }

        // ECDH (RFC 5656 §4 / RFC 8731).
        let (alg, hash): (&'static agreement::Algorithm, &'static digest::Algorithm) = match n.kex.as_str() {
            "curve25519-sha256" | "curve25519-sha256@libssh.org" => (&agreement::X25519, &digest::SHA256),
            "ecdh-sha2-nistp256" => (&agreement::ECDH_P256, &digest::SHA256),
            _ => (&agreement::ECDH_P384, &digest::SHA384),
        };
        let secret = EphemeralPrivateKey::generate(alg, &self.rng).map_err(|_| SshError::Protocol("ephemeral key generation failed".into()))?;
        let q_c = secret.compute_public_key().map_err(|_| SshError::Protocol("ephemeral key failed".into()))?.as_ref().to_vec();
        let mut w = Writer::msg(MSG_KEX_ECDH_INIT);
        w.string(&q_c);
        self.write_packet(&w.into_inner()).await?;
        let reply = self.kex_recv(first, MSG_KEX_ECDH_REPLY).await?;
        let mut r = Reader::new(&reply[1..]);
        let (k_s, q_s, sig) = (r.string()?.to_vec(), r.string()?.to_vec(), r.string()?.to_vec());
        let shared = agreement::agree_ephemeral(secret, &UnparsedPublicKey::new(alg, &q_s), |k| k.to_vec())
            .map_err(|_| SshError::Protocol("the server's ephemeral key is not a valid point".into()))?;
        let mut k = Writer::new();
        k.mpint(&shared);
        let k = k.into_inner();

        let mut h = Writer::new();
        h.string(VERSION).string(&self.server_version).string(&ours).string(&theirs).string(&k_s).string(&q_c).string(&q_s).raw(&k);
        let h = digest::digest(hash, &h.into_inner()).as_ref().to_vec();

        // The server proves it holds the host key by signing the exchange hash.
        let key = PublicKey::from_blob(&k_s)?;
        key.verify(&n.host_key, &h, &sig)?;
        match &self.host_key {
            Some(known) if *known != key => return Err(SshError::HostKey("the host key changed during a rekey".into())),
            Some(_) => {}
            None => self.check_host_key(&key)?,
        }
        self.host_key = Some(key);
        let session_id = self.session_id.get_or_insert_with(|| h.clone()).clone();

        let out = CipherKind::from_name(&n.cipher_out).expect("negotiated from our list");
        let inn = CipherKind::from_name(&n.cipher_in).expect("negotiated from our list");
        let derive = |letter: u8, len: usize| derive_key(hash, &k, &h, letter, &session_id, len);
        let seal = Protection::new(out, &derive(b'C', out.key_len()), &derive(b'A', out.iv_len()), true);
        let open = Protection::new(inn, &derive(b'D', inn.key_len()), &derive(b'B', inn.iv_len()), false);

        self.write_packet(&[MSG_NEWKEYS]).await?;
        self.seal = seal;
        if self.strict {
            self.send_seq = 0;
        }
        self.kex_recv(first, MSG_NEWKEYS).await?;
        self.open = open;
        if self.strict {
            self.recv_seq = 0;
        }
        self.traffic = 0;
        if !first {
            self.rekeys += 1;
        }
        self.negotiated = Some(n);
        Ok(())
    }

    /// The next key-exchange packet (`want`, or any when 0). Other messages: during
    /// the first exchange with strict kex none is allowed; otherwise transport chatter
    /// is skipped and the rest kept for after the exchange.
    async fn kex_recv(&mut self, first: bool, want: u8) -> Result<Vec<u8>, SshError> {
        loop {
            let p = self.read_packet().await?;
            let m = msg(&p);
            if m == MSG_DISCONNECT {
                return Err(disconnected(&p));
            }
            if want == 0 || m == want {
                return Ok(p);
            }
            if first && self.strict {
                return Err(SshError::Protocol(format!("strict kex: message {m} during the key exchange")));
            }
            match m {
                MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED => {}
                20..=49 => return Err(SshError::Protocol(format!("key exchange message {m} where {want} was expected"))),
                _ if first => return Err(SshError::Protocol(format!("message {m} before the key exchange finished"))),
                _ => self.pending.push_back(p),
            }
        }
    }

    fn check_host_key(&self, key: &PublicKey) -> Result<(), SshError> {
        match &self.policy {
            HostKeyPolicy::AcceptAny => Ok(()),
            HostKeyPolicy::Fingerprint(fp) => {
                if key.fingerprint() == fp.trim() {
                    Ok(())
                } else {
                    Err(SshError::HostKey(format!("the server's key is {}, not the expected {fp}", key.fingerprint())))
                }
            }
            HostKeyPolicy::KnownHosts(_) | HostKeyPolicy::KnownHostsText(_) => {
                hostkey::check_known_hosts(self.known_hosts.as_deref().unwrap_or(""), &self.host, self.port, key)
            }
        }
    }
}

fn disconnected(p: &[u8]) -> SshError {
    let mut r = Reader::new(&p[1..]);
    let reason = r.u32().unwrap_or(0);
    let description = r.utf8().unwrap_or_default();
    SshError::Disconnected { reason, description }
}

/// The key type an algorithm signs with.
fn alg_key_type(alg: &str) -> &str {
    if alg.starts_with("rsa-sha2-") {
        "ssh-rsa"
    } else {
        alg
    }
}

/// RFC 4253 §7.1: for each kind, the first of the client's algorithms the server
/// also lists.
fn negotiate(ours: &[u8], theirs: &[u8]) -> Result<Negotiated, SshError> {
    let lists = |p: &[u8]| -> Result<Vec<Vec<String>>, SshError> {
        let mut r = Reader::new(&p[17..]);
        (0..8).map(|_| r.name_list()).collect()
    };
    let (o, t) = (lists(ours)?, lists(theirs)?);
    let pick = |i: usize, kind: &'static str| {
        o[i].iter()
            .find(|a| t[i].contains(a) && !a.starts_with("ext-info-") && !a.starts_with("kex-strict-"))
            .cloned()
            .ok_or_else(|| SshError::NoCommonAlgorithm { kind, server: t[i].clone() })
    };
    let n = Negotiated { kex: pick(0, "key exchange")?, host_key: pick(1, "host key")?, cipher_out: pick(2, "cipher")?, cipher_in: pick(3, "cipher")? };
    // The ciphers carry their own tag: no MAC is negotiated (as OpenSSH does for AEAD).
    if !t[6].iter().any(|c| c == "none") {
        return Err(SshError::NoCommonAlgorithm { kind: "compression", server: t[6].clone() });
    }
    Ok(n)
}

/// Whether the server sent a guessed key-exchange packet that the negotiation proved
/// wrong (its first kex or host key algorithm is not the agreed one).
fn follows_with_wrong_guess(theirs: &[u8], n: &Negotiated) -> Result<bool, SshError> {
    let mut r = Reader::new(&theirs[17..]);
    let kex = r.name_list()?;
    let host = r.name_list()?;
    for _ in 0..8 {
        r.name_list()?;
    }
    let follows = r.bool()?;
    Ok(follows && (kex.first() != Some(&n.kex) || host.first() != Some(&n.host_key)))
}

/// RFC 4253 §7.2: `HASH(K || H || letter || session_id)`, extended with
/// `HASH(K || H || K1 || …)` until long enough.
fn derive_key(hash: &'static digest::Algorithm, k: &[u8], h: &[u8], letter: u8, session_id: &[u8], len: usize) -> Vec<u8> {
    let mut ctx = digest::Context::new(hash);
    ctx.update(k);
    ctx.update(h);
    ctx.update(&[letter]);
    ctx.update(session_id);
    let mut out = ctx.finish().as_ref().to_vec();
    while out.len() < len {
        let mut ctx = digest::Context::new(hash);
        ctx.update(k);
        ctx.update(h);
        ctx.update(&out);
        out.extend_from_slice(ctx.finish().as_ref());
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kexinit(lists: [&[&str]; 10], follows: bool) -> Vec<u8> {
        let mut w = Writer::msg(MSG_KEXINIT);
        w.raw(&[0; 16]);
        for l in lists {
            w.name_list(l);
        }
        w.bool(follows).u32(0);
        w.into_inner()
    }

    #[test]
    fn the_first_client_algorithm_the_server_knows_wins() {
        let ours = kexinit([&KEX, &HOST_KEY_ALGS, &["chacha20-poly1305@openssh.com", "aes256-gcm@openssh.com"], &["aes256-gcm@openssh.com"], &MACS, &MACS, &["none"], &["none"], &[], &[]], false);
        let theirs = kexinit(
            [
                &["sntrup761x25519-sha512", "ecdh-sha2-nistp256", "kex-strict-s-v00@openssh.com"],
                &["rsa-sha2-256", "ssh-ed25519"],
                &["aes256-gcm@openssh.com", "chacha20-poly1305@openssh.com"],
                &["aes256-gcm@openssh.com"],
                &["hmac-sha1"],
                &["hmac-sha1"],
                &["none", "zlib@openssh.com"],
                &["none"],
                &[],
                &[],
            ],
            false,
        );
        let n = negotiate(&ours, &theirs).unwrap();
        assert_eq!(n.kex, "ecdh-sha2-nistp256");
        assert_eq!(n.host_key, "ssh-ed25519", "the client's preference decides");
        assert_eq!(n.cipher_out, "chacha20-poly1305@openssh.com");
        assert_eq!(n.cipher_in, "aes256-gcm@openssh.com");

        let old = kexinit([&["diffie-hellman-group1-sha1"], &["ssh-dss"], &["3des-cbc"], &["3des-cbc"], &["hmac-md5"], &["hmac-md5"], &["none"], &["none"], &[], &[]], false);
        let e = negotiate(&ours, &old).unwrap_err();
        assert!(matches!(&e, SshError::NoCommonAlgorithm { kind: "key exchange", server } if server == &["diffie-hellman-group1-sha1"]), "{e}");
    }

    #[test]
    fn a_wrong_guess_is_detected() {
        let n = Negotiated { kex: "curve25519-sha256".into(), host_key: "ssh-ed25519".into(), cipher_out: String::new(), cipher_in: String::new() };
        let lists = |kex: &'static str| -> [&'static [&'static str]; 10] {
            let k: &'static [&'static str] = Box::leak(Box::new([kex]));
            [k, &["ssh-ed25519"], &[], &[], &[], &[], &[], &[], &[], &[]]
        };
        assert!(follows_with_wrong_guess(&kexinit(lists("ecdh-sha2-nistp256"), true), &n).unwrap());
        assert!(!follows_with_wrong_guess(&kexinit(lists("curve25519-sha256"), true), &n).unwrap());
        assert!(!follows_with_wrong_guess(&kexinit(lists("ecdh-sha2-nistp256"), false), &n).unwrap());
    }

    #[test]
    fn keys_are_derived_and_extended() {
        let k = derive_key(&digest::SHA256, b"K", b"H", b'C', b"S", 64);
        assert_eq!(k.len(), 64);
        let mut ctx = digest::Context::new(&digest::SHA256);
        ctx.update(b"KHCS");
        let k1 = ctx.finish();
        assert_eq!(&k[..32], k1.as_ref());
        let mut ctx = digest::Context::new(&digest::SHA256);
        ctx.update(b"KH");
        ctx.update(k1.as_ref());
        assert_eq!(&k[32..], ctx.finish().as_ref(), "K2 = HASH(K || H || K1)");
        assert_ne!(derive_key(&digest::SHA256, b"K", b"H", b'D', b"S", 32), k[..32], "each letter its own key");
    }
}
