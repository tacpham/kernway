//! SCRAM client (RFC 5802; SHA-256 per RFC 7677), shared by the drivers
//! that authenticate with it — PostgreSQL (SCRAM-SHA-256, `-PLUS` channel
//! binding), MongoDB (SCRAM-SHA-1 and SCRAM-SHA-256) and Kafka
//! (SCRAM-SHA-256 and SCRAM-SHA-512). Each driver frames
//! the messages its own way; the exchange and its arithmetic live here.
//!
//! HMAC, PBKDF2 (`Hi`) and base64 are written out (a few lines each); the
//! hash functions come from `sha1` / `sha2`.

/// A hash SCRAM can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    /// SHA-1 (SCRAM-SHA-1, MongoDB's legacy mechanism).
    Sha1,
    /// SHA-256 (SCRAM-SHA-256).
    Sha256,
    /// SHA-512 (SCRAM-SHA-512, Kafka).
    Sha512,
}

impl Hash {
    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            Hash::Sha1 => {
                use sha1::Digest;
                let mut h = sha1::Sha1::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
            Hash::Sha256 => {
                use sha2::Digest;
                let mut h = sha2::Sha256::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
            Hash::Sha512 => {
                use sha2::Digest;
                let mut h = sha2::Sha512::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
        }
    }

    /// The hash's block size (HMAC pads the key to it).
    fn block(self) -> usize {
        match self {
            Hash::Sha1 | Hash::Sha256 => 64,
            Hash::Sha512 => 128,
        }
    }

    /// HMAC (RFC 2104) with this hash.
    pub fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        let block = self.block();
        let mut k = vec![0u8; block];
        if key.len() > block {
            let d = self.digest(&[key]);
            k[..d.len()].copy_from_slice(&d);
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
        let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
        let inner = self.digest(&[&ipad, data]);
        self.digest(&[&opad, &inner])
    }

    /// PBKDF2-HMAC with one output block (SCRAM's `Hi`).
    pub fn hi(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut u = self.hmac(password, &[salt, &1u32.to_be_bytes()].concat());
        let mut out = u.clone();
        for _ in 1..iterations {
            u = self.hmac(password, &u);
            for (o, x) in out.iter_mut().zip(&u) {
                *o ^= x;
            }
        }
        out
    }

    /// The plain hash.
    pub fn hash(self, data: &[u8]) -> Vec<u8> {
        self.digest(&[data])
    }
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

/// Channel binding for the `gs2` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// No binding (`n`): no TLS, or the mechanism has none.
    None,
    /// The client could bind but the server offered no `-PLUS` (`y`).
    Supported,
    /// Bind to this `tls-server-end-point` data (`p=tls-server-end-point`).
    Bind(Vec<u8>),
}

/// One SCRAM exchange, client side.
pub struct Scram {
    hash: Hash,
    password: Vec<u8>,
    nonce: String,
    gs2: String,
    binding: Vec<u8>,
    client_first_bare: String,
    server_signature: Option<Vec<u8>>,
    salted: Option<Vec<u8>>,
}

/// `=` → `=3D`, `,` → `=2C` (SCRAM `saslname`).
fn escape_name(user: &str) -> String {
    user.replace('=', "=3D").replace(',', "=2C")
}

impl Scram {
    /// Start an exchange as `user` (empty for PostgreSQL, which takes the
    /// user from the start-up message) with the already-prepared password
    /// bytes (MongoDB SCRAM-SHA-1 hashes the password first; SCRAM-SHA-256
    /// takes its [`saslprep`](crate::saslprep::saslprep) form).
    /// `nonce`: printable client nonce without commas ([`new_nonce`]).
    pub fn new(hash: Hash, user: &str, password: &[u8], nonce: String, binding: Binding) -> Self {
        let (gs2, binding) = match binding {
            Binding::None => ("n,,".to_string(), Vec::new()),
            Binding::Supported => ("y,,".to_string(), Vec::new()),
            Binding::Bind(d) => ("p=tls-server-end-point,,".to_string(), d),
        };
        let client_first_bare = format!("n={},r={nonce}", escape_name(user));
        Self { hash, password: password.to_vec(), nonce, gs2, binding, client_first_bare, server_signature: None, salted: None }
    }

    /// Whether the exchange binds to the TLS channel (the `-PLUS` mechanism).
    pub fn is_bound(&self) -> bool {
        !self.binding.is_empty()
    }

    /// `client-first-message`.
    pub fn client_first(&self) -> String {
        format!("{}{}", self.gs2, self.client_first_bare)
    }

    /// Answer `server-first-message` with `client-final-message`.
    pub fn client_final(&mut self, server_first: &str) -> Result<String, String> {
        self.client_final_cached(server_first, None)
    }

    /// [`client_final`](Self::client_final), reusing a salted password
    /// computed earlier for the same (salt, iterations) — PBKDF2 is the
    /// expensive part (MongoDB drivers cache it per connection string).
    pub fn client_final_cached(&mut self, server_first: &str, cached: Option<(&[u8], u32, &[u8])>) -> Result<String, String> {
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
        let salted = match cached {
            Some((s, i, salted)) if s == salt.as_slice() && i == iterations => salted.to_vec(),
            _ => self.hash.hi(&self.password, &salt, iterations),
        };
        let client_key = self.hash.hmac(&salted, b"Client Key");
        let stored_key = self.hash.hash(&client_key);
        let channel = b64_encode(&[self.gs2.as_bytes(), &self.binding].concat());
        let without_proof = format!("c={channel},r={nonce}");
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);
        let signature = self.hash.hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&signature).map(|(k, s)| k ^ s).collect();
        let server_key = self.hash.hmac(&salted, b"Server Key");
        self.server_signature = Some(self.hash.hmac(&server_key, auth_message.as_bytes()));
        self.salted = Some(salted);
        Ok(format!("{without_proof},p={}", b64_encode(&proof)))
    }

    /// The salted password of the last [`client_final`](Self::client_final).
    pub fn salted_password(&self) -> Option<&[u8]> {
        self.salted.as_deref()
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
        match &self.server_signature {
            Some(s) if &v == s => Ok(()),
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

/// The `tls-server-end-point` channel-binding data (RFC 5929): the hash of
/// the server certificate, with the certificate's own signature hash — or
/// SHA-256 when that is MD5 / SHA-1 (or unknown).
pub fn tls_server_end_point(cert_der: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    match signature_hash(cert_der) {
        384 => sha2::Sha384::digest(cert_der).to_vec(),
        512 => sha2::Sha512::digest(cert_der).to_vec(),
        _ => sha2::Sha256::digest(cert_der).to_vec(),
    }
}

/// The SHA-2 size named by the certificate's `signatureAlgorithm` (256 by default).
fn signature_hash(der: &[u8]) -> u16 {
    fn split(b: &[u8], tag: Option<u8>) -> Option<(&[u8], &[u8])> {
        if let Some(t) = tag {
            if *b.first()? != t {
                return None;
            }
        }
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
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm SEQUENCE { OID, … }, … }
    let oid = (|| {
        let (_, cert) = split(der, Some(0x30))?;
        let (rest, _tbs) = split(cert, None)?;
        let (_, alg) = split(rest, Some(0x30))?;
        let (_, oid) = split(alg, Some(0x06))?;
        Some(oid)
    })();
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01]; // 1.2.840.113549.1.1
    const ECDSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03]; // 1.2.840.10045.4.3
    match oid {
        Some([p @ .., 12]) if p == RSA => 384,
        Some([p @ .., 13]) if p == RSA => 512,
        Some([p @ .., 3]) if p == ECDSA => 384,
        Some([p @ .., 4]) if p == ECDSA => 512,
        _ => 256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc5802_sha1_example() {
        // RFC 5802 §5 (user "user", password "pencil").
        let mut s = Scram::new(Hash::Sha1, "user", b"pencil", "fyko+d2lbbFgONRv9qkxdawL".into(), Binding::None);
        assert_eq!(s.client_first(), "n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL");
        let fin = s.client_final("r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
        assert_eq!(fin, "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=");
        s.verify_server("v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").unwrap();
    }

    #[test]
    fn rfc7677_sha256_example() {
        let mut s = Scram::new(Hash::Sha256, "user", b"pencil", "rOprNGfwEbeRWgbNEkqO".into(), Binding::None);
        let fin = s.client_final("r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
        assert_eq!(fin, "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        s.verify_server("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
        assert!(s.verify_server("v=AAAA").is_err());
        assert!(s.verify_server("e=invalid-proof").is_err());
        // The salted password can be reused for the same salt / iterations.
        let salted = s.salted_password().unwrap().to_vec();
        let mut again = Scram::new(Hash::Sha256, "user", b"pencil", "rOprNGfwEbeRWgbNEkqO".into(), Binding::None);
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let fin2 = again
            .client_final_cached("r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096", Some((&salt, 4096, &salted)))
            .unwrap();
        assert_eq!(fin, fin2);
    }

    #[test]
    fn names_are_escaped_and_nonces_checked() {
        assert_eq!(Scram::new(Hash::Sha256, "a=b,c", b"", "n".into(), Binding::None).client_first(), "n,,n=a=3Db=2Cc,r=n");
        let mut s = Scram::new(Hash::Sha256, "", b"pw", "abc".into(), Binding::None);
        assert!(s.client_final("r=xyz123,s=QSXCR+Q6sek8bf92,i=4096").is_err());
        assert!(s.client_final("r=abc,s=QSXCR+Q6sek8bf92,i=4096").is_err());
        assert!(s.client_final("r=abcd,s=QSXCR+Q6sek8bf92").is_err());
        assert!(s.client_final("r=abcd,s=QSXCR+Q6sek8bf92,i=1,m=ext").is_err());
        let b = Scram::new(Hash::Sha256, "", b"pw", "n".into(), Binding::Bind(vec![1, 2]));
        assert!(b.is_bound());
        assert!(b.client_first().starts_with("p=tls-server-end-point,,"));
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test cases 1 and 6 (a key longer than the block).
        let hex = |v: Vec<u8>| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let key = [0x0b; 20];
        assert_eq!(hex(Hash::Sha256.hmac(&key, b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(
            hex(Hash::Sha512.hmac(&key, b"Hi There")),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
        let long = [0xaa; 131];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        assert_eq!(hex(Hash::Sha256.hmac(&long, data)), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
        assert_eq!(
            hex(Hash::Sha512.hmac(&long, data)),
            "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f3526b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598"
        );
    }

    #[test]
    fn primitives() {
        // RFC 4231 test case 2; RFC 2202 test case 2.
        assert_eq!(hex(&Hash::Sha256.hmac(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(hex(&Hash::Sha1.hmac(b"Jefe", b"what do ya want for nothing?")), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        for (raw, enc) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foobar", "Zm9vYmFy")] {
            assert_eq!(b64_encode(raw), enc);
            assert_eq!(b64_decode(enc).unwrap(), raw);
        }
        assert!(b64_decode("a*b").is_none());
        assert_eq!(tls_server_end_point(b"not a certificate").len(), 32, "unknown: SHA-256");
    }
}
