//! User private keys: OpenSSH's own format (`-----BEGIN OPENSSH PRIVATE KEY-----`,
//! what `ssh-keygen` writes), PKCS#1 RSA and PKCS#8 PEM; and signing with them for
//! public-key authentication (RFC 4252 §7).

use crate::hostkey::{Curve, PublicKey};
use crate::wire::{Reader, Writer};
use crate::SshError;
use kernway_wire::base64;
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RsaKeyPair};
use std::fmt;
use std::sync::Arc;

/// A private key for public-key authentication.
#[derive(Clone)]
pub struct PrivateKey {
    inner: Arc<Inner>,
    public: PublicKey,
}

enum Inner {
    Ed25519(Ed25519KeyPair),
    Ecdsa(Curve, EcdsaKeyPair),
    Rsa(RsaKeyPair),
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PrivateKey({} {})", self.public.type_name(), self.public.fingerprint())
    }
}

fn key_err(m: impl Into<String>) -> SshError {
    SshError::Key(m.into())
}

impl PrivateKey {
    /// Read a private key file's contents: OpenSSH format, `BEGIN RSA PRIVATE KEY`
    /// (PKCS#1) or `BEGIN PRIVATE KEY` (PKCS#8). Passphrase-protected keys are refused
    /// with an error saying so (decrypt them with `ssh-keygen -p -N ''`, or keep them
    /// in an agent).
    pub fn from_pem(text: &[u8]) -> Result<Self, SshError> {
        let text = std::str::from_utf8(text).map_err(|_| key_err("the key file is not text (PEM)"))?;
        let (label, body) = pem_body(text)?;
        match label.as_str() {
            "OPENSSH PRIVATE KEY" => Self::from_openssh_blob(&body),
            "RSA PRIVATE KEY" => {
                let pair = RsaKeyPair::from_der(&body).map_err(|e| key_err(format!("PKCS#1 RSA key: {e}")))?;
                Ok(Self::from_rsa(pair))
            }
            "PRIVATE KEY" => Self::from_pkcs8(&body),
            "ENCRYPTED PRIVATE KEY" => Err(key_err("an encrypted PKCS#8 key: passphrase-protected keys are not supported")),
            "EC PRIVATE KEY" => Err(key_err("a SEC1 EC key: convert it with `ssh-keygen -p -m PKCS8` or use the OpenSSH format")),
            other => Err(key_err(format!("not a private key (PEM label {other:?})"))),
        }
    }

    /// The public half (what goes in `authorized_keys`).
    #[must_use]
    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    fn from_rsa(pair: RsaKeyPair) -> Self {
        let c = pair.public().into();
        let signature::RsaPublicKeyComponents::<Vec<u8>> { n, e } = c;
        Self { public: PublicKey::Rsa { e: strip(&e).to_vec(), n: strip(&n).to_vec() }, inner: Arc::new(Inner::Rsa(pair)) }
    }

    fn from_pkcs8(der: &[u8]) -> Result<Self, SshError> {
        let rng = SystemRandom::new();
        if let Ok(pair) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            let public = PublicKey::Ed25519(pair.public_key().as_ref().try_into().expect("32 bytes"));
            return Ok(Self { public, inner: Arc::new(Inner::Ed25519(pair)) });
        }
        for (curve, alg) in [(Curve::P256, &signature::ECDSA_P256_SHA256_FIXED_SIGNING), (Curve::P384, &signature::ECDSA_P384_SHA384_FIXED_SIGNING)] {
            if let Ok(pair) = EcdsaKeyPair::from_pkcs8(alg, der, &rng) {
                let public = PublicKey::Ecdsa { curve, point: pair.public_key().as_ref().to_vec() };
                return Ok(Self { public, inner: Arc::new(Inner::Ecdsa(curve, pair)) });
            }
        }
        let pair = RsaKeyPair::from_pkcs8(der).map_err(|e| key_err(format!("PKCS#8 key: {e}")))?;
        Ok(Self::from_rsa(pair))
    }

    /// `openssh-key-v1` (PROTOCOL.key in OpenSSH).
    fn from_openssh_blob(b: &[u8]) -> Result<Self, SshError> {
        let body = b.strip_prefix(b"openssh-key-v1\0").ok_or_else(|| key_err("not an openssh-key-v1 file"))?;
        let mut r = Reader::new(body);
        let cipher = r.string()?;
        let _kdf = r.string()?;
        let _kdf_options = r.string()?;
        if cipher != b"none" {
            return Err(key_err(format!(
                "the key is passphrase-protected ({}); passphrase-protected keys are not supported — remove the passphrase with `ssh-keygen -p -N ''` or use password authentication",
                String::from_utf8_lossy(cipher)
            )));
        }
        if r.u32()? != 1 {
            return Err(key_err("a key file with more than one key"));
        }
        let public_blob = r.string()?;
        let mut p = Reader::new(r.string()?);
        if p.u32()? != p.u32()? {
            return Err(key_err("the key's check numbers differ (a corrupt file)"));
        }
        let kind = p.string()?;
        let key = match kind {
            b"ssh-ed25519" => {
                let public = p.string()?;
                let secret = p.string()?;
                if secret.len() != 64 || public.len() != 32 || &secret[32..] != public {
                    return Err(key_err("a malformed Ed25519 key"));
                }
                let pair = Ed25519KeyPair::from_seed_and_public_key(&secret[..32], public).map_err(|e| key_err(format!("Ed25519 key: {e}")))?;
                Self { public: PublicKey::Ed25519(public.try_into().expect("32 bytes")), inner: Arc::new(Inner::Ed25519(pair)) }
            }
            b"ecdsa-sha2-nistp256" | b"ecdsa-sha2-nistp384" => {
                let (curve, alg) = if kind == b"ecdsa-sha2-nistp256" {
                    (Curve::P256, &signature::ECDSA_P256_SHA256_FIXED_SIGNING)
                } else {
                    (Curve::P384, &signature::ECDSA_P384_SHA384_FIXED_SIGNING)
                };
                let _curve_name = p.string()?;
                let point = p.string()?;
                let d = p.mpint()?;
                let n = if curve == Curve::P256 { 32 } else { 48 };
                if d.len() > n {
                    return Err(key_err("a malformed ECDSA key"));
                }
                let mut fixed = vec![0u8; n];
                fixed[n - d.len()..].copy_from_slice(d);
                let pair = EcdsaKeyPair::from_private_key_and_public_key(alg, &fixed, point, &SystemRandom::new()).map_err(|e| key_err(format!("ECDSA key: {e}")))?;
                Self { public: PublicKey::Ecdsa { curve, point: point.to_vec() }, inner: Arc::new(Inner::Ecdsa(curve, pair)) }
            }
            b"ssh-rsa" => {
                let (n, e, d, iqmp, p_, q) = (p.mpint()?, p.mpint()?, p.mpint()?, p.mpint()?, p.mpint()?, p.mpint()?);
                let dp = big_mod(d, &minus_one(p_));
                let dq = big_mod(d, &minus_one(q));
                let pair = RsaKeyPair::from_components(&ring::rsa::KeyPairComponents {
                    public_key: ring::rsa::PublicKeyComponents { n, e },
                    d,
                    p: p_,
                    q,
                    dP: dp.as_slice(),
                    dQ: dq.as_slice(),
                    qInv: iqmp,
                })
                .map_err(|e| key_err(format!("RSA key: {e}")))?;
                Self::from_rsa(pair)
            }
            other => return Err(key_err(format!("unsupported key type {}", String::from_utf8_lossy(other)))),
        };
        if key.public.to_blob() != public_blob {
            return Err(key_err("the public and private halves of the key do not match"));
        }
        Ok(key)
    }

    /// The signature algorithm to use, given what the server said it accepts
    /// (`server-sig-algs`; `None` when it did not say).
    pub(crate) fn algorithm(&self, server_sig_algs: Option<&[String]>) -> &'static str {
        let ours = self.public.algorithms();
        match server_sig_algs {
            Some(theirs) => ours.iter().copied().find(|a| theirs.iter().any(|t| t == a)).unwrap_or(ours[0]),
            None if matches!(self.public, PublicKey::Rsa { .. }) => "rsa-sha2-256",
            None => ours[0],
        }
    }

    /// Sign `data` with `alg`, in the wire form (`string alg, string signature`).
    pub(crate) fn sign(&self, alg: &str, data: &[u8]) -> Result<Vec<u8>, SshError> {
        let rng = SystemRandom::new();
        let raw = match &*self.inner {
            Inner::Ed25519(pair) => pair.sign(data).as_ref().to_vec(),
            Inner::Ecdsa(curve, pair) => {
                let sig = pair.sign(&rng, data).map_err(|_| key_err("ECDSA signing failed"))?;
                let half = if *curve == Curve::P256 { 32 } else { 48 };
                let mut w = Writer::new();
                w.mpint(&sig.as_ref()[..half]).mpint(&sig.as_ref()[half..]);
                w.into_inner()
            }
            Inner::Rsa(pair) => {
                let enc: &'static dyn signature::RsaEncoding = if alg == "rsa-sha2-512" { &signature::RSA_PKCS1_SHA512 } else { &signature::RSA_PKCS1_SHA256 };
                let mut sig = vec![0u8; pair.public().modulus_len()];
                pair.sign(enc, &rng, data, &mut sig).map_err(|_| key_err("RSA signing failed"))?;
                sig
            }
        };
        let mut w = Writer::new();
        w.string(alg).string(raw);
        Ok(w.into_inner())
    }
}

/// The label and the decoded body of the first PEM block.
fn pem_body(text: &str) -> Result<(String, Vec<u8>), SshError> {
    let start = text.find("-----BEGIN ").ok_or_else(|| key_err("no -----BEGIN line (not a PEM key file)"))?;
    let rest = &text[start + 11..];
    let label_end = rest.find("-----").ok_or_else(|| key_err("a malformed BEGIN line"))?;
    let label = rest[..label_end].to_owned();
    let body_start = &rest[label_end + 5..];
    let end = body_start.find("-----END ").ok_or_else(|| key_err("no -----END line"))?;
    let body = &body_start[..end];
    if body.contains("Proc-Type:") {
        return Err(key_err("a passphrase-protected PEM key: passphrase-protected keys are not supported"));
    }
    let der = base64::decode(body).ok_or_else(|| key_err("bad base64 in the key file"))?;
    Ok((label, der))
}

fn strip(b: &[u8]) -> &[u8] {
    &b[b.iter().position(|&x| x != 0).unwrap_or(b.len())..]
}

/// `x - 1` for a positive big-endian integer.
fn minus_one(x: &[u8]) -> Vec<u8> {
    let mut v = x.to_vec();
    for b in v.iter_mut().rev() {
        if *b == 0 {
            *b = 0xff;
        } else {
            *b -= 1;
            break;
        }
    }
    v
}

/// `a mod m` for big-endian magnitudes (shift-and-subtract, one bit at a time: run
/// once per key load, so simplicity wins over speed).
fn big_mod(a: &[u8], m: &[u8]) -> Vec<u8> {
    let m = strip(m);
    let mut r: Vec<u8> = vec![0; m.len() + 1];
    let ge = |r: &[u8]| {
        // r (len m.len()+1) >= m ?
        let pad = r.len() - m.len();
        if r[..pad].iter().any(|&x| x != 0) {
            return true;
        }
        r[pad..] >= *m
    };
    for &byte in a {
        for bit in (0..8).rev() {
            // r = r * 2 + bit
            let mut carry = (byte >> bit) & 1;
            for x in r.iter_mut().rev() {
                let next = *x >> 7;
                *x = (*x << 1) | carry;
                carry = next;
            }
            if ge(&r) {
                let pad = r.len() - m.len();
                let mut borrow = 0i16;
                for i in (0..r.len()).rev() {
                    let sub = if i >= pad { i16::from(m[i - pad]) } else { 0 };
                    let mut v = i16::from(r[i]) - sub - borrow;
                    borrow = if v < 0 {
                        v += 256;
                        1
                    } else {
                        0
                    };
                    r[i] = v as u8;
                }
            }
        }
    }
    strip(&r).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn big_mod_and_minus_one() {
        assert_eq!(big_mod(&[0x01, 0x00], &[0x07]), [0x04], "256 mod 7");
        assert!(big_mod(&[0xff, 0xff, 0xff, 0xff], &[0x01, 0x00, 0x01]).is_empty(), "2^32-1 = 65537 * 65535");
        assert_eq!(big_mod(&[0xff, 0xff, 0xff, 0xff], &[0xff, 0xf1]), [0xe0], "2^32-1 mod 65521 = 224");
        // A 3072-bit number modulo a 1536-bit one (an RSA d mod p-1), checked against Python.
        assert_eq!(big_mod(&hex("57ee05cde00902c77ebff206867347214cdd2055930d6eaf14f4733f3e7d1bfbc7a2ea20b2f14c942e05319acb5c74273f98e2774cbd87ad5c90a9587403e430ec66a78795e761d17731af10506bf2efc6f877186d76b07e881ed162ae2eb1547f15052434b9b5df9e7769b10f4205b4907a70c31012f037b64ce4228c38fb2918f135d25f557203301850c5a38fd547923a736994e3bf911a61dbe22e44158bae97ba94d0eda82f8f6d05584ef8aa38922766581e27a1c08a6a63ec24ede6a46b4cb2424a23d5962217beaddbc496cb8e81973e0becd7b03898d190f9ebdacc0cb1e29c658cda1495e60af593bd04cf0fd630f1f29d0da9953f48f1a09f76b5a170b33839263059f28c105d1fb17c2390c192cfd3ac94af0f21ddb66cad4a268d116ece1738f7d93d9c172411e20b8f6b0d549b6f03675a1600a35a099950d836f675cc81e74ef5e8e25d940ed904759531985d5d9dc9f81818e811892f902bd23f0824128b2f330c5c7fd0a6a3a4506513270e269e0d37f2a74de452e6b438"), &hex("cf426dcbb394fb36bb2d420f0f88080b10a3d6b2aa05e11ab2715945795e8229451abd81f1d69ed617f5e837d70820fe119a72d174c9df6acc011cdd9474031b7f26144b98289fcd59a54a7bb1fee08f571242425051c1ccd17f9acae01f5057ca02135e92b1d3f28ede0d7ac3baea9e13deef86ab1031d0f646e1f40a097c976bf46c697d2caf82eeeacbe226e875555790f82ec1d3fcff2a3af4d46b0a18e8830e07bc1e398f1012bd4acefaecbd389be4bcfc49b64a0872e6cc3ababced20")), hex("41dec4c974cd3fe415e6c87fa1e26e62924c21c2bb74f75b54bebcbbeac93781a27b461e7115b90d34f3bb337b6801de17929ffc6e5d46b06c053bf8c165c3c4409e7af74f8a14e8e8ae051439703e3668f112b1705f3f5c976ec46768107d93f51ab93d67016649c911e16ffd32fb3af62e5be0cc231b49f3aab598ae9952773dcbdf9399bfc07374e93c4fd079fe088de06d3fe91bb3d286bf9be09cbf71dfb497662c3bf480136dd7a3a8f01fe06661ab30482df4e7fd03bd6aec6327aaf8"));
        assert!(big_mod(&[0x05], &[0x05]).is_empty(), "zero");
        assert_eq!(big_mod(&[0x03], &[0x05]), [0x03]);
        assert_eq!(minus_one(&[0x01, 0x00]), [0x00, 0xff]);
    }

    #[test]
    fn passphrases_and_junk_are_refused_with_a_reason() {
        assert!(PrivateKey::from_pem(b"hello").unwrap_err().to_string().contains("BEGIN"));
        let encrypted_pkcs1 = b"-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nAAAA\n-----END RSA PRIVATE KEY-----\n";
        assert!(PrivateKey::from_pem(encrypted_pkcs1).unwrap_err().to_string().contains("passphrase"));
    }

    #[test]
    fn a_generated_ed25519_key_signs_verifiably() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::encode(pkcs8.as_ref()));
        let k = PrivateKey::from_pem(pem.as_bytes()).unwrap();
        let sig = k.sign("ssh-ed25519", b"data").unwrap();
        k.public_key().verify("ssh-ed25519", b"data", &sig).unwrap();
    }
}
