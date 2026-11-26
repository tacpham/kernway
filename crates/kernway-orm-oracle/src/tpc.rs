//! Two-phase commit (distributed / XA transactions): a global transaction
//! branch identified by an [`Xid`] is begun, ended, prepared, and committed
//! or rolled back — possibly from another session (recovery). TTC functions
//! 103 (transaction switch) and 104 (change state), ported from
//! python-oracledb's `tpc_switch.pyx` / `tpc_change_state.pyx`.

use kernway_orm_core::error::OrmError;

use crate::buf::{PResult, Reader, Writer};
use crate::conn::{common_message, msg, CallStatus, Conn};

const FUNC_TXN_SWITCH: u8 = 103;
const FUNC_TXN_CHANGE_STATE: u8 = 104;
const OP_START: u32 = 0x01;
const OP_DETACH: u32 = 0x02;
const OP_COMMIT: u32 = 0x01;
const OP_ABORT: u32 = 0x02;
const OP_PREPARE: u32 = 0x03;
const STATE_REQUIRES_COMMIT: u32 = 1;
const STATE_COMMITTED: u32 = 2;
const STATE_ABORTED: u32 = 3;
const STATE_READ_ONLY: u32 = 4;
const STATE_FORGOTTEN: u32 = 5;
/// `tpc_end` flag: suspend the branch (resume it later with
/// [`TpcBegin::Resume`]).
const END_SUSPEND: u32 = 0x0010_0000;

/// A global transaction id: format, global transaction id and branch
/// qualifier (each at most 64 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xid {
    /// Format identifier (any non-negative number agreed by the parties).
    pub format_id: u32,
    /// Global transaction id.
    pub global_id: Vec<u8>,
    /// Branch qualifier.
    pub branch: Vec<u8>,
}

/// How [`Conn::tpc_begin`] attaches the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpcBegin {
    /// A new branch.
    New = 0x01,
    /// Join an existing branch.
    Join = 0x02,
    /// Resume a suspended branch.
    Resume = 0x04,
}

impl Xid {
    fn bytes(&self) -> Result<Vec<u8>, OrmError> {
        if self.global_id.len() > 64 || self.branch.len() > 64 {
            return Err(OrmError::Query("an XID part is longer than 64 bytes".into()));
        }
        let mut b = [self.global_id.as_slice(), self.branch.as_slice()].concat();
        b.resize(128, 0);
        Ok(b)
    }
}

fn write_xid_header(w: &mut Writer, xid: Option<&Xid>, xid_bytes: &[u8]) {
    match xid {
        Some(x) => {
            w.ub(u64::from(x.format_id)).ub(x.global_id.len() as u64).ub(x.branch.len() as u64);
            w.u8(1).ub(xid_bytes.len() as u64);
        }
        None => {
            w.ub(0).ub(0).ub(0);
            w.u8(0).ub(0);
        }
    }
}

#[derive(Default)]
struct Reply {
    context: Option<Vec<u8>>,
    state: u32,
    status: CallStatus,
}

fn parse(b: &[u8], fv: u8, eor: bool, switch: bool) -> PResult<Reply> {
    let mut r = Reader::new(b);
    let mut out = Reply::default();
    while r.remaining() > 0 {
        let t = r.u8()?;
        match t {
            msg::END_OF_RESPONSE => break,
            msg::PARAMETER if switch => {
                r.ub4()?; // application value
                let n = r.ub2()? as usize;
                out.context = Some(r.take(n)?.to_vec());
            }
            msg::PARAMETER => out.state = r.ub4()?,
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
    async fn tpc_call(&mut self, w: Writer, switch: bool) -> Result<Reply, OrmError> {
        self.send_data(&w.buf).await?;
        let (fv, eor) = (self.field_version, self.end_of_response());
        let reply = self.read_response(|b| parse(b, fv, eor, switch)).await?;
        if let Some(e) = &reply.status.error {
            return Err(crate::exec::server_error(e));
        }
        Ok(reply)
    }

    async fn tpc_switch(&mut self, op: u32, xid: Option<&Xid>, flags: u32, timeout: u32) -> Result<Reply, OrmError> {
        let xid_bytes = match xid {
            Some(x) => x.bytes()?,
            None => Vec::new(),
        };
        let context = self.tpc_context.clone();
        let mut w = Writer::new();
        self.function_header(&mut w, FUNC_TXN_SWITCH);
        w.ub(u64::from(op));
        match &context {
            Some(c) => w.u8(1).ub(c.len() as u64),
            None => w.u8(0).ub(0),
        };
        write_xid_header(&mut w, xid, &xid_bytes);
        w.ub(u64::from(flags)).ub(u64::from(timeout));
        w.u8(1).u8(1).u8(1); // application value, return context (+ length)
        w.u8(0).ub(0); // internal name
        w.u8(0).ub(0); // external name
        if let Some(c) = &context {
            w.raw(c);
        }
        w.raw(&xid_bytes);
        w.ub(0); // application value
        self.tpc_call(w, true).await
    }

    async fn tpc_change_state(&mut self, op: u32, xid: &Xid, state: u32) -> Result<u32, OrmError> {
        let xid_bytes = xid.bytes()?;
        let context = self.tpc_context.clone();
        let mut w = Writer::new();
        self.function_header(&mut w, FUNC_TXN_CHANGE_STATE);
        w.ub(u64::from(op));
        match &context {
            Some(c) => w.u8(1).ub(c.len() as u64),
            None => w.u8(0).ub(0),
        };
        write_xid_header(&mut w, Some(xid), &xid_bytes);
        w.ub(0); // timeout
        w.ub(u64::from(state));
        w.u8(1); // out state pointer
        w.ub(0); // flags
        if let Some(c) = &context {
            w.raw(c);
        }
        w.raw(&xid_bytes);
        Ok(self.tpc_call(w, false).await?.state)
    }

    /// Begin (or join / resume) the transaction branch `xid`; `timeout`
    /// seconds before an idle suspended branch is rolled back (0 = default).
    pub async fn tpc_begin(&mut self, xid: &Xid, how: TpcBegin, timeout: u32) -> Result<(), OrmError> {
        let r = self.tpc_switch(OP_START, Some(xid), how as u32, timeout).await?;
        self.tpc_context = r.context;
        Ok(())
    }

    /// End the work on the branch (detach); `suspend` to resume it later.
    pub async fn tpc_end(&mut self, xid: &Xid, suspend: bool) -> Result<(), OrmError> {
        self.tpc_switch(OP_DETACH, Some(xid), if suspend { END_SUSPEND } else { 0 }, 0).await?;
        self.tpc_context = None;
        Ok(())
    }

    /// Prepare the branch for commit: `true` when it changed data and must
    /// be committed, `false` when it was read-only (nothing to commit).
    pub async fn tpc_prepare(&mut self, xid: &Xid) -> Result<bool, OrmError> {
        match self.tpc_change_state(OP_PREPARE, xid, 0).await? {
            STATE_REQUIRES_COMMIT => Ok(true),
            STATE_READ_ONLY => Ok(false),
            s => Err(OrmError::Transaction(format!("unexpected transaction state {s} after prepare"))),
        }
    }

    /// Commit the branch: after [`tpc_prepare`](Self::tpc_prepare), or in
    /// one phase (no prepare). Works from any session (recovery).
    pub async fn tpc_commit(&mut self, xid: &Xid, one_phase: bool) -> Result<(), OrmError> {
        let want = if one_phase { STATE_READ_ONLY } else { STATE_COMMITTED };
        let s = self.tpc_change_state(OP_COMMIT, xid, want).await?;
        self.tpc_context = None;
        let ok = if one_phase { matches!(s, STATE_READ_ONLY | STATE_COMMITTED) } else { s == STATE_FORGOTTEN };
        if ok {
            Ok(())
        } else {
            Err(OrmError::Transaction(format!("unexpected transaction state {s} after commit")))
        }
    }

    /// Roll the branch back (from any session).
    pub async fn tpc_rollback(&mut self, xid: &Xid) -> Result<(), OrmError> {
        let s = self.tpc_change_state(OP_ABORT, xid, STATE_ABORTED).await?;
        self.tpc_context = None;
        if s == STATE_ABORTED {
            Ok(())
        } else {
            Err(OrmError::Transaction(format!("unexpected transaction state {s} after rollback")))
        }
    }
}
