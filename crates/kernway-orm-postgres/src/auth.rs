//! Password authentication: SCRAM-SHA-256 (RFC 5802 / 7677, with
//! `-PLUS` channel binding to the TLS session) and the legacy MD5 hash.
//! The hashes come from `sha2` / `md-5`; HMAC, PBKDF2 (`Hi`) and base64 are
//! written here, they are a few lines each.

use sha2::{Digest, Sha256, Sha384, Sha512};

/// `md5` + hex(md5(hex(md5(password + user)) + salt)), as `AuthenticationMD5Password` wants.
pub fn md5_password(user: &str, password: &str, salt: [u8; 4]) -> String {
    let inner = hex(&md5::Md5::digest([password.as_bytes(), user.as_bytes()].concat()));
    let outer = hex(&md5::Md5::digest([inner.as_bytes(), &salt].concat()));
    format!("md5{outer}")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// PBKDF2-HMAC-SHA-256 with one output block (SCRAM's `Hi`).
fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut u = hmac(password, &[salt, &1u32.to_be_bytes()].concat());
    let mut out = u;
    for _ in 1..iterations {
        u = hmac(password, &u);
        for (o, x) in out.iter_mut().zip(u) {
            *o ^= x;
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn b64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Decode standard base64 (padding optional); `None` if malformed.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The `tls-server-end-point` channel-binding data (RFC 5929): the hash of
/// the server certificate, with the certificate's own signature hash — or
/// SHA-256 when that is MD5 / SHA-1 (or unknown).
pub fn tls_server_end_point(cert_der: &[u8]) -> Vec<u8> {
    match signature_hash(cert_der) {
        Some(384) => Sha384::digest(cert_der).to_vec(),
        Some(512) => Sha512::digest(cert_der).to_vec(),
        _ => Sha256::digest(cert_der).to_vec(),
    }
}

/// The SHA-2 size named by the certificate's `signatureAlgorithm`.
fn signature_hash(der: &[u8]) -> Option<u16> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm SEQUENCE { OID, … }, … }
    let (_, cert) = der_item(der, 0x30)?;
    let (rest, _tbs) = der_skip(cert)?;
    let (_, alg) = der_item(rest, 0x30)?;
    let (_, oid) = der_item(alg, 0x06)?;
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01]; // 1.2.840.113549.1.1
    const ECDSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03]; // 1.2.840.10045.4.3
    match oid {
        [p @ .., 12] if p == RSA => Some(384),
        [p @ .., 13] if p == RSA => Some(512),
        [p @ .., 3] if p == ECDSA => Some(384),
        [p @ .., 4] if p == ECDSA => Some(512),
        _ => Some(256),
    }
}

/// The content of the DER item at the start of `b`, which must have `tag`;
/// returns (what follows, content).
fn der_item(b: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    if *b.first()? != tag {
        return None;
    }
    der_split(b)
}

fn der_skip(b: &[u8]) -> Option<(&[u8], &[u8])> {
    der_split(b)
}

fn der_split(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let first = *b.get(1)?;
    let (len, hdr) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let content = b.get(hdr..hdr + len)?;
    Some((&b[hdr + len..], content))
}

/// One SCRAM-SHA-256 exchange, client side.
pub struct Scram {
    password: Vec<u8>,
    nonce: String,
    /// The `gs2` header: `n,,`, `y,,` or `p=tls-server-end-point,,`.
    gs2: String,
    binding: Vec<u8>,
    client_first_bare: String,
    server_signature: Option<[u8; 32]>,
}

/// What [`Scram::new`] should do about channel binding.
pub enum Binding {
    /// The server offered no `-PLUS` mechanism (or we chose not to): `n`.
    None,
    /// The client could bind but the server did not offer it: `y` (a
    /// downgrade then shows up on the server).
    Supported,
    /// Bind to this `tls-server-end-point` data: `p=tls-server-end-point`.
    Bind(Vec<u8>),
}

impl Scram {
    /// Start an exchange: `nonce` are the client nonce characters (printable,
    /// no comma). The password is used as given: SASLprep only changes
    /// non-ASCII passwords, and the server falls back to the raw bytes when a
    /// password does not survive it.
    pub fn new(password: &str, nonce: String, binding: Binding) -> Self {
        let (gs2, binding) = match binding {
            Binding::None => ("n,,".to_string(), Vec::new()),
            Binding::Supported => ("y,,".to_string(), Vec::new()),
            Binding::Bind(d) => ("p=tls-server-end-point,,".to_string(), d),
        };
        // The user name goes in the start-up message; SCRAM's is left empty.
        let client_first_bare = format!("n=,r={nonce}");
        Self { password: password.as_bytes().to_vec(), nonce, gs2, binding, client_first_bare, server_signature: None }
    }

    /// The mechanism name for `SASLInitialResponse`.
    pub fn mechanism(&self) -> &'static str {
        if self.binding.is_empty() {
            "SCRAM-SHA-256"
        } else {
            "SCRAM-SHA-256-PLUS"
        }
    }

    /// `client-first-message`.
    pub fn client_first(&self) -> String {
        format!("{}{}", self.gs2, self.client_first_bare)
    }

    /// Answer `server-first-message` with `client-final-message`.
    pub fn client_final(&mut self, server_first: &str) -> Result<String, String> {
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for attr in server_first.split(',') {
            match attr.split_at_checked(2) {
                Some(("r=", v)) => nonce = Some(v),
                Some(("s=", v)) => salt = Some(b64_decode(v).ok_or("bad salt")?),
                Some(("i=", v)) => iterations = Some(v.parse::<u32>().map_err(|_| "bad iteration count")?),
                Some(("m=", _)) => return Err("server requires an unsupported SCRAM extension".into()),
                _ => {}
            }
        }
        let (nonce, salt, iterations) = match (nonce, salt, iterations) {
            (Some(n), Some(s), Some(i)) if i > 0 => (n, s, i),
            _ => return Err(format!("malformed server-first-message {server_first:?}")),
        };
        if !nonce.starts_with(&self.nonce) || nonce.len() == self.nonce.len() {
            return Err("server nonce does not extend the client nonce".into());
        }
        let salted = hi(&self.password, &salt, iterations);
        let client_key = hmac(&salted, b"Client Key");
        let stored_key: [u8; 32] = Sha256::digest(client_key).into();
        let channel = b64_encode(&[self.gs2.as_bytes(), &self.binding].concat());
        let without_proof = format!("c={channel},r={nonce}");
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);
        let signature = hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(k, s)| k ^ s).collect();
        let server_key = hmac(&salted, b"Server Key");
        self.server_signature = Some(hmac(&server_key, auth_message.as_bytes()));
        Ok(format!("{without_proof},p={}", b64_encode(&proof)))
    }

    /// Check `server-final-message`: the server proves it knew the password too.
    pub fn verify_server(&self, server_final: &str) -> Result<(), String> {
        if let Some(e) = server_final.strip_prefix("e=") {
            return Err(format!("server rejected SCRAM: {e}"));
        }
        let v = server_final
            .split(',')
            .find_map(|a| a.strip_prefix("v="))
            .and_then(b64_decode)
            .ok_or_else(|| format!("malformed server-final-message {server_final:?}"))?;
        match self.server_signature {
            Some(s) if v == s => Ok(()),
            _ => Err("server signature does not match: the server did not prove it knows the password".into()),
        }
    }
}

/// A fresh client nonce: 18 random bytes, base64.
pub fn new_nonce() -> std::io::Result<String> {
    let mut b = [0u8; 18];
    kernway_tls::fill_random(&mut b)?;
    Ok(b64_encode(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7677_example() {
        // RFC 7677 §3 (user "user", password "pencil").
        let mut s = Scram::new("pencil", "rOprNGfwEbeRWgbNEkqO".into(), Binding::None);
        // The RFC's messages carry n=user; ours an empty name, so redo the
        // example with the RFC's bare message to check the arithmetic.
        s.client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO".into();
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let fin = s.client_final(server_first).unwrap();
        assert_eq!(fin, "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        s.verify_server("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
        assert!(s.verify_server("v=AAAA").is_err());
        assert!(s.verify_server("e=invalid-proof").is_err());
    }

    #[test]
    fn nonce_must_extend_ours() {
        let mut s = Scram::new("pw", "abc".into(), Binding::None);
        assert!(s.client_final("r=xyz123,s=QSXCR+Q6sek8bf92,i=4096").is_err());
        assert!(s.client_final("r=abc,s=QSXCR+Q6sek8bf92,i=4096").is_err());
        assert!(s.client_final("r=abcd,s=QSXCR+Q6sek8bf92").is_err());
    }

    #[test]
    fn md5_matches_postgres() {
        // SELECT 'md5' || md5(md5('secret' || 'alice') || 'abcd')
        assert_eq!(md5_password("alice", "secret", *b"abcd"), "md5c90bcafc0a2b54511742383e34d5dc9d");
    }

    #[test]
    fn base64() {
        for (raw, enc) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foobar", "Zm9vYmFy")] {
            assert_eq!(b64_encode(raw), enc);
            assert_eq!(b64_decode(enc).unwrap(), raw);
        }
        assert!(b64_decode("a*b").is_none());
    }

    #[test]
    fn hmac_rfc4231() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
