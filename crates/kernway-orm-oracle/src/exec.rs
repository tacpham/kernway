//! Statement execution: EXECUTE / FETCH requests, the response messages they
//! produce (describe, rows, bit vectors, PL/SQL OUT binds), and
//! commit / rollback / logoff.

use kernway_orm_core::error::OrmError;
use std::collections::HashMap;

use crate::buf::{PResult, ParseError, Reader, Writer};
use crate::conn::{common_message, func, msg, CallStatus, Conn};
use crate::stmt::{self, Kind};
use crate::tns::packet_type;
use crate::value::{self, ora_type, ResultSet, Value};

const OPT_PARSE: u32 = 0x01;
const OPT_DEFINE: u32 = 0x10;
const OPT_BIND: u32 = 0x08;
const OPT_EXECUTE: u32 = 0x20;
const OPT_FETCH: u32 = 0x40;
const OPT_COMMIT: u32 = 0x100;
const OPT_PLSQL_BIND: u32 = 0x400;
const OPT_NOT_PLSQL: u32 = 0x8000;
const EXEC_FLAG_IMPLICIT_RESULTSET: u32 = 0x8000;
const BIND_USE_INDICATORS: u8 = 0x01;
const BIND_DIR_INPUT: u8 = 32;
const ERR_NO_DATA_FOUND: u32 = 1403;
/// Rows requested with the execute of a new query, and per FETCH.
const PREFETCH_ROWS: u32 = 2;
/// Buffer size declared for a LONG / LONG RAW define (CLOB / BLOB fetched
/// inline) and for long binds.
const MAX_LONG_LENGTH: u64 = 0x7FFF_FFFF;
const FETCH_ARRAY_SIZE: u32 = 100;
/// Column define flag: prefetch the LOB's whole content with the row.
const LOB_PREFETCH_FLAG: u64 = 0x200_0000;
/// Largest JSON / VECTOR image prefetched with a row.
const JSON_MAX_LENGTH: u64 = 32 * 1024 * 1024;
const VECTOR_MAX_LENGTH: u64 = 1024 * 1024;

/// Receives each fetched row with its columns.
type RowSink<'a> = dyn FnMut(&[Column], Vec<Value>) -> Result<(), OrmError> + 'a;

/// The declared type of an OUT (or IN OUT) bind.
#[derive(Debug, Clone, PartialEq)]
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
    /// SYS_REFCURSOR: returned as [`Value::Cursor`] with every row fetched.
    Cursor,
    /// TIMESTAMP WITH TIME ZONE.
    TimestampTz,
    /// INTERVAL YEAR TO MONTH.
    IntervalYm,
    /// INTERVAL DAY TO SECOND.
    IntervalDs,
    /// CLOB of any size: returned as [`Value::Str`] (read through its locator).
    Clob,
    /// BLOB of any size: returned as [`Value::Raw`].
    Blob,
    /// An object type, collection or PL/SQL record, by its full name
    /// (`SCHEMA.[PACKAGE.]NAME`): returned as [`Value::Object`] /
    /// [`Value::Collection`] / [`Value::AssocArray`].
    Object(String),
}

impl OutType {
    /// The object type an OUT bind of this type needs, if any.
    pub(crate) fn object_type(&self) -> Option<&str> {
        match self {
            OutType::Object(n) => Some(n),
            _ => None,
        }
    }
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
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// The type the column is fetched as after a define (0 = as described):
    /// CLOB as LONG, BLOB as LONG RAW, JSON / VECTOR as text, ROWID and
    /// INTERVAL as their text form.
    pub wire_type: u8,
    /// Character set form of [`wire_type`](Self::wire_type).
    pub wire_csfrm: u8,
    /// For an object / collection / XMLType column: its type
    /// (`SCHEMA.NAME`).
    pub type_name: Option<String>,
}

impl Column {
    /// (type, csfrm) the values arrive in.
    fn wire(&self) -> (u8, u8) {
        if self.wire_type != 0 {
            (self.wire_type, self.wire_csfrm)
        } else {
            (self.ora_type, self.csfrm)
        }
    }
}

/// Statements kept parsed per session (as python-oracledb's default).
pub(crate) const STMT_CACHE_SIZE: usize = 20;

/// A parsed cursor kept for re-execution: its id and (queries) the result
/// columns as defined — the server neither re-parses nor re-describes it.
#[derive(Debug, Clone)]
pub(crate) struct CachedStmt {
    pub(crate) cursor_id: u16,
    pub(crate) columns: Vec<Column>,
}

/// A query cursor left open on the server (see [`Conn::open_cursor`]).
#[derive(Debug, Clone)]
pub struct OpenCursor {
    id: u16,
    columns: Vec<Column>,
    /// The last row fetched: the server sends repeated column values only once.
    last_row: Vec<Value>,
}

/// A cursor of the shared `SqlConn` cursor API: rows the server already sent,
/// and the cursor still open on the server (`None` once exhausted).
pub(crate) struct CursorState {
    pub(crate) columns: Vec<Column>,
    pub(crate) buffered: std::collections::VecDeque<Vec<Value>>,
    pub(crate) open: Option<OpenCursor>,
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
    /// Result sets a PL/SQL block returned with `DBMS_SQL.RETURN_RESULT`,
    /// each fetched to the end.
    pub implicit: Vec<ResultSet>,
    /// A warning the statement succeeded with: Oracle error code and message
    /// (ORA-24344 = created with compilation errors).
    pub warning: Option<(u16, String)>,
}

fn out_meta(t: &OutType) -> (u8, u8, u32) {
    match t {
        OutType::Str(n) => (ora_type::VARCHAR, 1, (*n).max(1) * 4),
        OutType::Number => (ora_type::NUMBER, 0, 22),
        OutType::Float => (ora_type::BINARY_DOUBLE, 0, 8),
        OutType::Date => (ora_type::DATE, 0, 7),
        OutType::Timestamp => (ora_type::TIMESTAMP, 0, 11),
        OutType::Bool => (ora_type::BOOLEAN, 0, 4),
        OutType::Raw(n) => (ora_type::RAW, 0, (*n).max(1)),
        OutType::Cursor => (ora_type::CURSOR, 0, 4),
        OutType::TimestampTz => (ora_type::TIMESTAMP_TZ, 0, 13),
        OutType::IntervalYm => (ora_type::INTERVAL_YM, 0, 5),
        OutType::IntervalDs => (ora_type::INTERVAL_DS, 0, 11),
        OutType::Clob => (ora_type::CLOB, 1, LOB_BIND_SIZE),
        OutType::Blob => (ora_type::BLOB, 0, LOB_BIND_SIZE),
        OutType::Object(_) => (ora_type::OBJECT, 0, 0),
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
        Value::TimestampTz(..) => (ora_type::TIMESTAMP_TZ, 0, 13),
        Value::IntervalYm { .. } => (ora_type::INTERVAL_YM, 0, 5),
        Value::IntervalDs { .. } => (ora_type::INTERVAL_DS, 0, 11),
        Value::Raw(b) => (ora_type::RAW, 0, (b.len() as u32).max(1)),
        Value::Bool(_) => (ora_type::BOOLEAN, 0, 4),
        Value::Cursor(_) => (ora_type::CURSOR, 0, 4),
        Value::TimestampTzRegion(..) | Value::TimestampTzRegionId(..) => (ora_type::TIMESTAMP_TZ, 0, 13),
        Value::Object { .. } | Value::Collection { .. } | Value::AssocArray { .. } | Value::ObjectImage { .. } | Value::ObjectBind { .. } => {
            (ora_type::OBJECT, 0, 0)
        }
        Value::Lob(l) if l.blob => (ora_type::BLOB, 0, LOB_BIND_SIZE),
        Value::Lob(_) => (ora_type::CLOB, 1, LOB_BIND_SIZE),
    }
}

/// Bind buffer size of a LOB locator.
const LOB_BIND_SIZE: u32 = 112;
/// Longest value a PL/SQL VARCHAR2 / RAW bind can carry; past it, a value
/// travels as a temporary LOB.
const PLSQL_MAX_BIND: usize = 32767;

impl Bind {
    /// (ora_type, csfrm, buffer_size) sent in the bind metadata.
    fn meta(&self) -> (u8, u8, u32) {
        match self {
            Bind::In(v) => value_meta(v),
            Bind::Out(t) | Bind::InOut(_, t) => out_meta(t),
        }
    }

    /// The object type of this bind (an object OUT type or an object value).
    pub(crate) fn object_type(&self) -> Option<&str> {
        match self {
            Bind::Out(t) | Bind::InOut(_, t) if t.object_type().is_some() => t.object_type(),
            Bind::In(Value::ObjectBind { type_name, .. }) | Bind::InOut(Value::ObjectBind { type_name, .. }, _) => Some(type_name),
            _ => None,
        }
    }
}

/// Type OID and version of each object type bound, by full name.
pub(crate) type ObjectBindTypes = HashMap<String, (Vec<u8>, u32)>;

fn write_value(w: &mut Writer, ora_type_num: u8, v: Option<&Value>) -> Result<(), String> {
    let Some(v) = v.filter(|v| !matches!(v, Value::Null)) else {
        if ora_type_num == ora_type::BOOLEAN {
            w.raw(&[0xFD, 0x01]);
        } else if ora_type_num == ora_type::CURSOR {
            w.raw(&[0x01, 0x00]);
        } else if ora_type_num == ora_type::OBJECT {
            // TOID, OID, snapshot, version, image length, flags (top level).
            w.ub(0).ub(0).ub(0).ub(0).ub(0).ub(u64::from(crate::object::BIND_FLAGS));
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
        Value::TimestampTz(d, off) => {
            w.bytes_wl(&value::encode_timestamp_tz(d, *off));
        }
        Value::IntervalYm { years, months } => {
            let mut b = ((i64::from(*years) + 0x8000_0000) as u32).to_be_bytes().to_vec();
            b.push((months + 60) as u8);
            w.bytes_wl(&b);
        }
        Value::IntervalDs { days, hours, minutes, seconds, nanos } => {
            let mut b = ((i64::from(*days) + 0x8000_0000) as u32).to_be_bytes().to_vec();
            b.extend([(hours + 60) as u8, (minutes + 60) as u8, (seconds + 60) as u8]);
            b.extend(((i64::from(*nanos) + 0x8000_0000) as u32).to_be_bytes());
            w.bytes_wl(&b);
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
        Value::Cursor(_) => return Err("a cursor cannot be passed as an input bind".into()),
        Value::Lob(l) => {
            w.bytes_2l(&l.locator);
        }
        // Bound by its offset at that instant (the same point in time).
        Value::TimestampTzRegion(d, off, _) => {
            w.bytes_wl(&value::encode_timestamp_tz(d, *off));
        }
        Value::TimestampTzRegionId(..) => return Err("an unresolved time-zone region cannot be bound".into()),
        // Pickled (with its type) by the bind preparation.
        Value::ObjectBind { bytes, .. } => {
            w.raw(bytes);
        }
        Value::ObjectImage { .. } => return Err("a received object image cannot be bound".into()),
        Value::Object { .. } | Value::Collection { .. } | Value::AssocArray { .. } => {
            return Err("an object value must be bound through a typed argument".into())
        }
    }
    Ok(())
}

/// Bind metadata for one placeholder across every row of an execution: the
/// type of the first non-NULL value, the largest size.
fn merged_meta(rows: &[&[Bind]], i: usize) -> Result<(u8, u8, u32), String> {
    let mut meta: Option<(u8, u8, u32)> = None;
    let mut max = 0;
    for row in rows {
        let b = &row[i];
        let m = b.meta();
        max = max.max(m.2);
        let is_null = matches!(b, Bind::In(Value::Null));
        match meta {
            None if !is_null => meta = Some(m),
            Some((t, _, _)) if !is_null && t != m.0 => {
                return Err(format!("bind {} mixes value types across rows ({t} and {})", i + 1, m.0));
            }
            _ => {}
        }
    }
    let (t, csfrm, _) = meta.unwrap_or((ora_type::VARCHAR, 1, 4));
    Ok((t, csfrm, max))
}

#[allow(clippy::too_many_arguments)] // the EXECUTE message's own fields
/// The EXECUTE request body (after the function header) for a new cursor,
/// run once per row of `rows` (array DML when there are several).
fn write_execute(
    w: &mut Writer,
    fv: u8,
    max_string_size: u32,
    sql: &str,
    kind: Kind,
    rows: &[&[Bind]],
    autocommit: bool,
    objects: &ObjectBindTypes,
    reuse: Option<u16>,
) -> Result<(), String> {
    let is_query = kind == Kind::Query;
    let binds: &[Bind] = rows.first().copied().unwrap_or(&[]);
    let num_params = binds.len() as u64;
    let executions = rows.len().max(1) as u64;
    let metas: Vec<(u8, u8, u32)> = (0..binds.len()).map(|i| merged_meta(rows, i)).collect::<Result<_, _>>()?;
    // A cached cursor is executed again by id: no parse, no SQL text.
    let mut options = if reuse.is_some() { OPT_EXECUTE } else { OPT_EXECUTE | OPT_PARSE };
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
    if autocommit {
        options |= OPT_COMMIT;
    }
    w.ub(u64::from(options)).ub(u64::from(reuse.unwrap_or(0))); // options, cursor id (0 = new)
    if reuse.is_some() {
        w.u8(0).ub(0); // no SQL
    } else {
        w.u8(1).ub(sql.len() as u64); // SQL pointer + length
    }
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
    if reuse.is_none() {
        w.bytes_wl(sql.as_bytes());
    }
    w.ub(u64::from(reuse.is_none())); // al8i4[0]: parse
    // al8i4[1]: executions (a re-executed query: rows to prefetch).
    w.ub(match (is_query, reuse) {
        (true, None) => 0,
        (true, Some(_)) => u64::from(num_iters),
        (false, _) => executions,
    });
    w.ub(0).ub(0).ub(0).ub(0).ub(0); // [2..6]
    w.ub(u64::from(is_query)); // [7]
    w.ub(0); // [8]
    w.ub(u64::from(exec_flags)); // [9]
    w.ub(0).ub(0).ub(0); // [10..12]
    if num_params > 0 {
        for (i, &(t, csfrm, size)) in metas.iter().enumerate() {
            w.u8(t).u8(BIND_USE_INDICATORS).u8(0).u8(0);
            let cont = if matches!(t, ora_type::CLOB | ora_type::BLOB) { LOB_PREFETCH_FLAG } else { 0 };
            w.ub(u64::from(size)).ub(0).ub(cont);
            match binds[i].object_type() {
                // An object bind names its type: OID and version.
                Some(name) => {
                    let (oid, version) = objects.get(name).ok_or_else(|| format!("object type {name} not loaded"))?;
                    w.bytes_2l(oid).ub(u64::from(*version));
                }
                None => {
                    w.ub(0).ub(0);
                }
            }
            w.ub(if csfrm != 0 { 873 } else { 0 }).u8(csfrm).ub(0);
            if fv >= 8 {
                w.ub(0);
            }
        }
        if kind == Kind::PlSql {
            if let Some(&(_, csfrm, size)) = metas.iter().find(|m| m.2 > max_string_size && is_text_or_raw(m.0)) {
                return Err(format!(
                    "a {}-byte value is too long for a PL/SQL bind (limit {max_string_size}); \
                     write it with SQL (INSERT / UPDATE) instead",
                    size / if csfrm != 0 { 4 } else { 1 }
                ));
            }
        }
        // In SQL, values longer than the server's string limit are "long"
        // binds: sent after all the others (in their original order). The
        // server converts them for CLOB / BLOB columns.
        let is_long = |m: &(u8, u8, u32)| kind != Kind::PlSql && m.2 > max_string_size && is_text_or_raw(m.0);
        for row in rows {
            w.u8(msg::ROW_DATA);
            for long_pass in [false, true] {
                for (b, m) in row.iter().zip(&metas).filter(|(_, m)| is_long(m) == long_pass) {
                    let v = match b {
                        Bind::In(v) | Bind::InOut(v, _) => Some(v),
                        Bind::Out(_) => None,
                    };
                    write_value(w, m.0, v)?;
                }
            }
        }
    }
    Ok(())
}

fn is_object_value(v: &Value) -> bool {
    matches!(v, Value::Object { .. } | Value::Collection { .. } | Value::AssocArray { .. })
}

fn is_unpacked_object(b: &Bind) -> bool {
    matches!(b, Bind::In(v) | Bind::InOut(v, _) if is_object_value(v))
}

/// The type of an object IN value: its own, else the IN OUT bind's.
fn object_value_type(v: &Value, b: &Bind) -> Option<String> {
    let own = match v {
        Value::Object { type_name, .. } | Value::Collection { type_name, .. } | Value::AssocArray { type_name, .. } => type_name.clone(),
        _ => String::new(),
    };
    if !own.is_empty() {
        return Some(own);
    }
    match b {
        Bind::InOut(_, OutType::Object(n)) if !n.is_empty() => Some(n.clone()),
        _ => None,
    }
}

/// Whether a PL/SQL bind must travel as a temporary LOB: a CLOB / BLOB
/// IN OUT argument with an input value, or an IN value past the bind limit.
fn needs_temp_lob(b: &Bind) -> bool {
    let big = |v: &Value| match v {
        Value::Str(s) => s.len() > PLSQL_MAX_BIND,
        Value::Raw(r) => r.len() > PLSQL_MAX_BIND,
        _ => false,
    };
    match b {
        Bind::In(v) => big(v),
        Bind::InOut(v, OutType::Clob | OutType::Blob) => !matches!(v, Value::Null | Value::Lob(_)),
        Bind::InOut(v, _) => big(v),
        Bind::Out(_) => false,
    }
}

fn is_text_or_raw(t: u8) -> bool {
    matches!(t, ora_type::VARCHAR | ora_type::RAW)
}

/// The type a column is fetched as: CLOB / NCLOB inline as LONG, BLOB as
/// LONG RAW; JSON and VECTOR with their whole binary image prefetched
/// (decoded by `oson`); everything else as described.
fn define_type(c: &Column) -> (u8, u8, u64) {
    use ora_type::*;
    match c.ora_type {
        CLOB => (LONG, c.csfrm.max(1), MAX_LONG_LENGTH),
        BLOB => (LONG_RAW, 0, MAX_LONG_LENGTH),
        JSON => (JSON, 0, JSON_MAX_LENGTH),
        VECTOR => (VECTOR, 0, VECTOR_MAX_LENGTH),
        t => (t, c.csfrm, u64::from(c.buffer_size)),
    }
}

fn converted_by_define(t: u8) -> bool {
    use ora_type::*;
    matches!(t, CLOB | BLOB | JSON | VECTOR)
}

/// The first column whose type this driver cannot fetch (object types,
/// XMLType, BFILE, nested cursors …) — refused up front instead of
/// returning garbage.
fn unsupported_column(cols: &[Column]) -> Option<&Column> {
    use ora_type::*;
    cols.iter().find(|c| {
        !converted_by_define(c.ora_type)
            && !matches!(
                c.ora_type,
                VARCHAR | NUMBER | BINARY_INTEGER | LONG | DATE | RAW | LONG_RAW | CHAR | BINARY_FLOAT
                    | BINARY_DOUBLE | TIMESTAMP | TIMESTAMP_TZ | TIMESTAMP_LTZ | BOOLEAN | JSON | VECTOR | INTERVAL_YM | INTERVAL_DS | ROWID | UROWID
                    | CURSOR | BFILE | OBJECT
            )
    })
}

fn unsupported_error(c: &Column) -> OrmError {
    let what = match c.ora_type {
        ora_type::OBJECT => "an object type / XMLType",
        ora_type::BFILE => "BFILE",
        ora_type::CURSOR => "a nested cursor",
        _ => "a type this driver does not decode",
    };
    OrmError::Unsupported(format!(
        "column {} is {what} (Oracle type {}); convert it in SQL (e.g. TO_CHAR, XMLSERIALIZE, JSON_SERIALIZE)",
        c.name, c.ora_type
    ))
}

/// Set the types the rows will arrive in after a define.
fn apply_define(cols: &mut [Column]) {
    for c in cols {
        let (t, csfrm, _) = define_type(c);
        c.wire_type = t;
        c.wire_csfrm = csfrm;
        c.buffer_size = c.buffer_size.max(1);
    }
}

/// An EXECUTE that fetches the next rows of an open cursor without SQL text
/// (a REF CURSOR).
fn write_cursor_fetch(w: &mut Writer, fv: u8, cursor_id: u16) {
    w.ub(u64::from(OPT_FETCH | OPT_NOT_PLSQL)).ub(u64::from(cursor_id));
    w.u8(0).ub(0); // no SQL
    w.u8(1).ub(13);
    w.u8(0).u8(0);
    w.ub(0);
    w.ub(u64::from(FETCH_ARRAY_SIZE));
    w.ub(MAX_LONG_LENGTH);
    w.u8(0).ub(0); // binds
    w.u8(0).u8(0).u8(0).u8(0).u8(0);
    w.u8(0).ub(0); // defines
    w.ub(0);
    w.u8(0).u8(1);
    w.u8(0).ub(0);
    w.u8(0).ub(0);
    w.ub(0);
    w.u8(0).ub(0).u8(0);
    if fv >= 8 {
        w.u8(0).ub(0).u8(0).ub(0).u8(0);
        if fv >= 9 {
            w.u8(0).ub(0);
        }
    }
    w.ub(0); // [0] no parse
    w.ub(u64::from(FETCH_ARRAY_SIZE)); // [1]
    w.ub(0).ub(0).ub(0).ub(0).ub(0);
    w.ub(1); // [7] is query
    w.ub(0);
    w.ub(0); // [9]
    w.ub(0).ub(0).ub(0);
}

/// Whether the columns need a define round trip before rows can be fetched.
fn needs_define(cols: &[Column]) -> bool {
    cols.iter().any(|c| converted_by_define(c.ora_type))
}

/// An EXECUTE that only (re)defines the output columns of an open cursor.
fn write_define(w: &mut Writer, fv: u8, cursor_id: u16, cols: &[Column]) {
    w.ub(u64::from(OPT_DEFINE | OPT_NOT_PLSQL)).ub(u64::from(cursor_id));
    w.u8(0).ub(0); // no SQL
    w.u8(1).ub(13); // al8i4
    w.u8(0).u8(0); // al8o4, al8o4l
    w.ub(0); // prefetch buffer size
    w.ub(u64::from(PREFETCH_ROWS));
    w.ub(MAX_LONG_LENGTH);
    w.u8(0).ub(0); // binds
    w.u8(0).u8(0).u8(0).u8(0).u8(0);
    w.u8(1).ub(cols.len() as u64); // defines
    w.ub(0);
    w.u8(0).u8(1);
    w.u8(0).ub(0);
    w.u8(0).ub(0);
    w.ub(0);
    w.u8(0).ub(0).u8(0);
    if fv >= 8 {
        w.u8(0).ub(0).u8(0).ub(0).u8(0);
        if fv >= 9 {
            w.u8(0).ub(0);
        }
    }
    w.ub(0); // al8i4[0]: no parse
    w.ub(u64::from(PREFETCH_ROWS)); // [1]
    w.ub(0).ub(0).ub(0).ub(0).ub(0); // [2..6]
    w.ub(1); // [7] is query
    w.ub(0); // [8]
    w.ub(0); // [9] exec flags
    w.ub(0).ub(0).ub(0); // [10..12]
    for c in cols {
        let (t, csfrm, size) = define_type(c);
        // JSON / VECTOR come as LOBs whose whole content is prefetched.
        let prefetch = match t {
            ora_type::JSON | ora_type::VECTOR => size,
            _ => 0,
        };
        let cont_flag = if prefetch > 0 { LOB_PREFETCH_FLAG } else { 0 };
        w.u8(t).u8(BIND_USE_INDICATORS).u8(0).u8(0);
        w.ub(size).ub(0).ub(cont_flag).ub(0).ub(0); // size, max elements, cont flag, OID, version
        w.ub(if csfrm != 0 { 873 } else { 0 }).u8(csfrm).ub(prefetch);
        if fv >= 8 {
            w.ub(0);
        }
    }
}

/// What one execute / fetch response yielded.
#[derive(Default)]
struct Response {
    columns: Option<Vec<Column>>,
    rows: Vec<Vec<Value>>,
    /// Bind directions from the IO vector (PL/SQL).
    out_positions: Option<Vec<usize>>,
    out_values: Vec<Value>,
    /// REF CURSORs among the OUT values: (index into `out_values`, server
    /// cursor id, columns), to be fetched after the call.
    cursors: Vec<(usize, u16, Vec<Column>)>,
    /// Implicit result sets (`DBMS_SQL.RETURN_RESULT`): cursor id, columns.
    implicit: Vec<(u16, Vec<Column>)>,
    /// Nested cursors (`CURSOR(…)` columns) the server opened, in the order
    /// their placeholders appear in `rows`: id and columns.
    nested: Vec<(u16, Vec<Column>)>,
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
                        if ot == ora_type::OBJECT {
                            // TOID, OID, snapshot, version, length, flags, image.
                            for _ in 0..3 {
                                skip_bytes_with_length(&mut r)?;
                            }
                            r.ub2()?;
                            let n = r.ub4()?;
                            r.ub2()?;
                            let v = if n > 0 {
                                let image = r.bytes()?.unwrap_or_default();
                                Value::ObjectImage { type_name: binds[i].object_type().unwrap_or_default().to_string(), image }
                            } else {
                                Value::Null
                            };
                            r.sb()?; // actual length
                            out.out_values.push(v);
                            continue;
                        }
                        if ot == ora_type::CURSOR {
                            r.u8()?; // fixed length
                            let cols = parse_describe(&mut r, fv)?;
                            let id = r.ub2()?;
                            r.sb()?; // actual length
                            out.cursors.push((out.out_values.len(), id, cols));
                            out.out_values.push(Value::Null);
                            continue;
                        }
                        if matches!(ot, ora_type::CLOB | ora_type::BLOB) {
                            // A locator: size, chunk size, then the locator.
                            let v = if r.ub4()? > 0 {
                                r.ub8()?;
                                r.ub4()?;
                                let locator = r.bytes()?.unwrap_or_default();
                                Value::Lob(crate::lob::Lob { locator, blob: ot == ora_type::BLOB, bfile: false })
                            } else {
                                Value::Null
                            };
                            r.sb()?; // actual length
                            out.out_values.push(v);
                            continue;
                        }
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
                        row.push(read_column(&mut r, c, fv, &mut out.nested)?);
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
                let n = r.ub4()?;
                for _ in 0..n {
                    let skip = r.u8()? as usize;
                    r.skip(skip)?;
                    let cols = parse_describe(&mut r, fv)?;
                    let id = r.ub2()?;
                    out.implicit.push((id, cols));
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

const ROWID_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Oracle's extended rowid text: object (6), file (3), block (6), row (3)
/// in base64 digits.
fn encode_rowid(rba: u32, partition: u16, block: u32, slot: u16) -> String {
    let mut out = String::with_capacity(18);
    for (value, digits) in [(u64::from(rba), 6), (u64::from(partition), 3), (u64::from(block), 6), (u64::from(slot), 3)] {
        for i in (0..digits).rev() {
            out.push(ROWID_ALPHABET[(value >> (6 * i) & 0x3F) as usize] as char);
        }
    }
    out
}

fn skip_bytes_with_length(r: &mut Reader<'_>) -> PResult<()> {
    if r.ub4()? > 0 {
        r.skip_bytes()?;
    }
    Ok(())
}

fn read_column(r: &mut Reader<'_>, c: &Column, fv: u8, nested: &mut Vec<(u16, Vec<Column>)>) -> PResult<Value> {
    use ora_type::*;
    let (t, csfrm) = c.wire();
    if c.buffer_size == 0 && !matches!(t, LONG | LONG_RAW | UROWID) {
        return Ok(Value::Null);
    }
    if c.wire_type == 0 {
        // Types this driver refuses are still read past exactly, so the
        // response stays in step; the caller then reports them.
        match c.ora_type {
            OBJECT => {
                // Type OID, OID, snapshot, version, image length, flags,
                // image: decoded by the caller with the type's metadata.
                for _ in 0..3 {
                    skip_bytes_with_length(r)?;
                }
                r.ub2()?;
                let n = r.ub4()?;
                r.ub2()?;
                if n == 0 {
                    return Ok(Value::Null);
                }
                let image = r.bytes()?.unwrap_or_default();
                let type_name = c.type_name.clone().unwrap_or_default();
                return Ok(Value::ObjectImage { type_name, image });
            }
            BFILE => {
                // The locator only (a BFILE has no size / chunk size here);
                // the caller reads the file through it.
                if r.ub4()? == 0 {
                    return Ok(Value::Null);
                }
                let locator = r.bytes()?.unwrap_or_default();
                return Ok(Value::Lob(crate::lob::Lob { locator, blob: true, bfile: true }));
            }
            CURSOR => {
                // A nested cursor: its columns and id; the caller fetches
                // its rows into this placeholder (a result set without columns).
                r.u8()?;
                let cols = parse_describe(r, fv)?;
                nested.push((r.ub2()?, cols));
                return Ok(Value::Cursor(ResultSet::default()));
            }
            _ => {}
        }
    }
    if t == ROWID {
        // A physical rowid as its parts; text is Oracle's 18-character base64.
        let n = r.u8()?;
        if n == 0 || n == 0xFF {
            return Ok(Value::Null);
        }
        let rba = r.ub4()?;
        let partition = r.ub2()?;
        r.u8()?;
        let block = r.ub4()?;
        let slot = r.ub2()?;
        return Ok(Value::Str(encode_rowid(rba, partition, block, slot)));
    }
    if t == UROWID {
        // Two length-prefixed chunks: a length (ignored), then the image.
        if r.bytes()?.is_none() {
            return Ok(Value::Null);
        }
        let b = r.bytes()?.unwrap_or_default();
        if b.is_empty() {
            return Ok(Value::Null);
        }
        if b[0] == 1 && b.len() < 13 {
            return Err(ParseError::Invalid("short physical UROWID".into()));
        }
        if b[0] == 1 {
            let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
            let be16 = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
            return Ok(Value::Str(encode_rowid(be32(1), be16(5), be32(7), be16(11))));
        }
        // Logical rowid (index-organized table): '*' + base64 of the rest.
        let mut out = String::from("*");
        for chunk in b[1..].chunks(3) {
            let v = chunk.iter().enumerate().fold(0u32, |a, (i, x)| a | u32::from(*x) << (16 - 8 * i));
            for k in 0..=chunk.len() {
                out.push(ROWID_ALPHABET[(v >> (18 - 6 * k) & 0x3F) as usize] as char);
            }
        }
        return Ok(Value::Str(out));
    }
    if matches!(t, JSON | VECTOR) {
        // A LOB with its whole content prefetched: size, chunk size, the
        // image, then the (unused) locator.
        if r.ub4()? == 0 {
            return Ok(Value::Null);
        }
        r.ub8()?;
        r.ub4()?;
        let image = r.bytes()?.unwrap_or_default();
        r.skip_bytes()?;
        if image.is_empty() {
            return Ok(Value::Null);
        }
        let text = if t == JSON { crate::oson::decode_oson(&image) } else { crate::oson::decode_vector(&image) };
        return text.map(Value::Str).map_err(ParseError::Invalid);
    }
    let v = match r.bytes()? {
        Some(b) => value::decode(t, csfrm, b).map_err(ParseError::Invalid)?,
        None => Value::Null,
    };
    if matches!(t, LONG | LONG_RAW) {
        r.sb()?;
        r.skip_ub()?;
    }
    Ok(v)
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
        let schema = r.str_2l()?;
        let tname = r.str_2l()?;
        if c.ora_type == ora_type::OBJECT {
            c.type_name = match (schema, tname) {
                (Some(s), Some(n)) if !s.is_empty() => Some(format!("{s}.{n}")),
                (_, n) => n,
            };
        }
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

pub(crate) fn server_error(e: &crate::conn::ServerError) -> OrmError {
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
    /// in PL/SQL). Queries return all rows; PL/SQL returns OUT values and
    /// implicit result sets.
    pub async fn execute(&mut self, sql: &str, binds: &[Bind]) -> Result<Executed, OrmError> {
        self.execute_collect(sql, binds, false).await
    }

    /// [`execute`](Self::execute), committing the transaction as part of the
    /// same call (Oracle's autocommit): one round trip instead of two.
    pub async fn execute_autocommit(&mut self, sql: &str, binds: &[Bind]) -> Result<Executed, OrmError> {
        self.execute_collect(sql, binds, true).await
    }

    async fn execute_collect(&mut self, sql: &str, binds: &[Bind], autocommit: bool) -> Result<Executed, OrmError> {
        let mut rows = Vec::new();
        let mut sink = |_: &[Column], row: Vec<Value>| {
            rows.push(row);
            Ok(())
        };
        let mut ex = self.execute_core(sql, &[binds], autocommit, &mut sink).await?;
        ex.rows = rows;
        Ok(ex)
    }

    /// Execute a statement, handing each result row to `each` as its fetch
    /// round arrives — rows are never all held in memory. The returned
    /// [`Executed`] carries columns, counts and OUT values, but no rows.
    pub async fn execute_each<F>(&mut self, sql: &str, binds: &[Bind], autocommit: bool, mut each: F) -> Result<Executed, OrmError>
    where
        F: FnMut(&[Column], Vec<Value>) -> Result<(), OrmError>,
    {
        self.execute_core(sql, &[binds], autocommit, &mut each).await
    }

    /// Run one DML statement for every row of `rows` in a single round trip
    /// (array DML); returns the total rows affected. On an error the rows
    /// before the failing one stay applied — run it in a transaction to get
    /// all or nothing.
    pub async fn execute_many(&mut self, sql: &str, rows: &[Vec<Bind>], autocommit: bool) -> Result<u64, OrmError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&[Bind]> = rows.iter().map(Vec::as_slice).collect();
        let mut sink = |_: &[Column], _: Vec<Value>| Ok(());
        Ok(self.execute_core(sql, &refs, autocommit, &mut sink).await?.row_count)
    }

    /// [`execute_inner`](Self::execute_inner) with LOBs in PL/SQL: an IN
    /// value past the 32 KB bind limit (or any IN value of a CLOB / BLOB
    /// IN OUT argument) goes in a temporary LOB, and CLOB / BLOB OUT values
    /// are read through their locators. Temporary LOBs are freed after.
    async fn execute_core(
        &mut self,
        sql: &str,
        rows: &[&[Bind]],
        autocommit: bool,
        sink: &mut RowSink<'_>,
    ) -> Result<Executed, OrmError> {
        let plsql = stmt::classify(sql) == Kind::PlSql;
        // Object / collection IN values are pickled with their types.
        let packed: Option<Vec<Vec<Bind>>> = if rows.iter().flat_map(|r| r.iter()).any(is_unpacked_object) {
            let mut all = Vec::with_capacity(rows.len());
            for row in rows {
                let mut out = Vec::with_capacity(row.len());
                for b in row.iter() {
                    out.push(match b {
                        Bind::In(v) | Bind::InOut(v, _) if is_object_value(v) => {
                            let name = object_value_type(v, b).ok_or_else(|| {
                                OrmError::Query("an object / collection value needs its type (bind it through call() or name the type)".into())
                            })?;
                            let bytes = self.pack_bind(&name, v).await?;
                            let ob = Value::ObjectBind { type_name: name.clone(), bytes };
                            match b {
                                Bind::InOut(..) => Bind::InOut(ob, OutType::Object(name)),
                                _ => Bind::In(ob),
                            }
                        }
                        other => other.clone(),
                    });
                }
                all.push(out);
            }
            Some(all)
        } else {
            None
        };
        let packed_refs: Vec<&[Bind]>;
        let rows: &[&[Bind]] = match &packed {
            Some(all) => {
                packed_refs = all.iter().map(Vec::as_slice).collect();
                &packed_refs
            }
            None => rows,
        };
        // Object binds need their types' OID and version.
        let names: Vec<String> = rows.iter().flat_map(|r| r.iter()).filter_map(|b| b.object_type().map(String::from)).collect();
        for name in names {
            self.object_type(&name).await?;
        }
        let mut temps: Vec<crate::lob::Lob> = Vec::new();
        let lobbed: Option<Vec<Bind>> = match rows {
            [one] if plsql && one.iter().any(needs_temp_lob) => {
                let mut out = Vec::with_capacity(one.len());
                for b in one.iter() {
                    out.push(match b {
                        b if !needs_temp_lob(b) => b.clone(),
                        Bind::In(v) | Bind::InOut(v, _) => {
                            let blob = match b {
                                Bind::InOut(_, OutType::Blob) => true,
                                Bind::InOut(_, OutType::Clob) => false,
                                _ => matches!(v, Value::Raw(_)),
                            };
                            let mut lob = self.lob_create_temp(blob).await?;
                            temps.push(lob.clone());
                            self.lob_write(&mut lob, v).await?;
                            match b {
                                // The output comes back the way it went in: as a LOB.
                                Bind::InOut(..) => Bind::InOut(Value::Lob(lob), if blob { OutType::Blob } else { OutType::Clob }),
                                _ => Bind::In(Value::Lob(lob)),
                            }
                        }
                        Bind::Out(_) => b.clone(),
                    });
                }
                Some(out)
            }
            _ => None,
        };
        let result = match &lobbed {
            Some(binds) => self.execute_inner(sql, &[binds.as_slice()], autocommit, sink).await,
            None => self.execute_inner(sql, rows, autocommit, sink).await,
        };
        let result = match result {
            Ok(mut ex) => {
                let mut read = Ok(());
                for v in &mut ex.out {
                    if let Value::ObjectImage { type_name, image } = v {
                        let (name, image) = (std::mem::take(type_name), std::mem::take(image));
                        match self.unpack_object(&name, &image).await {
                            Ok(decoded) => *v = decoded,
                            Err(e) => {
                                read = Err(e);
                                break;
                            }
                        }
                        continue;
                    }
                    if matches!(v, Value::TimestampTzRegionId(..)) {
                        if let Err(e) = self.resolve_region(v).await {
                            read = Err(e);
                            break;
                        }
                        continue;
                    }
                    if let Value::Lob(lob) = v {
                        let lob = lob.clone();
                        match self.lob_read_all(&lob).await {
                            Ok(data) => *v = data,
                            Err(e) => {
                                read = Err(e);
                                break;
                            }
                        }
                        if lob.is_temporary() && !temps.contains(&lob) {
                            temps.push(lob);
                        }
                    }
                }
                read.map(|()| ex)
            }
            Err(e) => Err(e),
        };
        temps.append(&mut self.call_temp_lobs);
        for lob in temps {
            if matches!(self.lob_free_temp(&lob).await, Err(OrmError::Connection(_))) {
                break;
            }
        }
        result
    }

    async fn execute_inner(
        &mut self,
        sql: &str,
        rows: &[&[Bind]],
        autocommit: bool,
        sink: &mut RowSink<'_>,
    ) -> Result<Executed, OrmError> {
        self.execute_open(sql, rows, autocommit, sink, None).await
    }

    /// Execute; with `open`, a query stops after its first round of rows and
    /// leaves the cursor open on the server (stored in `open`) for
    /// [`fetch_open`](Self::fetch_open), unless it is already exhausted.
    async fn execute_open(
        &mut self,
        sql: &str,
        rows: &[&[Bind]],
        autocommit: bool,
        sink: &mut RowSink<'_>,
        open: Option<&mut Option<OpenCursor>>,
    ) -> Result<Executed, OrmError> {
        let kind = stmt::classify(sql);
        let names = stmt::bind_names(sql, kind);
        let binds: &[Bind] = rows.first().copied().unwrap_or(&[]);
        if let Some(bad) = rows.iter().find(|r| r.len() != names.len()) {
            return Err(OrmError::Query(format!(
                "statement has {} bind placeholder(s) {:?} but {} value(s) were given",
                names.len(),
                names,
                bad.len()
            )));
        }
        if rows.len() > 1 && kind != Kind::Dml {
            return Err(OrmError::Query("several rows of binds can only be run for INSERT / UPDATE / DELETE / MERGE".into()));
        }
        if kind != Kind::PlSql && rows.iter().any(|r| r.iter().any(|b| !matches!(b, Bind::In(_)))) {
            return Err(OrmError::Query("OUT binds are only supported in PL/SQL blocks".into()));
        }
        let mut w = Writer::new();
        self.begin_call(&mut w, func::EXECUTE);
        let mut objects = ObjectBindTypes::new();
        for b in binds {
            if let Some(name) = b.object_type() {
                if let Some(t) = self.types.get(name) {
                    objects.insert(name.to_string(), (t.oid.clone(), t.version));
                }
            }
        }
        // Re-execute a cached cursor of this SQL (not a DDL, not a cursor
        // handed out to the caller).
        let cacheable = matches!(kind, Kind::Query | Kind::Dml | Kind::PlSql) && open.is_none();
        let cached = if cacheable { self.stmt_cache.remove(sql) } else { None };
        write_execute(&mut w, self.field_version, self.max_string_size, sql, kind, rows, autocommit, &objects, cached.as_ref().map(|c| c.cursor_id))
            .map_err(OrmError::TypeConversion)?;
        self.send_data(&w.buf).await?;

        let cached_cols = cached.as_ref().map(|c| c.columns.clone());
        let mut resp = self.read_call(cached_cols.as_deref(), binds, &[]).await?;
        while resp.flush_out_binds {
            self.send_data(&[msg::FLUSH_OUT_BINDS]).await?;
            let more = self.read_call(None, binds, &[]).await?;
            let base = resp.out_values.len();
            resp.cursors.extend(more.cursors.into_iter().map(|(i, id, c)| (base + i, id, c)));
            resp.out_values.extend(more.out_values);
            resp.implicit.extend(more.implicit);
            resp.status = more.status;
            resp.flush_out_binds = more.flush_out_binds;
            if resp.out_positions.is_none() {
                resp.out_positions = more.out_positions;
            }
        }

        let cursor_id = resp.status.cursor_id;
        let mut result = Executed { columns: resp.columns.take().or(cached_cols).unwrap_or_default(), ..Default::default() };
        let mut status = resp.status.clone();
        let mut fetched = 0u64;
        let mut last_row: Vec<Value> = Vec::new();
        // Hand rows to the sink; on its error stop and release the cursor.
        macro_rules! emit {
            ($rows:expr) => {
                for row in $rows {
                    fetched += 1;
                    last_row.clone_from(&row);
                    if let Err(e) = sink(&result.columns, row) {
                        self.queue_close(cursor_id);
                        return Err(e);
                    }
                }
            };
        }
        let ok_so_far = status.error.as_ref().is_none_or(|e| e.code == ERR_NO_DATA_FOUND);
        if kind == Kind::Query && ok_so_far {
            if let Some(c) = unsupported_column(&result.columns) {
                let e = unsupported_error(c);
                self.queue_close(cursor_id);
                return Err(e);
            }
        }
        if kind == Kind::Query && status.error.is_none() && cached.is_none() && needs_define(&result.columns) {
            // Redefine converted columns (LOB / JSON / VECTOR / ROWID /
            // INTERVAL). The server sends no rows before the define.
            let mut w = Writer::new();
            self.begin_call(&mut w, func::EXECUTE);
            write_define(&mut w, self.field_version, cursor_id, &result.columns);
            self.send_data(&w.buf).await?;
            apply_define(&mut result.columns);
            let cols = result.columns.clone();
            let r = self.read_call(Some(&cols), &[], &[]).await?;
            if let Some(e) = r.status.error.as_ref().filter(|e| e.code != ERR_NO_DATA_FOUND) {
                self.queue_close(cursor_id);
                return Err(server_error(e));
            }
            emit!(r.rows);
            status = r.status;
        } else {
            emit!(std::mem::take(&mut resp.rows));
        }
        if kind == Kind::Query {
            if let Some(slot) = open {
                match &status.error {
                    Some(e) if e.code == ERR_NO_DATA_FOUND => {}
                    Some(e) => {
                        self.queue_close(cursor_id);
                        return Err(server_error(e));
                    }
                    None => {
                        *slot = Some(OpenCursor { id: cursor_id, columns: result.columns.clone(), last_row });
                        result.row_count = fetched;
                        return Ok(result);
                    }
                }
            }
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
                let cols = result.columns.clone();
                let r = self.read_call(Some(&cols), &[], &last_row).await?;
                emit!(r.rows);
                status = r.status;
            }
        } else if let Some(e) = &status.error {
            self.queue_close(cursor_id);
            return Err(server_error(e));
        }
        if cacheable && cursor_id != 0 {
            if let Some(old) = self.stmt_cache.evict_for_insert() {
                self.queue_close(old.cursor_id);
            }
            self.stmt_cache.insert(sql, CachedStmt { cursor_id, columns: result.columns.clone() });
        } else {
            self.queue_close(cursor_id);
        }
        result.row_count = if kind == Kind::Query { fetched } else { status.row_count };
        result.warning = status.warning.clone().or_else(|| resp.status.warning.clone());
        // REF CURSORs: fetch each to the end and hand back its rows.
        for (idx, id, cols) in std::mem::take(&mut resp.cursors) {
            let rs = self.fetch_ref_cursor(id, cols).await?;
            if let Some(slot) = resp.out_values.get_mut(idx) {
                *slot = Value::Cursor(rs);
            }
        }
        for (id, cols) in std::mem::take(&mut resp.implicit) {
            result.implicit.push(self.fetch_ref_cursor(id, cols).await?);
        }
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

    /// Fetch every row of a REF CURSOR the server opened, then close it. A
    /// cursor has no SQL text, so rows come from EXECUTE-with-FETCH calls on
    /// its id (after a define round trip if it has LOB columns).
    async fn fetch_ref_cursor(&mut self, cursor_id: u16, mut columns: Vec<Column>) -> Result<ResultSet, OrmError> {
        if let Some(c) = unsupported_column(&columns) {
            let e = unsupported_error(c);
            self.queue_close(cursor_id);
            return Err(e);
        }
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let fv = self.field_version;
        if needs_define(&columns) {
            let mut w = Writer::new();
            self.begin_call(&mut w, func::EXECUTE);
            write_define(&mut w, fv, cursor_id, &columns);
            self.send_data(&w.buf).await?;
            apply_define(&mut columns);
            let r = self.read_call(Some(&columns), &[], &[]).await?;
            rows.extend(r.rows);
            if let Some(e) = &r.status.error {
                self.queue_close(cursor_id);
                if e.code == ERR_NO_DATA_FOUND {
                    return Ok(ResultSet { columns, rows });
                }
                return Err(server_error(e));
            }
        }
        loop {
            let mut w = Writer::new();
            self.begin_call(&mut w, func::EXECUTE);
            write_cursor_fetch(&mut w, fv, cursor_id);
            self.send_data(&w.buf).await?;
            let prev = rows.last().cloned().unwrap_or_default();
            let r = self.read_call(Some(&columns), &[], &prev).await?;
            rows.extend(r.rows);
            match &r.status.error {
                None => {}
                Some(e) if e.code == ERR_NO_DATA_FOUND => break,
                Some(e) => {
                    self.queue_close(cursor_id);
                    return Err(server_error(e));
                }
            }
        }
        self.queue_close(cursor_id);
        Ok(ResultSet { columns, rows })
    }

    /// Open a server-side cursor over a query: its first rows (already sent
    /// by the server with the execute) go to `first`; `None` comes back when
    /// those were all the rows (the cursor is then closed).
    pub async fn open_cursor(&mut self, sql: &str, binds: &[Bind], first: &mut Vec<Vec<Value>>) -> Result<(Vec<Column>, Option<OpenCursor>), OrmError> {
        if stmt::classify(sql) != Kind::Query {
            return Err(OrmError::Query("cursor_open: the statement returns no rows".into()));
        }
        let mut slot = None;
        let mut sink = |_: &[Column], row: Vec<Value>| {
            first.push(row);
            Ok(())
        };
        let ex = self.execute_open(sql, &[binds], false, &mut sink, Some(&mut slot)).await?;
        Ok((ex.columns, slot))
    }

    /// Fetch up to `max_rows` more rows of an open cursor into `rows`;
    /// returns whether the cursor is exhausted (then it is closed).
    pub async fn fetch_open(&mut self, cur: &mut OpenCursor, max_rows: u32, rows: &mut Vec<Vec<Value>>) -> Result<bool, OrmError> {
        let mut w = Writer::new();
        self.begin_call(&mut w, func::FETCH);
        w.ub(u64::from(cur.id)).ub(u64::from(max_rows.max(1)));
        self.send_data(&w.buf).await?;
        let cols = cur.columns.clone();
        let r = self.read_call(Some(&cols), &[], &cur.last_row).await?;
        if let Some(last) = r.rows.last() {
            cur.last_row.clone_from(last);
        }
        rows.extend(r.rows);
        match &r.status.error {
            Some(e) if e.code == ERR_NO_DATA_FOUND => {
                self.queue_close(cur.id);
                Ok(true)
            }
            Some(e) => {
                self.queue_close(cur.id);
                Err(server_error(e))
            }
            None => Ok(false),
        }
    }

    /// Close an open cursor (with the next call).
    pub fn close_open(&mut self, cur: OpenCursor) {
        self.queue_close(cur.id);
    }

    /// Turn a region-based TIMESTAMP WITH TIME ZONE (UTC time + region id)
    /// into its local time, offset and region name — computed by the server:
    /// the region names come from `V$TIMEZONE_NAMES` (once per session), the
    /// local time from `AT TIME ZONE`, so daylight saving follows the
    /// server's own time-zone file.
    async fn resolve_region(&mut self, v: &mut Value) -> Result<(), OrmError> {
        let Value::TimestampTzRegionId(utc, id) = v.clone() else { return Ok(()) };
        self.resolving_tz = true;
        let r = self.resolve_region_inner(&utc, id).await;
        self.resolving_tz = false;
        *v = r?;
        Ok(())
    }

    async fn resolve_region_inner(&mut self, utc: &value::DateTime, id: u16) -> Result<Value, OrmError> {
        if self.tz_regions.is_none() {
            let ex = Box::pin(self.execute(
                    "SELECT tzname, FROM_TZ(TIMESTAMP '2000-01-01 00:00:00', tzname) FROM (SELECT DISTINCT tzname FROM v$timezone_names) ORDER BY tzname",
                    &[],
                ))
                .await?;
            let mut map = HashMap::new();
            for row in ex.rows {
                if let [Value::Str(name), Value::TimestampTzRegionId(_, rid)] = row.as_slice() {
                    map.entry(*rid).or_insert_with(|| name.clone());
                }
            }
            self.tz_regions = Some(map);
        }
        let name = self
            .tz_regions
            .as_ref()
            .and_then(|m| m.get(&id))
            .cloned()
            .ok_or_else(|| OrmError::TypeConversion(format!("unknown Oracle time-zone region id {id}")))?;
        let ex = Box::pin(self.execute(
            "SELECT TO_CHAR(FROM_TZ(:1, 'UTC') AT TIME ZONE (:2), 'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM') FROM dual",
            &[Bind::In(Value::Timestamp(*utc)), Bind::In(Value::Str(name.clone()))],
        ))
        .await?;
        let text = ex.rows.first().and_then(|r| r.first()).and_then(Value::to_text).unwrap_or_default();
        let (local, offset) = value::parse_datetime_offset(&text)
            .ok_or_else(|| OrmError::TypeConversion(format!("unexpected AT TIME ZONE result {text:?}")))?;
        Ok(Value::TimestampTzRegion(local, offset, name))
    }

    fn queue_close(&mut self, cursor_id: u16) {
        if cursor_id != 0 {
            self.pending_close.push(u32::from(cursor_id));
        }
    }

    async fn read_call(&mut self, columns: Option<&[Column]>, binds: &[Bind], prev_row: &[Value]) -> Result<Response, OrmError> {
        let (fv, eor) = (self.field_version, self.end_of_response());
        let mut r = self.read_response(|b| parse_response(b, fv, eor, columns, binds, prev_row)).await?;
        // Nested cursors: fetch each to the end into its placeholder.
        let mut nested = std::mem::take(&mut r.nested).into_iter();
        for row in &mut r.rows {
            for v in row.iter_mut() {
                if matches!(v, Value::Cursor(rs) if rs.columns.is_empty()) {
                    if let Some((id, cols)) = nested.next() {
                        *v = Value::Cursor(Box::pin(self.fetch_ref_cursor(id, cols)).await?);
                    }
                } else if let Value::Lob(lob) = v {
                    // BFILE columns: read the file through its locator.
                    let lob = lob.clone();
                    *v = Box::pin(self.lob_read_all(&lob)).await?;
                } else if let Value::ObjectImage { type_name, image } = v {
                    let (name, image) = (std::mem::take(type_name), std::mem::take(image));
                    *v = Box::pin(self.unpack_object(&name, &image)).await?;
                } else if matches!(v, Value::TimestampTzRegionId(..)) && !self.resolving_tz {
                    Box::pin(self.resolve_region(v)).await?;
                }
            }
        }
        for (id, _) in nested {
            self.queue_close(id);
        }
        Ok(r)
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
        write_execute(&mut w, 24, 32767, "SELECT 1 FROM DUAL", Kind::Query, &[&[]], false, &Default::default(), None).unwrap();
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
        write_execute(&mut w, 24, 32767, "BEGIN :R := F(:A); END;", Kind::PlSql, &[&binds], false, &Default::default(), None).unwrap();
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
