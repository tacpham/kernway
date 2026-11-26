//! Statement execution: EXECUTE / FETCH requests, the response messages they
//! produce (describe, rows, bit vectors, PL/SQL OUT binds), and
//! commit / rollback / logoff.

use kernway_orm_core::error::OrmError;

use crate::buf::{PResult, ParseError, Reader, Writer};
use crate::conn::{common_message, func, msg, CallStatus, Conn};
use crate::stmt::{self, Kind};
use crate::tns::packet_type;
use crate::value::{self, ora_type, Value};

const OPT_PARSE: u32 = 0x01;
const OPT_BIND: u32 = 0x08;
const OPT_EXECUTE: u32 = 0x20;
const OPT_FETCH: u32 = 0x40;
const OPT_PLSQL_BIND: u32 = 0x400;
const OPT_NOT_PLSQL: u32 = 0x8000;
const EXEC_FLAG_IMPLICIT_RESULTSET: u32 = 0x8000;
const BIND_USE_INDICATORS: u8 = 0x01;
const BIND_DIR_INPUT: u8 = 32;
const ERR_NO_DATA_FOUND: u32 = 1403;
/// Rows requested with the execute of a new query, and per FETCH.
const PREFETCH_ROWS: u32 = 2;
const FETCH_ARRAY_SIZE: u32 = 100;

/// The declared type of an OUT (or IN OUT) bind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutType {
    /// VARCHAR2 of at most this many bytes.
    Str(u32),
    /// NUMBER.
    Number,
    /// BINARY_DOUBLE.
    Float,
    /// DATE.
    Date,
    /// TIMESTAMP.
    Timestamp,
    /// BOOLEAN (PL/SQL, or SQL on 23ai+).
    Bool,
    /// RAW of at most this many bytes.
    Raw(u32),
}

/// A bind variable, by position.
#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    /// An input value.
    In(Value),
    /// A PL/SQL OUT parameter of this type.
    Out(OutType),
    /// A PL/SQL IN OUT parameter: input value and output type.
    InOut(Value, OutType),
}

impl<T: Into<Value>> From<T> for Bind {
    fn from(v: T) -> Self {
        Bind::In(v.into())
    }
}

/// Column metadata from DESCRIBE_INFO.
#[derive(Debug, Clone, Default)]
pub struct Column {
    /// Column name (upper case unless quoted in the SQL).
    pub name: String,
    /// Oracle type number (2 NUMBER, 1 VARCHAR2, 12 DATE, 180 TIMESTAMP, …).
    pub ora_type: u8,
    /// Character set form: 0 none, 1 database charset, 2 national charset.
    pub csfrm: u8,
    /// Maximum size in bytes the server reserves for the column.
    pub buffer_size: u32,
    /// NUMBER precision (0 when unspecified).
    pub precision: i8,
    /// NUMBER scale (-127 for FLOAT).
    pub scale: i8,
    /// Whether the column accepts NULL.
    pub nullable: bool,
}

/// The outcome of [`Conn::execute`].
#[derive(Debug, Clone, Default)]
pub struct Executed {
    /// Result columns (queries).
    pub columns: Vec<Column>,
    /// All rows (queries).
    pub rows: Vec<Vec<Value>>,
    /// Rows affected (DML) or fetched (queries).
    pub row_count: u64,
    /// One entry per bind: the returned value of an OUT / IN OUT bind, `Null`
    /// for an IN bind.
    pub out: Vec<Value>,
}

fn out_meta(t: OutType) -> (u8, u8, u32) {
    match t {
        OutType::Str(n) => (ora_type::VARCHAR, 1, n.max(1) * 4),
        OutType::Number => (ora_type::NUMBER, 0, 22),
        OutType::Float => (ora_type::BINARY_DOUBLE, 0, 8),
        OutType::Date => (ora_type::DATE, 0, 7),
        OutType::Timestamp => (ora_type::TIMESTAMP, 0, 11),
        OutType::Bool => (ora_type::BOOLEAN, 0, 4),
        OutType::Raw(n) => (ora_type::RAW, 0, n.max(1)),
    }
}

fn value_meta(v: &Value) -> (u8, u8, u32) {
    match v {
        Value::Null => (ora_type::VARCHAR, 1, 4),
        Value::Str(s) => (ora_type::VARCHAR, 1, (s.len() as u32 * 4).max(4)),
        Value::Number(_) => (ora_type::NUMBER, 0, 22),
        Value::Float(_) => (ora_type::BINARY_DOUBLE, 0, 8),
        Value::Date(_) => (ora_type::DATE, 0, 7),
        Value::Timestamp(_) => (ora_type::TIMESTAMP, 0, 11),
        Value::Raw(b) => (ora_type::RAW, 0, (b.len() as u32).max(1)),
        Value::Bool(_) => (ora_type::BOOLEAN, 0, 4),
    }
}

impl Bind {
    /// (ora_type, csfrm, buffer_size) sent in the bind metadata.
    fn meta(&self) -> (u8, u8, u32) {
        match self {
            Bind::In(v) => value_meta(v),
            Bind::Out(t) | Bind::InOut(_, t) => out_meta(*t),
        }
    }
}

fn write_value(w: &mut Writer, ora_type_num: u8, v: Option<&Value>) -> Result<(), String> {
    let Some(v) = v.filter(|v| !matches!(v, Value::Null)) else {
        if ora_type_num == ora_type::BOOLEAN {
            w.raw(&[0xFD, 0x01]);
        } else {
            w.u8(0);
        }
        return Ok(());
    };
    match v {
        Value::Null => unreachable!(),
        Value::Str(s) => {
            w.bytes_wl(s.as_bytes());
        }
        Value::Number(n) => {
            w.bytes_wl(&value::encode_number(n)?);
        }
        Value::Float(f) => {
            w.bytes_wl(&value::encode_double(*f));
        }
        Value::Date(d) => {
            w.bytes_wl(&value::encode_date(d));
        }
        Value::Timestamp(d) => {
            w.bytes_wl(&value::encode_timestamp(d));
        }
        Value::Raw(b) => {
            w.bytes_wl(b);
        }
        Value::Bool(b) => {
            if *b {
                w.raw(&[0x02, 0x01, 0x01]);
            } else {
                w.raw(&[0x01, 0x00]);
            }
        }
    }
    Ok(())
}

/// The EXECUTE request body (after the function header) for a new cursor.
fn write_execute(w: &mut Writer, fv: u8, sql: &str, kind: Kind, binds: &[Bind]) -> Result<(), String> {
    let is_query = kind == Kind::Query;
    let num_params = binds.len() as u64;
    let mut options = OPT_EXECUTE | OPT_PARSE;
    let exec_flags = EXEC_FLAG_IMPLICIT_RESULTSET;
    let num_iters = if is_query { PREFETCH_ROWS } else { 1 };
    if is_query {
        options |= OPT_FETCH;
    }
    if kind != Kind::PlSql {
        options |= OPT_NOT_PLSQL;
    } else if num_params > 0 {
        options |= OPT_PLSQL_BIND;
    }
    if num_params > 0 {
        options |= OPT_BIND;
    }
    w.ub(u64::from(options)).ub(0); // options, cursor id (new)
    w.u8(1).ub(sql.len() as u64); // SQL pointer + length
    w.u8(1).ub(13); // al8i4
    w.u8(0).u8(0); // al8o4, al8o4l
    w.ub(0); // prefetch buffer size
    w.ub(u64::from(num_iters)); // prefetch rows
    w.ub(0x7FFF_FFFF); // max long size
    if num_params == 0 {
        w.u8(0).ub(0);
    } else {
        w.u8(1).ub(num_params);
    }
    w.u8(0).u8(0).u8(0).u8(0).u8(0); // al8app, al8txn, al8txl, al8kv, al8kvl
    w.u8(0).ub(0); // defines
    w.ub(0); // registration id (lsb)
    w.u8(0).u8(1); // al8objlist, al8objlen
    w.u8(0).ub(0); // al8blv, al8blvl
    w.u8(0).ub(0); // al8dnam, al8dnaml
    w.ub(0); // registration id (msb)
    w.u8(0).ub(0).u8(0); // array DML row counts (off)
    if fv >= 8 {
        w.u8(0).ub(0).u8(0).ub(0).u8(0); // SQL signature, SQL ID
        if fv >= 9 {
            w.u8(0).ub(0); // chunk ids
        }
    }
    w.bytes_wl(sql.as_bytes());
    w.ub(1); // al8i4[0]: parse
    w.ub(if is_query { 0 } else { 1 }); // al8i4[1]: executions
    w.ub(0).ub(0).ub(0).ub(0).ub(0); // [2..6]
    w.ub(u64::from(is_query)); // [7]
    w.ub(0); // [8]
    w.ub(u64::from(exec_flags)); // [9]
    w.ub(0).ub(0).ub(0); // [10..12]
    if num_params > 0 {
        for b in binds {
            let (t, csfrm, size) = b.meta();
            w.u8(t).u8(BIND_USE_INDICATORS).u8(0).u8(0);
            w.ub(u64::from(size)).ub(0).ub(0).ub(0).ub(0);
            w.ub(if csfrm != 0 { 873 } else { 0 }).u8(csfrm).ub(0);
            if fv >= 8 {
                w.ub(0);
            }
        }
        w.u8(msg::ROW_DATA);
        for b in binds {
            let (t, _, _) = b.meta();
            let v = match b {
                Bind::In(v) | Bind::InOut(v, _) => Some(v),
                Bind::Out(_) => None,
            };
            write_value(w, t, v)?;
        }
    }
    Ok(())
}

/// What one execute / fetch response yielded.
#[derive(Default)]
struct Response {
    columns: Option<Vec<Column>>,
    rows: Vec<Vec<Value>>,
    /// Bind directions from the IO vector (PL/SQL).
    out_positions: Option<Vec<usize>>,
    out_values: Vec<Value>,
    status: CallStatus,
    flush_out_binds: bool,
}

/// Parse an execute or fetch response. `columns` are the known result
/// columns (for a fetch), `binds` the request's binds (for OUT values),
/// `prev_row` the last row seen (bit-vector duplicates refer to it).
fn parse_response(
    bytes: &[u8],
    fv: u8,
    eor: bool,
    columns: Option<&[Column]>,
    binds: &[Bind],
    prev_row: &[Value],
) -> PResult<Response> {
    let mut r = Reader::new(bytes);
    let mut out = Response::default();
    let mut bit_vector: Option<Vec<u8>> = None;
    let mut last_row: Vec<Value> = prev_row.to_vec();
    while r.remaining() > 0 {
        let t = r.u8()?;
        match t {
            msg::END_OF_RESPONSE => break,
            msg::DESCRIBE_INFO => {
                r.skip_bytes()?;
                out.columns = Some(parse_describe(&mut r, fv)?);
            }
            msg::ROW_HEADER => {
                r.u8()?; // flags
                r.skip_ub()?; // num requests
                r.skip_ub()?; // iteration number
                r.skip_ub()?; // num iters
                r.skip_ub()?; // buffer length
                let n = r.ub4()? as usize;
                if n > 0 {
                    r.u8()?;
                    bit_vector = Some(r.take(n)?.to_vec());
                }
                r.skip_bytes_2l()?; // rxhrid
            }
            msg::BIT_VECTOR => {
                r.ub2()?; // columns sent
                let ncols = out.columns.as_deref().or(columns).map_or(0, |c| c.len());
                bit_vector = Some(r.take(ncols.div_ceil(8))?.to_vec());
            }
            msg::ROW_DATA => {
                if let Some(positions) = &out.out_positions {
                    // PL/SQL OUT binds: one value per returned bind, each
                    // followed by its actual length.
                    for &i in positions {
                        let (ot, csfrm, _) = binds[i].meta();
                        let v = match r.bytes()? {
                            Some(b) => value::decode(ot, csfrm, b).map_err(ParseError::Invalid)?,
                            None => Value::Null,
                        };
                        let actual = r.sb()?;
                        let v = if ot == ora_type::BOOLEAN && actual < 0 { Value::Null } else { v };
                        out.out_values.push(v);
                    }
                } else {
                    let cols = out
                        .columns
                        .as_deref()
                        .or(columns)
                        .ok_or_else(|| ParseError::Invalid("row data before column metadata".into()))?;
                    let mut row = Vec::with_capacity(cols.len());
                    for (i, c) in cols.iter().enumerate() {
                        let duplicate = bit_vector
                            .as_ref()
                            .is_some_and(|bv| bv.get(i / 8).is_some_and(|byte| byte & (1 << (i % 8)) == 0));
                        if duplicate {
                            row.push(last_row.get(i).cloned().unwrap_or(Value::Null));
                            continue;
                        }
                        row.push(read_column(&mut r, c)?);
                    }
                    bit_vector = None;
                    last_row = row.clone();
                    out.rows.push(row);
                }
            }
            msg::IO_VECTOR => {
                r.u8()?;
                let n16 = r.ub2()? as usize;
                let n32 = r.ub4()? as usize;
                let num_binds = n32 * 256 + n16;
                r.skip_ub()?;
                r.skip_ub()?;
                let n = r.ub2()? as usize;
                r.skip(n)?;
                let n = r.ub2()? as usize;
                r.skip(n)?;
                let mut positions = Vec::new();
                for i in 0..num_binds {
                    if r.u8()? != BIND_DIR_INPUT {
                        positions.push(i);
                    }
                }
                out.out_positions = Some(positions);
            }
            msg::PARAMETER => {
                let n = r.ub2()?;
                for _ in 0..n {
                    r.skip_ub()?;
                }
                let n = r.ub2()? as usize;
                r.skip(n)?;
                let pairs = r.ub2()?;
                for _ in 0..pairs {
                    if r.ub2()? > 0 {
                        r.skip_bytes()?;
                    }
                    if r.ub2()? > 0 {
                        r.skip_bytes()?;
                    }
                    r.ub2()?;
                }
                let n = r.ub2()? as usize;
                r.skip(n)?;
            }
            msg::FLUSH_OUT_BINDS => {
                out.flush_out_binds = true;
                break;
            }
            msg::IMPLICIT_RESULTSET => {
                return Err(ParseError::Invalid("implicit result sets are not supported yet".into()));
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

fn read_column(r: &mut Reader<'_>, c: &Column) -> PResult<Value> {
    use ora_type::*;
    if c.buffer_size == 0 && !matches!(c.ora_type, LONG | LONG_RAW | UROWID) {
        return Ok(Value::Null);
    }
    match c.ora_type {
        CLOB | BLOB | JSON | VECTOR | CURSOR | ROWID | UROWID => Err(ParseError::Invalid(format!(
            "column {} has a type not supported yet ({}); convert it in SQL (TO_CHAR, DBMS_LOB.SUBSTR, ROWIDTOCHAR)",
            c.name, c.ora_type
        ))),
        _ => {
            let v = match r.bytes()? {
                Some(b) => value::decode(c.ora_type, c.csfrm, b).map_err(ParseError::Invalid)?,
                None => Value::Null,
            };
            if matches!(c.ora_type, LONG | LONG_RAW) {
                r.sb()?;
                r.skip_ub()?;
            }
            Ok(v)
        }
    }
}

fn parse_describe(r: &mut Reader<'_>, fv: u8) -> PResult<Vec<Column>> {
    r.skip_ub()?; // max row size
    let n = r.ub4()? as usize;
    if n > 0 {
        r.u8()?;
    }
    let mut cols = Vec::with_capacity(n);
    for _ in 0..n {
        let mut c = Column { ora_type: r.u8()?, ..Default::default() };
        r.u8()?; // flags
        c.precision = r.u8()? as i8;
        c.scale = r.u8()? as i8;
        c.buffer_size = r.ub4()?;
        r.skip_ub()?; // max array elements
        r.skip_ub()?; // cont flags
        r.skip_bytes_2l()?; // OID
        r.skip_ub()?; // version
        r.skip_ub()?; // charset id
        c.csfrm = r.u8()?;
        r.skip_ub()?; // max size
        if fv >= 8 {
            r.skip_ub()?; // oaccolid
        }
        c.nullable = r.u8()? != 0;
        r.u8()?; // v7 name length
        c.name = r.str_2l()?.unwrap_or_default();
        r.str_2l()?; // schema
        r.str_2l()?; // type name
        r.skip_ub()?; // position
        r.skip_ub()?; // uds flags
        if fv >= 17 {
            r.str_2l()?; // domain schema
            r.str_2l()?; // domain name
        }
        if fv >= 20 && r.ub4()? > 0 {
            r.u8()?;
            let count = r.ub4()?;
            r.u8()?;
            for _ in 0..count {
                r.str_2l()?;
                r.str_2l()?;
                r.skip_ub()?;
            }
            r.skip_ub()?;
        }
        if fv >= 24 {
            r.skip_ub()?; // vector dimensions
            r.u8()?; // format
            r.u8()?; // flags
        }
        cols.push(c);
    }
    r.skip_bytes_2l()?; // current date
    r.skip_ub()?; // dcbflag
    r.skip_ub()?; // dcbmdbz
    r.skip_ub()?; // dcbmnpr
    r.skip_ub()?; // dcbmxpr
    r.skip_bytes_2l()?; // dcbqcky
    Ok(cols)
}

fn server_error(e: &crate::conn::ServerError) -> OrmError {
    let msg = if e.message.starts_with("ORA-") { e.message.clone() } else { format!("ORA-{:05}: {}", e.code, e.message) };
    match e.code {
        1 => OrmError::UniqueViolation { field: unique_field(&e.message) },
        2291 | 2292 => OrmError::ForeignKeyViolation,
        _ => OrmError::Query(msg),
    }
}

/// Constraint name from "ORA-00001: unique constraint (SCHEMA.NAME) violated".
fn unique_field(m: &str) -> String {
    m.split_once('(')
        .and_then(|(_, r)| r.split_once(')'))
        .map(|(n, _)| n.rsplit('.').next().unwrap_or(n).to_string())
        .unwrap_or_else(|| "unknown".into())
}

impl Conn {
    /// Write the function header for `code`, preceded by a close-cursors
    /// piggyback when cursors are waiting to be released.
    fn begin_call(&mut self, w: &mut Writer, code: u8) {
        if !self.pending_close.is_empty() {
            let seq = self.next_seq();
            w.u8(msg::PIGGYBACK).u8(func::CLOSE_CURSORS).u8(seq);
            if self.field_version >= 18 {
                w.ub(0);
            }
            w.u8(1).ub(self.pending_close.len() as u64);
            for id in std::mem::take(&mut self.pending_close) {
                w.ub(u64::from(id));
            }
        }
        self.function_header(w, code);
    }

    /// Execute one SQL statement or PL/SQL block with positional `binds`
    /// (one per placeholder: every occurrence in SQL, each distinct name once
    /// in PL/SQL). Queries return all rows; PL/SQL returns OUT values.
    pub async fn execute(&mut self, sql: &str, binds: &[Bind]) -> Result<Executed, OrmError> {
        let kind = stmt::classify(sql);
        let names = stmt::bind_names(sql, kind);
        if names.len() != binds.len() {
            return Err(OrmError::Query(format!(
                "statement has {} bind placeholder(s) {:?} but {} value(s) were given",
                names.len(),
                names,
                binds.len()
            )));
        }
        if kind != Kind::PlSql && binds.iter().any(|b| !matches!(b, Bind::In(_))) {
            return Err(OrmError::Query("OUT binds are only supported in PL/SQL blocks".into()));
        }
        let mut w = Writer::new();
        self.begin_call(&mut w, func::EXECUTE);
        write_execute(&mut w, self.field_version, sql, kind, binds).map_err(OrmError::TypeConversion)?;
        self.send_data(&w.buf).await?;

        let mut resp = self.read_call(None, binds, &[]).await?;
        while resp.flush_out_binds {
            self.send_data(&[msg::FLUSH_OUT_BINDS]).await?;
            let more = self.read_call(None, binds, &[]).await?;
            resp.out_values.extend(more.out_values);
            resp.status = more.status;
            resp.flush_out_binds = more.flush_out_binds;
            if resp.out_positions.is_none() {
                resp.out_positions = more.out_positions;
            }
        }

        let cursor_id = resp.status.cursor_id;
        let mut result = Executed { columns: resp.columns.unwrap_or_default(), rows: resp.rows, ..Default::default() };
        let mut status = resp.status;
        if kind == Kind::Query {
            loop {
                match &status.error {
                    Some(e) if e.code == ERR_NO_DATA_FOUND => break,
                    Some(e) => {
                        self.queue_close(cursor_id);
                        return Err(server_error(e));
                    }
                    None => {}
                }
                let mut w = Writer::new();
                self.begin_call(&mut w, func::FETCH);
                w.ub(u64::from(cursor_id)).ub(u64::from(FETCH_ARRAY_SIZE));
                self.send_data(&w.buf).await?;
                let prev = result.rows.last().cloned().unwrap_or_default();
                let cols = result.columns.clone();
                let r = self.read_call(Some(&cols), &[], &prev).await?;
                result.rows.extend(r.rows);
                status = r.status;
            }
        } else if let Some(e) = &status.error {
            self.queue_close(cursor_id);
            return Err(server_error(e));
        }
        self.queue_close(cursor_id);
        result.row_count = if kind == Kind::Query { result.rows.len() as u64 } else { status.row_count };
        result.out = vec![Value::Null; binds.len()];
        if let Some(positions) = resp.out_positions {
            for (pos, v) in positions.into_iter().zip(resp.out_values) {
                if let Some(slot) = result.out.get_mut(pos) {
                    *slot = v;
                }
            }
        }
        Ok(result)
    }

    fn queue_close(&mut self, cursor_id: u16) {
        if cursor_id != 0 {
            self.pending_close.push(u32::from(cursor_id));
        }
    }

    async fn read_call(&mut self, columns: Option<&[Column]>, binds: &[Bind], prev_row: &[Value]) -> Result<Response, OrmError> {
        let (fv, eor) = (self.field_version, self.end_of_response());
        self.read_response(|b| parse_response(b, fv, eor, columns, binds, prev_row)).await
    }

    async fn simple_call(&mut self, code: u8) -> Result<(), OrmError> {
        let mut w = Writer::new();
        self.begin_call(&mut w, code);
        self.send_data(&w.buf).await?;
        let resp = self.read_call(None, &[], &[]).await?;
        match resp.status.error {
            Some(e) => Err(server_error(&e)),
            None => Ok(()),
        }
    }

    /// Commit the current transaction.
    pub async fn commit(&mut self) -> Result<(), OrmError> {
        self.simple_call(func::COMMIT).await
    }

    /// Roll back the current transaction.
    pub async fn rollback(&mut self) -> Result<(), OrmError> {
        self.simple_call(func::ROLLBACK).await
    }

    /// Log off and close the connection.
    pub async fn close(mut self) -> Result<(), OrmError> {
        self.simple_call(func::LOGOFF).await?;
        self.tns.send(packet_type::DATA, 0, &[0x00, 0x40]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
    }

    fn header(seq: u8) -> Writer {
        let mut w = Writer::new();
        w.u8(msg::FUNCTION).u8(func::EXECUTE).u8(seq).ub(0);
        w
    }

    #[test]
    fn execute_of_a_query_matches_the_golden_bytes() {
        let mut w = header(6);
        write_execute(&mut w, 24, "SELECT 1 FROM DUAL", Kind::Query, &[]).unwrap();
        assert_eq!(
            hex(&w.buf),
            "03 5E 06 00 02 80 61 00 01 01 12 01 01 0D 00 00 00 01 02 04 7F FF FF FF 00 00 00 00 00 00 \
             00 00 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 12 53 45 4C 45 43 54 20 31 \
             20 46 52 4F 4D 20 44 55 41 4C 01 01 00 00 00 00 00 00 01 01 00 02 80 00 00 00 00"
        );
    }

    #[test]
    fn execute_of_plsql_with_out_bind_matches_the_golden_bytes() {
        let mut w = header(7);
        let binds = [Bind::Out(OutType::Number), Bind::In(Value::from(5))];
        write_execute(&mut w, 24, "BEGIN :R := F(:A); END;", Kind::PlSql, &binds).unwrap();
        assert_eq!(
            hex(&w.buf),
            "03 5E 07 00 02 04 29 00 01 01 17 01 01 0D 00 00 00 01 01 04 7F FF FF FF 01 01 02 00 00 00 \
             00 00 00 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 17 42 45 47 49 4E 20 3A \
             52 20 3A 3D 20 46 28 3A 41 29 3B 20 45 4E 44 3B 01 01 01 01 00 00 00 00 00 00 00 02 80 00 \
             00 00 00 02 01 00 00 01 16 00 00 00 00 00 00 00 00 02 01 00 00 01 16 00 00 00 00 00 00 \
             00 00 07 00 02 C1 06"
        );
    }

    #[test]
    fn unique_violation_names_the_constraint() {
        assert_eq!(unique_field("ORA-00001: unique constraint (KERNWAY.SYS_C008) violated"), "SYS_C008");
    }
}
