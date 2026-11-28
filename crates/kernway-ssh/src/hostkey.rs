//! Public keys (RFC 4253 §6.6, RFC 5656, RFC 8709, RFC 8332): their wire form,
//! signature checks, `SHA256:` fingerprints, and OpenSSH `known_hosts` matching.

use crate::wire::{Reader, Writer};
use crate::SshError;
use kernway_wire::base64;
use ring::signature::{self, UnparsedPublicKey};

/// A server's (or a user's) public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    /// `ssh-ed25519`.
    Ed25519([u8; 32]),
    /// `ecdsa-sha2-nistp256` / `-nistp384`: the uncompressed point.
    Ecdsa {
        /// The curve.
        curve: Curve,
        /// The uncompressed point (`04 || x || y`).
        point: Vec<u8>,
    },
    /// `ssh-rsa` (signed with SHA-256 or SHA-512): exponent and modulus, big-endian.
    Rsa {
        /// The public exponent.
        e: Vec<u8>,
        /// The modulus.
        n: Vec<u8>,
    },
}

/// An ECDSA curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    /// NIST P-256.
    P256,
    /// NIST P-384.
    P384,
}

impl Curve {
    fn name(self) -> &'static str {
        match self {
            Curve::P256 => "nistp256",
            Curve::P384 => "nistp384",
        }
    }

    fn scalar_len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
        }
    }
}

impl PublicKey {
    /// Parse the wire form (`string type, ...`).
    pub fn from_blob(blob: &[u8]) -> Result<Self, SshError> {
        let mut r = Reader::new(blob);
        let kind = r.string()?;
        let key = match kind {
            b"ssh-ed25519" => {
                let k = r.string()?;
                PublicKey::Ed25519(k.try_into().map_err(|_| SshError::Protocol("an Ed25519 key that is not 32 bytes".into()))?)
            }
            b"ecdsa-sha2-nistp256" | b"ecdsa-sha2-nistp384" => {
                let curve = if kind == b"ecdsa-sha2-nistp256" { Curve::P256 } else { Curve::P384 };
                if r.string()? != curve.name().as_bytes() {
                    return Err(SshError::Protocol("an ECDSA key whose curve does not match its type".into()));
                }
                let point = r.string()?;
                if point.len() != 1 + 2 * curve.scalar_len() || point[0] != 4 {
                    return Err(SshError::Protocol("an ECDSA point that is not uncompressed".into()));
                }
                PublicKey::Ecdsa { curve, point: point.to_vec() }
            }
            b"ssh-rsa" => {
                let e = r.mpint()?.to_vec();
                let n = r.mpint()?.to_vec();
                PublicKey::Rsa { e, n }
            }
            other => return Err(SshError::HostKey(format!("unsupported key type {}", String::from_utf8_lossy(other)))),
        };
        if !r.is_empty() {
            return Err(SshError::Protocol("trailing bytes after a public key".into()));
        }
        Ok(key)
    }

    /// The wire form.
    #[must_use]
    pub fn to_blob(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            PublicKey::Ed25519(k) => {
                w.string("ssh-ed25519").string(k);
            }
            PublicKey::Ecdsa { curve, point } => {
                w.string(self.type_name()).string(curve.name()).string(point);
            }
            PublicKey::Rsa { e, n } => {
                w.string("ssh-rsa").mpint(e).mpint(n);
            }
        }
        w.into_inner()
    }

    /// The key type as `known_hosts` and `authorized_keys` write it.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            PublicKey::Ed25519(_) => "ssh-ed25519",
            PublicKey::Ecdsa { curve: Curve::P256, .. } => "ecdsa-sha2-nistp256",
            PublicKey::Ecdsa { curve: Curve::P384, .. } => "ecdsa-sha2-nistp384",
            PublicKey::Rsa { .. } => "ssh-rsa",
        }
    }

    /// The signature algorithms this key can be used with, best first.
    pub(crate) fn algorithms(&self) -> &'static [&'static str] {
        match self {
            PublicKey::Ed25519(_) => &["ssh-ed25519"],
            PublicKey::Ecdsa { curve: Curve::P256, .. } => &["ecdsa-sha2-nistp256"],
            PublicKey::Ecdsa { curve: Curve::P384, .. } => &["ecdsa-sha2-nistp384"],
            PublicKey::Rsa { .. } => &["rsa-sha2-512", "rsa-sha2-256"],
        }
    }

    /// The OpenSSH fingerprint: `SHA256:` and the unpadded base64 of the blob's SHA-256.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let d = ring::digest::digest(&ring::digest::SHA256, &self.to_blob());
        format!("SHA256:{}", base64::encode_unpadded(d.as_ref()))
    }

    /// The key as one `known_hosts` / `authorized_keys` line field pair: `type base64`.
    #[must_use]
    pub fn to_openssh(&self) -> String {
        format!("{} {}", self.type_name(), base64::encode(&self.to_blob()))
    }

    /// Parse `type base64 [comment]` (one line of a `.pub` file).
    pub fn from_openssh(line: &str) -> Result<Self, SshError> {
        let mut parts = line.split_ascii_whitespace();
        let (Some(kind), Some(b64)) = (parts.next(), parts.next()) else {
            return Err(SshError::Key("a public key line needs a type and a base64 key".into()));
        };
        let blob = base64::decode(b64).ok_or_else(|| SshError::Key("bad base64 in a public key".into()))?;
        let key = PublicKey::from_blob(&blob)?;
        if key.type_name() != kind {
            return Err(SshError::Key(format!("a {} key labelled {kind}", key.type_name())));
        }
        Ok(key)
    }

    /// Check `sig` (the wire form: `string algorithm, string signature`) over `data`.
    /// `alg` is the algorithm the key exchange agreed on (it must be the one used).
    pub(crate) fn verify(&self, alg: &str, data: &[u8], sig: &[u8]) -> Result<(), SshError> {
        let bad = || SshError::HostKey("the host key's signature over the key exchange is not valid".into());
        let mut r = Reader::new(sig);
        let used = r.string()?;
        let raw = r.string()?;
        if used != alg.as_bytes() {
            return Err(SshError::HostKey(format!("signed with {} where {alg} was agreed", String::from_utf8_lossy(used))));
        }
        match (self, alg) {
            (PublicKey::Ed25519(k), "ssh-ed25519") => UnparsedPublicKey::new(&signature::ED25519, k).verify(data, raw).map_err(|_| bad()),
            (PublicKey::Ecdsa { curve, point }, _) => {
                // The signature is `mpint r, mpint s`; ring wants them fixed-width.
                let mut rs = Reader::new(raw);
                let (sr, ss) = (rs.mpint()?, rs.mpint()?);
                let n = curve.scalar_len();
                if sr.len() > n || ss.len() > n {
                    return Err(bad());
                }
                let mut fixed = vec![0u8; 2 * n];
                fixed[n - sr.len()..n].copy_from_slice(sr);
                fixed[2 * n - ss.len()..].copy_from_slice(ss);
                let alg: &dyn signature::VerificationAlgorithm = match curve {
                    Curve::P256 => &signature::ECDSA_P256_SHA256_FIXED,
                    Curve::P384 => &signature::ECDSA_P384_SHA384_FIXED,
                };
                UnparsedPublicKey::new(alg, point).verify(data, &fixed).map_err(|_| bad())
            }
            (PublicKey::Rsa { e, n }, "rsa-sha2-256" | "rsa-sha2-512") => {
                let params = if alg == "rsa-sha2-256" { &signature::RSA_PKCS1_2048_8192_SHA256 } else { &signature::RSA_PKCS1_2048_8192_SHA512 };
                signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }.verify(params, data, raw).map_err(|_| bad())
            }
            _ => Err(SshError::HostKey(format!("a {} key cannot sign with {alg}", self.type_name()))),
        }
    }
}

/// What the client checks the server's host key against. Without a check anyone
/// between the client and the server could pose as the server.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// An OpenSSH `known_hosts` file (read on the blocking pool). The host must be
    /// listed with this key; a different key of the same type is refused as a
    /// possible attack, and `@revoked` keys are refused everywhere.
    KnownHosts(std::path::PathBuf),
    /// `known_hosts` lines given as text (the file's contents).
    KnownHostsText(String),
    /// The key's `SHA256:` fingerprint, as `ssh-keygen -lf` prints it.
    Fingerprint(String),
    /// Accept any host key. The session is encrypted but the server is not
    /// authenticated: only for tests, or a first connection whose fingerprint
    /// you then check by other means ([`crate::SshClient::host_key`]).
    AcceptAny,
}

/// The name a host is listed under in `known_hosts`: `host`, or `[host]:port`
/// off port 22.
pub(crate) fn known_hosts_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{port}", host.to_ascii_lowercase())
    }
}

/// Check `key` for `host:port` against `known_hosts` text.
pub(crate) fn check_known_hosts(text: &str, host: &str, port: u16, key: &PublicKey) -> Result<(), SshError> {
    let name = known_hosts_name(host, port);
    let mut same_type = false;
    let mut listed = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let mut first = fields.next().unwrap_or("");
        let marker = if first.starts_with('@') {
            let m = first;
            first = fields.next().unwrap_or("");
            Some(m)
        } else {
            None
        };
        let (Some(kind), Some(b64)) = (fields.next(), fields.next()) else { continue };
        let Some(entry) = base64::decode(b64).and_then(|b| PublicKey::from_blob(&b).ok()) else { continue };
        if entry.type_name() != kind {
            continue;
        }
        match marker {
            Some("@revoked") => {
                if &entry == key {
                    return Err(SshError::HostKey(format!("{} is revoked in known_hosts", key.fingerprint())));
                }
                continue;
            }
            Some(_) => continue, // @cert-authority: host certificates are not supported
            None => {}
        }
        if !hosts_match(first, &name) {
            continue;
        }
        listed = true;
        if &entry == key {
            return Ok(());
        }
        if entry.type_name() == key.type_name() {
            same_type = true;
        }
    }
    Err(SshError::HostKey(if same_type {
        format!(
            "the {} key of {name} has CHANGED (it is now {}); someone may be impersonating the server — check before trusting it",
            key.type_name(),
            key.fingerprint()
        )
    } else if listed {
        format!("{name} is known with other key types, not {} {}", key.type_name(), key.fingerprint())
    } else {
        format!("{name} is not in known_hosts (its {} key is {})", key.type_name(), key.fingerprint())
    }))
}

/// The key types `known_hosts` lists for `host:port` (not revoked ones).
pub(crate) fn known_types(text: &str, host: &str, port: u16) -> Vec<String> {
    let name = known_hosts_name(host, port);
    let mut types = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(hosts) = fields.next() else { continue };
        if hosts.starts_with('#') || hosts.starts_with('@') {
            continue;
        }
        if let Some(kind) = fields.next() {
            if hosts_match(hosts, &name) && !types.iter().any(|t| t == kind) {
                types.push(kind.to_owned());
            }
        }
    }
    types
}

/// Whether a `known_hosts` host field (comma-separated patterns, `!negations`, or one
/// hashed `|1|salt|hash` entry) matches `name`.
fn hosts_match(field: &str, name: &str) -> bool {
    if let Some(hashed) = field.strip_prefix("|1|") {
        let Some((salt, hash)) = hashed.split_once('|') else { return false };
        let (Some(salt), Some(hash)) = (base64::decode(salt), base64::decode(hash)) else { return false };
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        return ring::hmac::verify(&key, name.as_bytes(), &hash).is_ok();
    }
    let mut matched = false;
    for pattern in field.split(',') {
        if let Some(neg) = pattern.strip_prefix('!') {
            if glob(&neg.to_ascii_lowercase(), name) {
                return false;
            }
        } else if glob(&pattern.to_ascii_lowercase(), name) {
            matched = true;
        }
    }
    matched
}

/// `*` and `?` wildcards, as in `ssh_config` patterns.
fn glob(pattern: &str, s: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), s.as_bytes());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ssh-keygen -t ed25519 (a throwaway key) and its fingerprint from ssh-keygen -lf.
    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINb+N62ntVz77jZ/Av9WrFlnosiqWf/9mmT0EqHShyRm kw";
    const ED_FP: &str = "SHA256:O//oVH8oxrWu+2pQJbIYCsuRthATPQ7y8HMqHv2pX24";

    #[test]
    fn keys_parse_print_and_fingerprint_like_openssh() {
        let k = PublicKey::from_openssh(ED).unwrap();
        assert_eq!(k.type_name(), "ssh-ed25519");
        assert_eq!(PublicKey::from_blob(&k.to_blob()).unwrap(), k);
        assert_eq!(k.to_openssh(), ED.trim_end_matches(" kw"));
        assert_eq!(k.fingerprint(), ED_FP);
        assert!(PublicKey::from_openssh("ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAINb+N62ntVz77jZ/Av9WrFlnosiqWf/9mmT0EqHShyRm").is_err(), "a mislabelled key");
    }

    #[test]
    fn known_hosts_lines_match_like_openssh() {
        let k = PublicKey::from_openssh(ED).unwrap();
        let line = |hosts: &str| format!("{hosts} {}\n", k.to_openssh());
        assert!(check_known_hosts(&line("files.example"), "files.example", 22, &k).is_ok());
        assert!(check_known_hosts(&line("Files.Example,10.0.0.5"), "files.example", 22, &k).is_ok(), "names are case-insensitive");
        assert!(check_known_hosts(&line("[files.example]:2222"), "files.example", 2222, &k).is_ok());
        assert!(check_known_hosts(&line("files.example"), "files.example", 2222, &k).is_err(), "another port is another entry");
        assert!(check_known_hosts(&line("*.example,!bad.example"), "files.example", 22, &k).is_ok());
        assert!(check_known_hosts(&line("*.example,!bad.example"), "bad.example", 22, &k).is_err());
        assert!(check_known_hosts(&line("fil?s.exam*"), "files.example", 22, &k).is_ok());
        // Hashed with HashKnownHosts (salt / HMAC-SHA1 of "[127.0.0.1]:2222").
        let salt = b"0123456789abcdefghij";
        let mac = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, salt), b"[127.0.0.1]:2222");
        let hashed = format!("|1|{}|{}", base64::encode(salt), base64::encode(mac.as_ref()));
        assert!(check_known_hosts(&line(&hashed), "127.0.0.1", 2222, &k).is_ok());
        assert!(check_known_hosts(&line(&hashed), "127.0.0.1", 22, &k).is_err());

        let other = PublicKey::Ed25519([1; 32]);
        let e = check_known_hosts(&line("files.example"), "files.example", 22, &other).unwrap_err();
        assert!(e.to_string().contains("CHANGED"), "{e}");
        let e = check_known_hosts("", "files.example", 22, &k).unwrap_err();
        assert!(e.to_string().contains("not in known_hosts") && e.to_string().contains(ED_FP), "{e}");
        let revoked = format!("@revoked * {}\n{}", k.to_openssh(), line("files.example"));
        assert!(check_known_hosts(&revoked, "files.example", 22, &k).unwrap_err().to_string().contains("revoked"));
        assert!(check_known_hosts(&format!("# comment\n\ngarbage line\n{}", line("files.example")), "files.example", 22, &k).is_ok());
    }

    #[test]
    fn signatures_are_checked() {
        let rng = ring::rand::SystemRandom::new();
        let pair = signature::Ed25519KeyPair::from_pkcs8(signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
        let key = PublicKey::Ed25519(ring::signature::KeyPair::public_key(&pair).as_ref().try_into().unwrap());
        let mut w = Writer::new();
        w.string("ssh-ed25519").string(pair.sign(b"exchange hash").as_ref());
        let sig = w.into_inner();
        assert!(key.verify("ssh-ed25519", b"exchange hash", &sig).is_ok());
        assert!(key.verify("ssh-ed25519", b"another hash", &sig).is_err());
        assert!(key.verify("rsa-sha2-256", b"exchange hash", &sig).is_err(), "the agreed algorithm must be the one used");
    }
}
