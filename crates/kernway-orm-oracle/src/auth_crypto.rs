//! O5LOGON authentication crypto (11g and 12c password verifiers).
//!
//! Follows python-oracledb's thin `auth.pyx` / `crypto.pyx`: AES-CBC with an
//! all-zero IV and no padding removal on decrypt, PKCS#7-style padding that
//! always adds at least one byte on encrypt, PBKDF2-HMAC-SHA512 and SHA-1/512.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use std::num::NonZeroU32;

/// 11g verifier types.
pub(crate) const VERIFIER_11G_1: u32 = 0xB152;
pub(crate) const VERIFIER_11G_2: u32 = 0x1B25;
/// 12c verifier type.
pub(crate) const VERIFIER_12C: u32 = 0x4815;

/// Upper-case hex.
pub(crate) fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Decode hex (either case).
pub(crate) fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

enum Aes {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Aes {
    fn new(key: &[u8]) -> Result<Self, String> {
        Ok(match key.len() {
            16 => Aes::A128(aes::Aes128::new_from_slice(key).map_err(|e| e.to_string())?),
            24 => Aes::A192(aes::Aes192::new_from_slice(key).map_err(|e| e.to_string())?),
            32 => Aes::A256(aes::Aes256::new_from_slice(key).map_err(|e| e.to_string())?),
            n => return Err(format!("unsupported AES key length {n}")),
        })
    }
    fn encrypt(&self, block: &mut [u8; 16]) {
        let b = aes::Block::from_mut_slice(block);
        match self {
            Aes::A128(c) => c.encrypt_block(b),
            Aes::A192(c) => c.encrypt_block(b),
            Aes::A256(c) => c.encrypt_block(b),
        }
    }
    fn decrypt(&self, block: &mut [u8; 16]) {
        let b = aes::Block::from_mut_slice(block);
        match self {
            Aes::A128(c) => c.decrypt_block(b),
            Aes::A192(c) => c.decrypt_block(b),
            Aes::A256(c) => c.decrypt_block(b),
        }
    }
}

/// AES-CBC encrypt with a zero IV, padding `n = 16 - len % 16` bytes of value `n`.
pub(crate) fn encrypt_cbc(key: &[u8], plain: &[u8]) -> Result<Vec<u8>, String> {
    let aes = Aes::new(key)?;
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut prev = [0u8; 16];
    for chunk in data.chunks_mut(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        aes.encrypt(&mut block);
        chunk.copy_from_slice(&block);
        prev = block;
    }
    Ok(data)
}

/// AES-CBC decrypt with a zero IV; padding is left in place.
pub(crate) fn decrypt_cbc(key: &[u8], cipher: &[u8]) -> Result<Vec<u8>, String> {
    if !cipher.len().is_multiple_of(16) {
        return Err("ciphertext is not a whole number of blocks".into());
    }
    let aes = Aes::new(key)?;
    let mut out = Vec::with_capacity(cipher.len());
    let mut prev = [0u8; 16];
    for chunk in cipher.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let saved = block;
        aes.decrypt(&mut block);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = saved;
    }
    Ok(out)
}

fn pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: u32, out_len: usize) -> Result<Vec<u8>, String> {
    let iters = NonZeroU32::new(iterations).ok_or("PBKDF2 iteration count is zero")?;
    let mut out = vec![0u8; out_len];
    ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA512, iters, salt, password, &mut out);
    Ok(out)
}

fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA512);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().to_vec()
}

fn sha1(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().to_vec()
}

fn md5(b: &[u8]) -> [u8; 16] {
    use md5::Digest;
    md5::Md5::digest(b).into()
}

/// Random bytes from the OS.
pub(crate) fn random_bytes(n: usize) -> Vec<u8> {
    use ring::rand::SecureRandom;
    let mut out = vec![0u8; n];
    ring::rand::SystemRandom::new()
        .fill(&mut out)
        .expect("system randomness available");
    out
}

/// What phase one of the server's auth reply provides.
pub(crate) struct ServerChallenge<'a> {
    pub(crate) verifier_type: u32,
    /// AUTH_VFR_DATA (hex).
    pub(crate) vfr_data: &'a str,
    /// AUTH_SESSKEY (hex).
    pub(crate) sesskey: &'a str,
    /// AUTH_PBKDF2_VGEN_COUNT (12c).
    pub(crate) vgen_count: Option<u32>,
    /// AUTH_PBKDF2_SDER_COUNT (12c combine).
    pub(crate) sder_count: Option<u32>,
    /// AUTH_PBKDF2_CSK_SALT (hex, 12c combine).
    pub(crate) csk_salt: Option<&'a str>,
}

/// The client's answer for auth phase two.
pub(crate) struct ClientProof {
    /// AUTH_SESSKEY to send.
    pub(crate) sesskey: String,
    /// AUTH_PBKDF2_SPEEDY_KEY (12c only).
    pub(crate) speedy_key: Option<String>,
    /// AUTH_PASSWORD.
    pub(crate) password: String,
    /// The combined session key, to verify AUTH_SVR_RESPONSE.
    pub(crate) session_key: Vec<u8>,
}

/// The fixed "random" inputs, injectable for tests.
pub(crate) struct Nonces {
    pub(crate) part_b: Vec<u8>,
    pub(crate) speedy_salt: Vec<u8>,
    pub(crate) password_salt: Vec<u8>,
}

impl Nonces {
    pub(crate) fn random(part_b_len: usize) -> Self {
        Self {
            part_b: random_bytes(part_b_len),
            speedy_salt: random_bytes(16),
            password_salt: random_bytes(16),
        }
    }
}

/// Derive the phase-two proof from the server challenge and the password.
pub(crate) fn prove(password: &[u8], ch: &ServerChallenge<'_>, nonces: Option<Nonces>) -> Result<ClientProof, String> {
    let verifier = hex_decode(ch.vfr_data)?;
    let is_12c = ch.verifier_type == VERIFIER_12C;
    if !is_12c && ch.verifier_type != VERIFIER_11G_1 && ch.verifier_type != VERIFIER_11G_2 {
        return Err(format!("unsupported password verifier type {:#x}", ch.verifier_type));
    }
    let (keylen, password_hash, password_key) = if is_12c {
        let iters = ch.vgen_count.ok_or("missing AUTH_PBKDF2_VGEN_COUNT")?;
        let mut salt = verifier.clone();
        salt.extend_from_slice(b"AUTH_PBKDF2_SPEEDY_KEY");
        let password_key = pbkdf2_sha512(password, &salt, iters, 64)?;
        let hash = sha512(&[&password_key, &verifier])[..32].to_vec();
        (32usize, hash, Some(password_key))
    } else {
        let mut hash = sha1(&[password, &verifier]);
        hash.extend_from_slice(&[0, 0, 0, 0]);
        (24usize, hash, None)
    };

    let part_a = decrypt_cbc(&password_hash, &hex_decode(ch.sesskey)?)?;
    let nonces = nonces.unwrap_or_else(|| Nonces::random(part_a.len()));
    let part_b = &nonces.part_b;
    if part_b.len() != part_a.len() {
        return Err("client key half has the wrong length".into());
    }
    let encoded_client_key = hex_upper(&encrypt_cbc(&password_hash, part_b)?);

    let (sesskey, session_key) = if part_a.len() == 48 {
        let b: Vec<u8> = (16..40).map(|i| part_a[i] ^ part_b[i]).collect();
        let mut key = md5(&b[..16]).to_vec();
        key.extend_from_slice(&md5(&b[16..24]));
        key.truncate(keylen);
        (encoded_client_key[..96].to_string(), key)
    } else {
        let salt = hex_decode(ch.csk_salt.ok_or("missing AUTH_PBKDF2_CSK_SALT")?)?;
        let iters = ch.sder_count.ok_or("missing AUTH_PBKDF2_SDER_COUNT")?;
        let mut temp = part_b[..keylen].to_vec();
        temp.extend_from_slice(&part_a[..keylen]);
        let key = pbkdf2_sha512(hex_upper(&temp).as_bytes(), &salt, iters, keylen)?;
        (encoded_client_key[..64].to_string(), key)
    };

    let speedy_key = match &password_key {
        Some(pk) => {
            let mut plain = nonces.speedy_salt.clone();
            plain.extend_from_slice(pk);
            Some(hex_upper(&encrypt_cbc(&session_key, &plain)?[..80]))
        }
        None => None,
    };
    let mut plain = nonces.password_salt.clone();
    plain.extend_from_slice(password);
    let password = hex_upper(&encrypt_cbc(&session_key, &plain)?);
    Ok(ClientProof { sesskey, speedy_key, password, session_key })
}

/// Check AUTH_SVR_RESPONSE: decrypted bytes 16..32 must read `SERVER_TO_CLIENT`.
pub(crate) fn verify_server_response(session_key: &[u8], response_hex: &str) -> bool {
    hex_decode(response_hex)
        .and_then(|c| decrypt_cbc(session_key, &c))
        .map(|p| p.get(16..32) == Some(b"SERVER_TO_CLIENT".as_slice()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        hex_decode(&s.replace([' ', '\n'], "")).unwrap()
    }

    #[test]
    fn verifier_12c_matches_the_reference_vectors() {
        let ch = ServerChallenge {
            verifier_type: VERIFIER_12C,
            vfr_data: "101112131415161718191A1B1C1D1E1F",
            sesskey: "B3F9F3DC4D64D905B745C34C376077FD34BBFA4073BE13B113EBA1586F5B9578",
            vgen_count: Some(4096),
            sder_count: Some(3),
            csk_salt: Some("303132333435363738393A3B3C3D3E3F"),
        };
        let nonces = Nonces {
            part_b: h("606162636465666768696A6B6C6D6E6F707172737475767778797A7B7C7D7E7F"),
            speedy_salt: h("B0B1B2B3B4B5B6B7B8B9BABBBCBDBEBF"),
            password_salt: h("A0A1A2A3A4A5A6A7A8A9AAABACADAEAF"),
        };
        let p = prove(b"tiger", &ch, Some(nonces)).unwrap();
        assert_eq!(p.sesskey, "8342DEA04DEEF53A9C8B7C8ADDF0E9A80AC15D01E78D8BFE4849E84B5C9D7385");
        assert_eq!(hex_upper(&p.session_key), "0F911B802E6C12A568BFF9D8DA97D0C1DFD9A9DA20490EA65A43CD0B49AAA2A4");
        assert_eq!(
            p.speedy_key.as_deref(),
            Some(concat!(
                "4F7FC29C7491D4101D58D95D97D61EA076F8E4DCE4D1FD2F5489E2068092BE97",
                "03FED00802D3C8A0B888E1A22BDC40CAB2D5B508E8D2C7482CA2E57052D9B612",
                "779A244188C9343493C2CC2BCD31FB63"
            ))
        );
        assert_eq!(p.password, "C2D93EB067042D3165234579D70B9859EDB779252C8ED47A837E81F63D068716");
        assert!(verify_server_response(
            &p.session_key,
            "67A7F98C94D8BF6A1D3091697344BEEF85C60759FD43234FE835155F6239A1EF"
        ));
        assert!(!verify_server_response(&p.session_key, "00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF"));
    }

    #[test]
    fn verifier_11g_matches_the_reference_vectors() {
        let ch = ServerChallenge {
            verifier_type: VERIFIER_11G_1,
            vfr_data: "0102030405060708090A",
            sesskey: "6C66F205CED653322798A6DB6E96112256300E77E19E5CEF5EFC08481ADFA5055178ECAFC1FB27A0E1DD83138AF5B045",
            vgen_count: None,
            sder_count: None,
            csk_salt: None,
        };
        let nonces = Nonces {
            part_b: h("707172737475767778797A7B7C7D7E7F808182838485868788898A8B8C8D8E8F909192939495969798999A9B9C9D9E9F"),
            speedy_salt: vec![],
            password_salt: h("A0A1A2A3A4A5A6A7A8A9AAABACADAEAF"),
        };
        let p = prove(b"tiger", &ch, Some(nonces)).unwrap();
        assert_eq!(
            p.sesskey,
            "63AECF30BE6070FE0D7DF1EFB2363D6073BC54472A54C3BFC4BF42A5490E093D054A60E2EF25F2C12FDFA5CF405A9C41"
        );
        assert_eq!(hex_upper(&p.session_key), "93EB4801F8E04C1A3BF108D9408AFA0FA4A215B7EF9BDCA9");
        assert_eq!(p.speedy_key, None);
        assert_eq!(p.password, "EE0182CE4BDD00CA8C183F59F128814E767B829DDBAAB2CA374C66C857AFED95");
    }

    #[test]
    fn cbc_padding_always_adds_a_block_on_exact_multiples() {
        assert_eq!(encrypt_cbc(&[1u8; 16], &[0u8; 16]).unwrap().len(), 32);
        assert_eq!(encrypt_cbc(&[1u8; 16], &[0u8; 15]).unwrap().len(), 16);
        let c = encrypt_cbc(&[2u8; 32], b"hello world").unwrap();
        assert_eq!(&decrypt_cbc(&[2u8; 32], &c).unwrap()[..11], b"hello world");
    }
}
