//! LOB locators: the handle a CLOB / BLOB travels as in PL/SQL (an OUT
//! argument, a function result, an IN value past the 32 KB PL/SQL limit), and
//! the LOB operations on it (TTC function 96). Ported from python-oracledb's
//! `messages/lob_op.pyx` and `lob.pyx`; checked against the server.

use kernway_orm_core::error::OrmError;

use crate::buf::{PResult, Reader, Writer};
use crate::conn::{common_message, func, msg, CallStatus, Conn};
use crate::value::Value;

/// LOB operation codes.
mod op {
    pub const READ: u32 = 0x0002;
    pub const WRITE: u32 = 0x0040;
    pub const CREATE_TEMP: u32 = 0x0110;
    pub const FREE_TEMP: u32 = 0x0111;
    pub const FILE_OPEN: u32 = 0x0100;
    pub const FILE_CLOSE: u32 = 0x0200;
}

/// FILE_OPEN mode: read only (the only mode a BFILE has).
const OPEN_READ_ONLY: i64 = 11;

/// Temporary LOBs live for the session (`OCI_DURATION_SESSION`).
const DURATION_SESSION: u32 = 10;
/// The database character set id for AL32UTF8 (CLOB data is sent in it).
const CHARSET_AL32UTF8: u64 = 873;
/// Bytes of a fresh locator buffer for CREATE_TEMP; the server fills it.
const TEMP_LOCATOR_LEN: usize = 40;
/// Most characters / bytes asked for by one READ.
const READ_ALL: i64 = u32::MAX as i64;

/// Locator flag bytes and bits.
const LOC_FLAG_1: usize = 4;
const LOC_FLAG_3: usize = 6;
const LOC_FLAG_4: usize = 7;
const LOC_ABSTRACT: u8 = 0x40;
const LOC_VAR_LENGTH_CHARSET: u8 = 0x80;
const LOC_TEMP: u8 = 0x01;
const LOC_LITTLE_ENDIAN: u8 = 0x40;

/// A CLOB / BLOB locator received from or created on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lob {
    /// The opaque locator bytes.
    pub locator: Vec<u8>,
    /// BLOB (bytes) rather than CLOB (text).
    pub blob: bool,
    /// A BFILE: a read-only file on the database server (bytes).
    pub bfile: bool,
}

impl Lob {
    /// A temporary LOB (created for a bind, or returned by PL/SQL) that must
    /// be freed once used.
    pub(crate) fn is_temporary(&self) -> bool {
        let f1 = self.locator.get(LOC_FLAG_1).copied().unwrap_or(0);
        let f4 = self.locator.get(LOC_FLAG_4).copied().unwrap_or(0);
        f1 & LOC_ABSTRACT != 0 || f4 & LOC_TEMP != 0
    }

    /// Decode CLOB data as the locator says it is encoded: UTF-16 (big- or,
    /// for 9i-era CLOBs, little-endian) for a variable-width database
    /// character set, else UTF-8.
    fn text(&self, data: &[u8]) -> Result<String, OrmError> {
        let f3 = self.locator.get(LOC_FLAG_3).copied().unwrap_or(0);
        let f4 = self.locator.get(LOC_FLAG_4).copied().unwrap_or(0);
        if f3 & LOC_VAR_LENGTH_CHARSET == 0 {
            return String::from_utf8(data.to_vec()).map_err(|e| OrmError::TypeConversion(format!("CLOB is not UTF-8: {e}")));
        }
        let le = f4 & LOC_LITTLE_ENDIAN != 0;
        let units: Vec<u16> =
            data.as_chunks::<2>().0.iter().map(|&c| if le { u16::from_le_bytes(c) } else { u16::from_be_bytes(c) }).collect();
        String::from_utf16(&units).map_err(|e| OrmError::TypeConversion(format!("CLOB is not UTF-16: {e}")))
    }

    /// Encode text for a WRITE, the way [`text`](Self::text) reads it.
    fn encode(&self, s: &str) -> Vec<u8> {
        let f3 = self.locator.get(LOC_FLAG_3).copied().unwrap_or(0);
        if f3 & LOC_VAR_LENGTH_CHARSET == 0 {
            return s.as_bytes().to_vec();
        }
        let le = self.locator.get(LOC_FLAG_4).copied().unwrap_or(0) & LOC_LITTLE_ENDIAN != 0;
        s.encode_utf16().flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() }).collect()
    }
}

/// One LOB operation request.
struct LobOp<'a> {
    op: u32,
    lob: Option<&'a Lob>,
    dest_length: u32,
    source_offset: u64,
    dest_offset: u64,
    amount: Option<i64>,
    data: Option<&'a [u8]>,
}

/// What came back.
#[derive(Default)]
struct LobReply {
    locator: Option<Vec<u8>>,
    data: Option<Vec<u8>>,
    #[allow(dead_code)]
    amount: i64,
    status: CallStatus,
}

fn write_op(w: &mut Writer, o: &LobOp<'_>) {
    match o.lob {
        Some(l) => w.u8(1).ub(l.locator.len() as u64),
        None => w.u8(0).ub(0),
    };
    w.u8(0).ub(u64::from(o.dest_length)); // dest pointer, dest length
    w.ub(0).ub(0); // short source / dest offsets
    w.u8(u8::from(o.op == op::CREATE_TEMP)); // charset pointer
    w.u8(0); // short amount pointer
    w.u8(u8::from(o.op == op::CREATE_TEMP)); // NULL LOB pointer
    w.ub(u64::from(o.op));
    w.u8(0).u8(0); // SCN array
    w.ub(o.source_offset).ub(o.dest_offset);
    w.u8(u8::from(o.amount.is_some()));
    w.u16be(0).u16be(0).u16be(0); // array LOB (unused)
    if let Some(l) = o.lob {
        w.raw(&l.locator);
    }
    if o.op == op::CREATE_TEMP {
        w.ub(CHARSET_AL32UTF8);
    }
    if let Some(d) = o.data {
        w.u8(msg::LOB_DATA).bytes_wl(d);
    }
    if let Some(a) = o.amount {
        w.ub(a as u64);
    }
}

fn parse_reply(b: &[u8], fv: u8, eor: bool, o_op: u32, locator_len: Option<usize>, wants_amount: bool) -> PResult<LobReply> {
    let mut r = Reader::new(b);
    let mut out = LobReply::default();
    while r.remaining() > 0 {
        let t = r.u8()?;
        match t {
            msg::END_OF_RESPONSE => break,
            msg::LOB_DATA => out.data = Some(r.bytes()?.unwrap_or_default()),
            msg::PARAMETER => {
                if let Some(n) = locator_len {
                    out.locator = Some(r.take(n)?.to_vec());
                }
                if o_op == op::CREATE_TEMP {
                    r.ub2()?; // character set
                    r.u8()?; // trailing flags
                } else if wants_amount {
                    out.amount = r.sb()?;
                }
            }
            other => {
                if common_message(&mut r, other, fv, &mut out.status)? && !eor {
                    break;
                }
            }
        }
    }
    Ok(out)
}

impl Conn {
    async fn lob_op(&mut self, o: LobOp<'_>) -> Result<LobReply, OrmError> {
        let mut w = Writer::new();
        self.function_header(&mut w, func::LOB_OP);
        write_op(&mut w, &o);
        self.send_data(&w.buf).await?;
        let (fv, eor, code) = (self.field_version, self.end_of_response(), o.op);
        let loc_len = o.lob.map(|l| l.locator.len());
        let wants = o.amount.is_some();
        let reply = self.read_response(|b| parse_reply(b, fv, eor, code, loc_len, wants)).await?;
        if let Some(e) = &reply.status.error {
            return Err(crate::exec::server_error(e));
        }
        Ok(reply)
    }

    /// A new temporary CLOB (or BLOB) for the session.
    pub(crate) async fn lob_create_temp(&mut self, blob: bool) -> Result<Lob, OrmError> {
        let mut lob = Lob { locator: vec![0; TEMP_LOCATOR_LEN], blob, bfile: false };
        let reply = self
            .lob_op(LobOp {
                op: op::CREATE_TEMP,
                lob: Some(&lob),
                dest_length: DURATION_SESSION,
                source_offset: if blob { 0 } else { 1 }, // character set form
                dest_offset: if blob { 113 } else { 112 }, // Oracle type number
                amount: None,
                data: None,
            })
            .await?;
        lob.locator = reply.locator.ok_or_else(|| OrmError::Connection("CREATE_TEMP returned no locator".into()))?;
        Ok(lob)
    }

    /// Write `value` (text for a CLOB, bytes for a BLOB) at the start of `lob`.
    pub(crate) async fn lob_write(&mut self, lob: &mut Lob, value: &Value) -> Result<(), OrmError> {
        let data = match (value, lob.blob) {
            (Value::Raw(b), true) => b.clone(),
            (Value::Str(s), false) => lob.encode(s),
            _ => return Err(OrmError::TypeConversion("a CLOB takes text, a BLOB bytes".into())),
        };
        let reply = self
            .lob_op(LobOp {
                op: op::WRITE,
                lob: Some(lob),
                dest_length: 0,
                source_offset: 1,
                dest_offset: 0,
                amount: None,
                data: Some(&data),
            })
            .await?;
        if let Some(l) = reply.locator {
            lob.locator = l;
        }
        Ok(())
    }

    /// The whole content of `lob`: text for a CLOB, bytes for a BLOB or a
    /// BFILE (opened for the read and closed again).
    pub(crate) async fn lob_read_all(&mut self, lob: &Lob) -> Result<Value, OrmError> {
        if lob.bfile {
            // The open state lives in the locator: each operation hands back
            // the locator to use for the next one.
            let mut lob = lob.clone();
            let open = self
                .lob_op(LobOp {
                    op: op::FILE_OPEN,
                    lob: Some(&lob),
                    dest_length: 0,
                    source_offset: 0,
                    dest_offset: 0,
                    amount: Some(OPEN_READ_ONLY),
                    data: None,
                })
                .await?;
            if let Some(l) = open.locator {
                lob.locator = l;
            }
            let read = self.lob_read_data(&mut lob).await;
            let close = self
                .lob_op(LobOp { op: op::FILE_CLOSE, lob: Some(&lob), dest_length: 0, source_offset: 0, dest_offset: 0, amount: None, data: None })
                .await;
            let data = read?;
            close?;
            return Ok(Value::Raw(data));
        }
        let data = self.lob_read_data(&mut lob.clone()).await?;
        Ok(if lob.blob { Value::Raw(data) } else { Value::Str(lob.text(&data)?) })
    }

    async fn lob_read_data(&mut self, lob: &mut Lob) -> Result<Vec<u8>, OrmError> {
        let reply = self
            .lob_op(LobOp {
                op: op::READ,
                lob: Some(lob),
                dest_length: 0,
                source_offset: 1,
                dest_offset: 0,
                amount: Some(READ_ALL),
                data: None,
            })
            .await?;
        if let Some(l) = reply.locator {
            lob.locator = l;
        }
        Ok(reply.data.unwrap_or_default())
    }

    /// Free a temporary LOB.
    pub(crate) async fn lob_free_temp(&mut self, lob: &Lob) -> Result<(), OrmError> {
        self.lob_op(LobOp { op: op::FREE_TEMP, lob: Some(lob), dest_length: 0, source_offset: 0, dest_offset: 0, amount: None, data: None })
            .await
            .map(|_| ())
    }
}
