//! Binary packet protection (RFC 4253 §6): framing, padding and the AEAD ciphers —
//! `chacha20-poly1305@openssh.com` (PROTOCOL.chacha20poly1305) and
//! `aes128-gcm@openssh.com` / `aes256-gcm@openssh.com` (RFC 5647). Both carry their
//! own integrity tag, so no separate MAC is negotiated.

use crate::SshError;
use ring::aead::{self, chacha20_poly1305_openssh as chacha, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

/// The largest packet accepted (RFC 4253 §6.1 asks for at least 35000; OpenSSH
/// sends up to 256 KiB).
pub(crate) const MAX_PACKET: usize = 256 * 1024 + 1024;

/// A cipher we can negotiate, by its SSH name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CipherKind {
    ChaCha20Poly1305,
    Aes256Gcm,
    Aes128Gcm,
}

impl CipherKind {
    /// Our preference order.
    pub(crate) const ALL: [CipherKind; 3] = [CipherKind::ChaCha20Poly1305, CipherKind::Aes256Gcm, CipherKind::Aes128Gcm];

    pub(crate) fn name(self) -> &'static str {
        match self {
            CipherKind::ChaCha20Poly1305 => "chacha20-poly1305@openssh.com",
            CipherKind::Aes256Gcm => "aes256-gcm@openssh.com",
            CipherKind::Aes128Gcm => "aes128-gcm@openssh.com",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    pub(crate) fn key_len(self) -> usize {
        match self {
            CipherKind::ChaCha20Poly1305 => chacha::KEY_LEN,
            CipherKind::Aes256Gcm => 32,
            CipherKind::Aes128Gcm => 16,
        }
    }

    pub(crate) fn iv_len(self) -> usize {
        match self {
            CipherKind::ChaCha20Poly1305 => 0,
            _ => 12,
        }
    }
}

/// One direction's packet protection.
pub(crate) enum Protection {
    /// Before the first key exchange completes.
    None,
    ChaCha { seal: Option<chacha::SealingKey>, open: Option<chacha::OpeningKey> },
    Gcm { key: Box<LessSafeKey>, iv: [u8; 12] },
}

impl Protection {
    /// The protection for `kind` with derived `key` and `iv`, sealing (we send) or
    /// opening (we receive).
    pub(crate) fn new(kind: CipherKind, key: &[u8], iv: &[u8], sealing: bool) -> Self {
        match kind {
            CipherKind::ChaCha20Poly1305 => {
                let k: &[u8; chacha::KEY_LEN] = key[..chacha::KEY_LEN].try_into().expect("64 key bytes");
                if sealing {
                    Protection::ChaCha { seal: Some(chacha::SealingKey::new(k)), open: None }
                } else {
                    Protection::ChaCha { seal: None, open: Some(chacha::OpeningKey::new(k)) }
                }
            }
            CipherKind::Aes256Gcm | CipherKind::Aes128Gcm => {
                let alg = if kind == CipherKind::Aes256Gcm { &aead::AES_256_GCM } else { &aead::AES_128_GCM };
                let key = Box::new(LessSafeKey::new(UnboundKey::new(alg, &key[..kind.key_len()]).expect("key length")));
                let mut fixed = [0u8; 12];
                fixed.copy_from_slice(&iv[..12]);
                Protection::Gcm { key, iv: fixed }
            }
        }
    }

    /// Bytes the padding aligns to, and whether the length field is outside that
    /// alignment (the AEAD modes keep it as associated data).
    fn block(&self) -> (usize, bool) {
        match self {
            Protection::None => (8, false),
            Protection::ChaCha { .. } => (8, true),
            Protection::Gcm { .. } => (16, true),
        }
    }

    fn tag_len(&self) -> usize {
        match self {
            Protection::None => 0,
            _ => 16,
        }
    }

    /// Frame `payload` as packet number `seq` and protect it: the bytes to write.
    pub(crate) fn seal(&mut self, seq: u32, payload: &[u8], rng: &SystemRandom) -> Result<Vec<u8>, SshError> {
        let (block, aad) = self.block();
        let framed = 1 + payload.len() + if aad { 0 } else { 4 };
        let mut pad = block - framed % block;
        if pad < 4 {
            pad += block;
        }
        let packet_len = 1 + payload.len() + pad;
        let mut out = Vec::with_capacity(4 + packet_len + self.tag_len());
        out.extend_from_slice(&u32::try_from(packet_len).map_err(|_| SshError::Protocol("a packet over 4 GiB".into()))?.to_be_bytes());
        out.push(u8::try_from(pad).expect("padding under 256"));
        out.extend_from_slice(payload);
        let start = out.len();
        out.resize(start + pad, 0);
        rng.fill(&mut out[start..]).map_err(|_| SshError::Protocol("the system RNG failed".into()))?;
        match self {
            Protection::None => {}
            Protection::ChaCha { seal, .. } => {
                let mut tag = [0u8; chacha::TAG_LEN];
                seal.as_ref().expect("a sealing key").seal_in_place(seq, &mut out, &mut tag);
                out.extend_from_slice(&tag);
            }
            Protection::Gcm { key, iv } => {
                let nonce = Nonce::assume_unique_for_key(*iv);
                let (len, body) = out.split_at_mut(4);
                let tag = key
                    .seal_in_place_separate_tag(nonce, Aad::from(&*len), body)
                    .map_err(|_| SshError::Protocol("AES-GCM sealing failed".into()))?;
                out.extend_from_slice(tag.as_ref());
                bump(iv);
            }
        }
        Ok(out)
    }

    /// How many bytes to read first to learn a packet's length (4, the length field).
    pub(crate) const fn first_len() -> usize {
        4
    }

    /// From the first 4 bytes of packet `seq`, the bytes that remain to be read.
    pub(crate) fn remaining(&self, seq: u32, first: [u8; 4]) -> Result<usize, SshError> {
        let len = match self {
            Protection::ChaCha { open, .. } => u32::from_be_bytes(open.as_ref().expect("an opening key").decrypt_packet_length(seq, first)),
            _ => u32::from_be_bytes(first),
        } as usize;
        let (block, aad) = self.block();
        let aligned = if aad { len } else { len + 4 };
        if !(5..=MAX_PACKET).contains(&len) || aligned % block != 0 {
            return Err(SshError::Protocol(format!("a bad packet length {len}")));
        }
        Ok(len + self.tag_len())
    }

    /// Check and decrypt packet `seq` (`packet` = the 4 length bytes as received plus
    /// the rest), returning its payload.
    pub(crate) fn open(&mut self, seq: u32, mut packet: Vec<u8>) -> Result<Vec<u8>, SshError> {
        let bad_mac = || SshError::Protocol("a packet failed its integrity check (corrupted or tampered with)".into());
        let tag_len = self.tag_len();
        let body_end = packet.len() - tag_len;
        match self {
            Protection::None => {}
            Protection::ChaCha { open, .. } => {
                let mut tag = [0u8; chacha::TAG_LEN];
                tag.copy_from_slice(&packet[body_end..]);
                open.as_ref().expect("an opening key").open_in_place(seq, &mut packet[..body_end], &tag).map_err(|_| bad_mac())?;
            }
            Protection::Gcm { key, iv } => {
                let nonce = Nonce::assume_unique_for_key(*iv);
                let (len, body) = packet.split_at_mut(4);
                key.open_in_place(nonce, Aad::from(&*len), body).map_err(|_| bad_mac())?;
                bump(iv);
            }
        }
        packet.truncate(body_end);
        let pad = *packet.get(4).ok_or_else(bad_mac)? as usize;
        if pad < 4 || 5 + pad > packet.len() {
            return Err(SshError::Protocol("bad packet padding".into()));
        }
        let end = packet.len() - pad;
        packet.truncate(end);
        packet.drain(..5);
        Ok(packet)
    }
}

/// RFC 5647 §7.1: the invocation counter (the IV's last 8 bytes) goes up by one per packet.
fn bump(iv: &mut [u8; 12]) {
    let mut c = [0u8; 8];
    c.copy_from_slice(&iv[4..]);
    iv[4..].copy_from_slice(&u64::from_be_bytes(c).wrapping_add(1).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(kind: Option<CipherKind>) {
        let rng = SystemRandom::new();
        let key = [7u8; 64];
        let iv = [9u8; 12];
        let (mut s, mut o) = match kind {
            None => (Protection::None, Protection::None),
            Some(k) => (Protection::new(k, &key, &iv, true), Protection::new(k, &key, &iv, false)),
        };
        for (seq, len) in [(0u32, 0usize), (1, 1), (2, 31), (3, 32 * 1024), (u32::MAX, 100)] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 31) as u8).collect();
            let wire = s.seal(seq, &payload, &rng).unwrap();
            let (block, aad) = s.block();
            assert_eq!((wire.len() - s.tag_len() - if aad { 4 } else { 0 }) % block, 0, "{kind:?}: aligned");
            let rest = o.remaining(seq, wire[..4].try_into().unwrap()).unwrap();
            assert_eq!(4 + rest, wire.len());
            assert_eq!(o.open(seq, wire.clone()).unwrap(), payload, "{kind:?} {len}");
            if let Some(k) = kind {
                // Every byte is covered: flip any and the packet is refused.
                for i in [0, 4, wire.len() / 2, wire.len() - 1] {
                    let (mut s2, mut o2) = (Protection::new(k, &key, &iv, true), Protection::new(k, &key, &iv, false));
                    let mut w = s2.seal(seq, &payload, &rng).unwrap();
                    w[i] ^= 1;
                    let ok = o2.remaining(seq, w[..4].try_into().unwrap()).is_ok_and(|r| r + 4 == w.len()) && o2.open(seq, w).is_ok();
                    assert!(!ok, "{kind:?}: byte {i} flipped went unnoticed");
                }
                // Same key, wrong sequence number: refused.
                if matches!(kind, Some(CipherKind::ChaCha20Poly1305)) {
                    let mut o3 = Protection::new(k, &key, &iv, false);
                    let w = Protection::new(k, &key, &iv, true).seal(seq, &payload, &rng).unwrap();
                    assert!(o3.remaining(seq.wrapping_add(1), w[..4].try_into().unwrap()).is_err() || o3.open(seq.wrapping_add(1), w).is_err());
                }
            }
            // Keep the GCM counters in step for the next iteration.
            if let (Protection::Gcm { iv: a, .. }, Protection::Gcm { iv: b, .. }) = (&s, &o) {
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn packets_round_trip_and_tampering_is_caught() {
        roundtrip(None);
        for k in CipherKind::ALL {
            roundtrip(Some(k));
        }
    }

    #[test]
    fn the_gcm_counter_counts_per_packet() {
        let mut iv = [0u8; 12];
        iv[11] = 0xff;
        bump(&mut iv);
        assert_eq!(&iv[4..], &[0, 0, 0, 0, 0, 0, 1, 0]);
        let mut top = [0xffu8; 12];
        bump(&mut top);
        assert_eq!(&top[..4], &[0xff; 4], "the fixed part never changes");
        assert_eq!(&top[4..], &[0; 8], "the counter wraps");
    }
}
