//! Hand-written SQL: typed result values, result sets, procedure calls, and
//! mapping a result set onto an entity by column name.

use crate::param::Param;
use kernway_orm_core::{
    decode::{cells_to_entity, Cell},
    entity::Entity,
    error::OrmError,
};
use serde::de::DeserializeOwned;

/// One value of a result row, typed by the column it came from.
///
/// Exact-precision and temporal values keep the database's own text form
/// (`Decimal("12.50")`, `DateTime("2026-11-26 09:30:00.250")`), so nothing is
/// rounded and no date library is imposed.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// A native boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer.
    UInt(u64),
    /// A binary floating-point number.
    Float(f64),
    /// An exact decimal number, as text.
    Decimal(String),
    /// Character data (also ENUM / SET values).
    Text(String),
    /// Binary data (binary strings, BLOBs, BIT, spatial values).
    Bytes(Vec<u8>),
    /// A JSON document, as text.
    Json(String),
    /// A date: `YYYY-MM-DD`.
    Date(String),
    /// A time of day or duration: `[-]HH:MM:SS[.fraction]` (hours may exceed 24).
    Time(String),
    /// A date and time: `YYYY-MM-DD HH:MM:SS[.fraction][ ±HH:MM]` (the
    /// offset only for a time-zone-aware type).
    DateTime(String),
    /// An interval (Oracle `INTERVAL YEAR TO MONTH` `+YY-MM` / `DAY TO
    /// SECOND` `+DD HH:MM:SS.fraction`).
    Interval(String),
    /// A result set returned as a value (a REF CURSOR).
    Rows(Box<Rows>),
}

impl Value {
    /// Whether the value is SQL NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The value as an `i64` (integers, booleans, and numeric text that fits).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::UInt(u) => i64::try_from(*u).ok(),
            Value::Bool(b) => Some(i64::from(*b)),
            Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// The value as an `f64` (any number, or numeric text).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// The value as a boolean (native booleans and 0 / non-zero integers).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Bytes(b) if !b.is_empty() => Some(b.iter().any(|x| *x != 0)),
            other => other.as_i64().map(|i| i != 0),
        }
    }

    /// The value as text: character data, decimals, JSON and temporal values.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s)
            | Value::Decimal(s)
            | Value::Json(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Interval(s) => Some(s),
            _ => None,
        }
    }

    /// The value as bytes: binary data, or the UTF-8 of a textual value.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            other => other.as_str().map(str::as_bytes),
        }
    }

    /// The value rendered as text, as a text-protocol result would show it
    /// (`None` for NULL and for a nested result set).
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null | Value::Rows(_) => None,
            Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            Value::Int(i) => Some(i.to_string()),
            Value::UInt(u) => Some(u.to_string()),
            Value::Float(f) => Some(f.to_string()),
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            other => other.as_str().map(String::from),
        }
    }

    /// The value as an entity-decoder cell.
    pub(crate) fn as_cell(&self) -> Cell<'_> {
        match self {
            Value::Null | Value::Rows(_) => Cell::Null,
            Value::Bool(b) => Cell::Bool(*b),
            Value::Int(i) => Cell::Int(*i),
            Value::UInt(u) => Cell::UInt(*u),
            Value::Float(f) => Cell::F64(*f),
            Value::Bytes(b) => Cell::Bytes(b),
            other => Cell::Bytes(other.as_str().unwrap_or_default().as_bytes()),
        }
    }
}

/// A result set: column names and rows of typed values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    /// Column names (labels), in select order.
    pub columns: Vec<String>,
    /// The rows, one value per column.
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    /// The position of column `name` (case-insensitive).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }

    /// Row `row`'s value in column `name`.
    pub fn get(&self, row: usize, name: &str) -> Option<&Value> {
        self.rows.get(row)?.get(self.index_of(name)?)
    }

    /// Map every row onto `T`, matching columns to `T`'s fields **by name**:
    /// a column `T` lacks is ignored; a field the result lacks reads as NULL.
    pub fn into_entities<T: Entity + DeserializeOwned>(&self) -> Result<Vec<T>, OrmError> {
        let cols = T::columns();
        let at: Vec<Option<usize>> = cols.iter().map(|c| self.index_of(c.name)).collect();
        let mut cells = Vec::with_capacity(cols.len());
        self.rows
            .iter()
            .map(|row| {
                cells.clear();
                cells.extend(at.iter().map(|i| i.and_then(|i| row.get(i)).map_or(Cell::Null, Value::as_cell)));
                cells_to_entity::<T>(&cells, cols)
            })
            .collect()
    }
}

/// What a statement did, besides any rows it returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Column names of the result set (empty for a statement without rows).
    pub columns: Vec<String>,
    /// Rows affected (INSERT / UPDATE / DELETE).
    pub affected_rows: u64,
    /// The key generated by an INSERT, when the database reports one.
    pub last_insert_id: Option<u64>,
}

/// The declared type of a procedure `OUT` argument. Databases that infer it
/// from the procedure (MySQL) ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutKind {
    /// Integer or exact number.
    Number,
    /// Binary floating point.
    Float,
    /// Character data.
    Text,
    /// Binary data.
    Bytes,
    /// Boolean.
    Bool,
    /// Date.
    Date,
    /// Timestamp.
    DateTime,
    /// A cursor (result set), read back as [`Value::Rows`].
    Cursor,
    /// Timestamp with a time zone offset.
    DateTimeTz,
    /// Interval in years and months.
    IntervalYm,
    /// Interval in days to seconds.
    IntervalDs,
}

/// One argument of a stored-procedure call.
#[derive(Debug, Clone, PartialEq)]
pub enum CallArg {
    /// An `IN` argument.
    In(Param),
    /// An `OUT` argument.
    Out(OutKind),
    /// An `IN OUT` argument with its initial value.
    InOut(Param),
    /// An argument passed by parameter name (`name => value`), in any order
    /// and skipping defaulted parameters. Oracle only: MySQL has no named
    /// arguments.
    Named(String, Box<CallArg>),
}

impl CallArg {
    /// `name => arg`: `CallArg::named("p_qty", 5)`,
    /// `CallArg::named("p_total", CallArg::Out(OutKind::Number))`.
    pub fn named(name: impl Into<String>, arg: impl Into<CallArg>) -> Self {
        CallArg::Named(name.into(), Box::new(arg.into()))
    }

    /// The argument without its name.
    pub fn inner(&self) -> &CallArg {
        match self {
            CallArg::Named(_, a) => a.inner(),
            a => a,
        }
    }
}

impl From<OutKind> for CallArg {
    fn from(k: OutKind) -> Self {
        CallArg::Out(k)
    }
}

impl<T: Into<Param>> From<T> for CallArg {
    fn from(v: T) -> Self {
        CallArg::In(v.into())
    }
}

/// Everything a procedure call returned.
#[derive(Debug, Clone, PartialEq)]
pub struct CallResult {
    /// The result sets the procedure produced, in order.
    pub results: Vec<Rows>,
    /// The `OUT` / `IN OUT` argument values, in argument order.
    pub out: Vec<Value>,
    /// Rows affected by the call's last statement.
    pub affected_rows: u64,
    /// Lines the procedure printed (Oracle `DBMS_OUTPUT`), in order.
    pub output: Vec<String>,
    /// What a function returned (`Null` for a procedure).
    pub value: Value,
}

impl Default for CallResult {
    fn default() -> Self {
        Self { results: Vec::new(), out: Vec::new(), affected_rows: 0, output: Vec::new(), value: Value::Null }
    }
}

impl From<i64> for Param {
    fn from(v: i64) -> Self {
        Param::Int(v)
    }
}
impl From<i32> for Param {
    fn from(v: i32) -> Self {
        Param::Int(v.into())
    }
}
impl From<u64> for Param {
    fn from(v: u64) -> Self {
        Param::UInt(v)
    }
}
impl From<u32> for Param {
    fn from(v: u32) -> Self {
        Param::UInt(v.into())
    }
}
impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Float(v)
    }
}
impl From<bool> for Param {
    fn from(v: bool) -> Self {
        Param::Bool(v)
    }
}
impl From<&str> for Param {
    fn from(v: &str) -> Self {
        Param::Str(v.to_string())
    }
}
impl From<String> for Param {
    fn from(v: String) -> Self {
        Param::Str(v)
    }
}
impl From<&String> for Param {
    fn from(v: &String) -> Self {
        Param::Str(v.clone())
    }
}
impl From<Vec<u8>> for Param {
    fn from(v: Vec<u8>) -> Self {
        Param::Bytes(v)
    }
}
impl From<&[u8]> for Param {
    fn from(v: &[u8]) -> Self {
        Param::Bytes(v.to_vec())
    }
}
impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(v: Option<T>) -> Self {
        v.map_or(Param::Null, Into::into)
    }
}

/// Build a `&[Param]` list from values: `params![42, "name", None::<i64>]`.
#[macro_export]
macro_rules! params {
    () => { &[] as &[$crate::Param] };
    ($($v:expr),+ $(,)?) => { &[$($crate::Param::from($v)),+] as &[$crate::Param] };
}
