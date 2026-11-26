//! TNS packet framing: an 8-byte header (length, checksum, type, flags,
//! header checksum), DATA packets carrying 2 bytes of data flags, and the
//! CONNECT / ACCEPT exchange that opens a session.

use kernway_orm_core::error::OrmError;
use kernway_wire::BufStream;

/// Size of every packet header.
pub(crate) const PACKET_HEADER_SIZE: usize = 8;

/// TNS packet types.
pub(crate) mod packet_type {
    #![allow(dead_code)]
    /// Client's connection request.
    pub const CONNECT: u8 = 1;
    /// Listener accepted the connection.
    pub const ACCEPT: u8 = 2;
    /// Listener refused the connection.
    pub const REFUSE: u8 = 4;
    /// Listener redirects to another address.
    pub const REDIRECT: u8 = 5;
    /// Normal data.
    pub const DATA: u8 = 6;
    /// Listener asks the client to resend its CONNECT.
    pub const RESEND: u8 = 11;
    /// Break / reset markers.
    pub const MARKER: u8 = 12;
    /// Control packets.
    pub const CONTROL: u8 = 14;
}

/// Protocol version we ask for, and the oldest we accept (12.1).
pub(crate) const VERSION_DESIRED: u16 = 319;
pub(crate) const VERSION_MINIMUM: u16 = 300;
pub(crate) const VERSION_MIN_ACCEPTED: u16 = 315;
/// From this version on, packet lengths are 4 bytes (large SDU).
pub(crate) const VERSION_MIN_LARGE_SDU: u16 = 315;
pub(crate) const VERSION_MIN_OOB_CHECK: u16 = 318;

const GSO_DONT_CARE: u16 = 0x0001;
const NSI_NA_DISABLED: u8 = 0x04;
const NSI_NA_REQUIRED: u8 = 0x10;
const NSI_SUPPORT_SECURITY_RENEG: u8 = 0x80;
/// Packet flag on a RESEND over TCPS: redo the TLS handshake first.
const PACKET_FLAG_TLS_RENEG: u8 = 0x08;
const PROTOCOL_CHARACTERISTICS: u16 = 0x4f98;
/// Connect data longer than this goes in a separate DATA packet.
const MAX_CONNECT_DATA: usize = 230;
/// Session data unit we propose.
pub(crate) const DEFAULT_SDU: u32 = 8192;

pub(crate) fn conn_err(msg: impl Into<String>) -> OrmError {
    OrmError::Connection(msg.into())
}

/// A received packet: its type, flags and payload (after the 8-byte header;
/// for DATA packets the payload still starts with the 2 data-flag bytes).
pub(crate) struct Packet<'a> {
    pub(crate) packet_type: u8,
    pub(crate) flags: u8,
    pub(crate) payload: &'a [u8],
}

/// Reads and writes TNS packets over a [`BufStream`].
pub(crate) struct Tns {
    pub(crate) io: BufStream,
    /// Negotiated protocol version (0 until ACCEPT).
    pub(crate) version: u16,
    /// Negotiated session data unit.
    pub(crate) sdu: u32,
    /// Accept flags (second word) from the server.
    pub(crate) accept_flags2: u32,
}

impl Tns {
    pub(crate) fn new(io: BufStream) -> Self {
        Self { io, version: 0, sdu: DEFAULT_SDU, accept_flags2: 0 }
    }

    fn large_lengths(&self) -> bool {
        self.version >= VERSION_MIN_LARGE_SDU
    }

    /// Send one packet whose payload (everything after the 8-byte header) is
    /// `payload`.
    pub(crate) async fn send(&mut self, packet_type: u8, flags: u8, payload: &[u8]) -> Result<(), OrmError> {
        let size = PACKET_HEADER_SIZE + payload.len();
        let large = self.large_lengths();
        let buf = self.io.write_buf();
        if large {
            buf.extend_from_slice(&(size as u32).to_be_bytes());
        } else {
            buf.extend_from_slice(&(size as u16).to_be_bytes());
            buf.extend_from_slice(&[0, 0]);
        }
        buf.extend_from_slice(&[packet_type, flags, 0, 0]);
        buf.extend_from_slice(payload);
        self.io.flush().await.map_err(|e| conn_err(format!("write: {e}")))
    }

    /// Receive one packet (payload borrowed from the read buffer).
    pub(crate) async fn recv(&mut self) -> Result<Packet<'_>, OrmError> {
        let read = |e: std::io::Error| conn_err(format!("read: {e}"));
        self.io.fill(PACKET_HEADER_SIZE).await.map_err(read)?;
        let h = self.io.peek(PACKET_HEADER_SIZE);
        let size = if self.large_lengths() {
            u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize
        } else {
            u16::from_be_bytes([h[0], h[1]]) as usize
        };
        let (packet_type, flags) = (h[4], h[5]);
        if size < PACKET_HEADER_SIZE {
            return Err(conn_err(format!("bad TNS packet size {size}")));
        }
        self.io.fill(size).await.map_err(read)?;
        self.io.take(PACKET_HEADER_SIZE);
        let payload = self.io.take(size - PACKET_HEADER_SIZE);
        Ok(Packet { packet_type, flags, payload })
    }

    /// Send CONNECT for `connect_data` (a connect descriptor) and process the
    /// listener's answer until ACCEPT. RESEND is honoured; REFUSE becomes an
    /// error carrying the listener's message; REDIRECT is not supported yet.
    pub(crate) async fn connect(
        &mut self,
        connect_data: &str,
        tls: Option<(std::sync::Arc<kernway_tls::rustls::ClientConfig>, &str)>,
    ) -> Result<(), OrmError> {
        let data = connect_data.as_bytes();
        let flags = 0u8;
        for _ in 0..4 {
            let nsi = NSI_SUPPORT_SECURITY_RENEG | NSI_NA_DISABLED;
            let mut p = Vec::with_capacity(74 + data.len());
            p.extend_from_slice(&VERSION_DESIRED.to_be_bytes());
            p.extend_from_slice(&VERSION_MINIMUM.to_be_bytes());
            p.extend_from_slice(&GSO_DONT_CARE.to_be_bytes()); // service options
            p.extend_from_slice(&(DEFAULT_SDU as u16).to_be_bytes());
            p.extend_from_slice(&(DEFAULT_SDU as u16).to_be_bytes());
            p.extend_from_slice(&PROTOCOL_CHARACTERISTICS.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes()); // line turnaround
            p.extend_from_slice(&1u16.to_be_bytes()); // value of 1
            p.extend_from_slice(&(data.len() as u16).to_be_bytes());
            p.extend_from_slice(&74u16.to_be_bytes()); // offset to connect data
            p.extend_from_slice(&0u32.to_be_bytes()); // max receivable data
            p.push(nsi);
            p.push(nsi);
            p.extend_from_slice(&[0u8; 24]); // obsolete
            p.extend_from_slice(&DEFAULT_SDU.to_be_bytes()); // SDU (large)
            p.extend_from_slice(&DEFAULT_SDU.to_be_bytes()); // TDU (large)
            p.extend_from_slice(&0u32.to_be_bytes()); // connect flags 1
            p.extend_from_slice(&0u32.to_be_bytes()); // connect flags 2
            debug_assert_eq!(PACKET_HEADER_SIZE + p.len(), 74);
            if data.len() > MAX_CONNECT_DATA {
                self.send(packet_type::CONNECT, flags, &p).await?;
                let mut d = vec![0u8, 0u8]; // data flags
                d.extend_from_slice(data);
                self.send(packet_type::DATA, 0, &d).await?;
            } else {
                p.extend_from_slice(data);
                self.send(packet_type::CONNECT, flags, &p).await?;
            }

            let pkt = self.recv().await?;
            let (ptype, pflags, payload) = (pkt.packet_type, pkt.flags, pkt.payload.to_vec());
            match ptype {
                packet_type::ACCEPT => return self.accept(payload),
                packet_type::RESEND => {
                    // Over TCPS the listener may ask for a brand-new TLS
                    // handshake on the same socket before the CONNECT is
                    // repeated.
                    if pflags & PACKET_FLAG_TLS_RENEG != 0 {
                        let (config, name) = tls.clone().ok_or_else(|| {
                            conn_err("listener asked for TLS renegotiation on a plain TCP connection")
                        })?;
                        self.io
                            .renegotiate_tls(config, name)
                            .await
                            .map_err(|e| conn_err(format!("TLS renegotiation: {e}")))?;
                    }
                    continue;
                }
                packet_type::REFUSE => {
                    let p = &payload;
                    let len = p.get(2..4).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as usize);
                    let msg = p.get(4..4 + len).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                    return Err(conn_err(format!("listener refused the connection: {msg}")));
                }
                packet_type::REDIRECT => return Err(conn_err("listener redirect is not supported yet")),
                other => return Err(conn_err(format!("unexpected TNS packet type {other} during connect"))),
            }
        }
        Err(conn_err("listener kept asking to resend the connect packet"))
    }

    fn accept(&mut self, p: Vec<u8>) -> Result<(), OrmError> {
        let u16_at = |i: usize| p.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        let u32_at = |i: usize| p.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        let version = u16_at(0).ok_or_else(|| conn_err("short ACCEPT"))?;
        if version < VERSION_MIN_ACCEPTED {
            return Err(conn_err(format!("server TNS version {version} is too old (need 12.1+)")));
        }
        // version(2) options(2) skip(10) flags1(1) skip(9) sdu(4) [skip(5) flags2(4)]
        let flags1 = *p.get(14).ok_or_else(|| conn_err("short ACCEPT"))?;
        if flags1 & NSI_NA_REQUIRED != 0 {
            return Err(conn_err("server requires Native Network Encryption, which is not supported"));
        }
        self.sdu = u32_at(24).ok_or_else(|| conn_err("short ACCEPT (sdu)"))?;
        if version >= VERSION_MIN_OOB_CHECK {
            self.accept_flags2 = u32_at(33).unwrap_or(0);
        }
        self.version = version;
        Ok(())
    }
}
