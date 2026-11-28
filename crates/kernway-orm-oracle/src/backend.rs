//! Oracle's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, returns generated keys, and keeps
//! statements outside a transaction committed (Oracle has no autocommit).

use crate::dialect::OracleDialect;
use crate::exec::{Bind, Column, CursorState, OutType};
use crate::value::{fmt_interval, fmt_offset, ora_type, DateTime, ResultSet, Value};
use crate::Conn;
use kernway_orm_core::{
    decode::{cells_to_entity, Cell},
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
    SqlDialect,
};
use kernway_orm_sql::{CallArg, CallResult, OutKind, Outcome, Param, Rows, ServerCursor, SqlConn};
use serde::de::DeserializeOwned;

static DIALECT: OracleDialect = OracleDialect;

fn bind(p: &Param) -> Bind {
    Bind::In(match p {
        Param::Null => Value::Null,
        Param::Int(i) => Value::Number(i.to_string()),
        Param::UInt(u) => Value::Number(u.to_string()),
        Param::Float(f) => Value::Float(*f),
        Param::Bool(b) => Value::Bool(*b),
        Param::Str(s) if s.is_empty() => Value::Null,
        Param::Str(s) => Value::Str(s.clone()),
        Param::Bytes(b) if b.is_empty() => Value::Null,
        Param::Bytes(b) => Value::Raw(b.clone()),
        // Typed later (from the procedure's declaration) and pickled.
        Param::Array(items) => Value::Collection { type_name: String::new(), elements: items.iter().map(param_value).collect() },
        Param::Object(fields) => Value::Object {
            type_name: String::new(),
            fields: fields.iter().map(|(k, v)| (k.clone(), param_value(v))).collect(),
        },
    })
}

fn param_value(p: &Param) -> Value {
    match bind(p) {
        Bind::In(v) => v,
        _ => Value::Null,
    }
}

pub(crate) fn binds(params: &[Param]) -> Vec<Bind> {
    params.iter().map(bind).collect()
}

/// A fetched value as a decoder cell for `col`. Oracle stores `''` as NULL,
/// so a NULL in a non-optional text column reads back as an empty string.
fn cell<'a>(v: &'a Value, col: Option<&ColumnDef>) -> Cell<'a> {
    match v {
        Value::Null => match col {
            Some(c) if !c.nullable && matches!(c.col_type, ColumnType::Text | ColumnType::Unknown) => Cell::Bytes(b""),
            Some(c) if !c.nullable && c.col_type == ColumnType::Binary => Cell::Bytes(b""),
            _ => Cell::Null,
        },
        Value::Str(s) | Value::Number(s) => Cell::Bytes(s.as_bytes()),
        Value::Float(f) => Cell::F64(*f),
        Value::Date(d) | Value::Timestamp(d) => Cell::Owned(d.to_string()),
        Value::TimestampTz(d, off) => Cell::Owned(format!("{d} {}", fmt_offset(*off))),
        Value::TimestampTzRegion(d, _, region) => Cell::Owned(format!("{d} {region}")),
        Value::TimestampTzRegionId(..) => Cell::Null,
        v @ (Value::IntervalYm { .. } | Value::IntervalDs { .. }) => Cell::Owned(fmt_interval(v, 2, 6)),
        Value::Raw(b) => Cell::Bytes(b),
        Value::Bool(b) => Cell::Bool(*b),
        Value::Cursor(_) | Value::Lob(_) | Value::ObjectImage { .. } | Value::ObjectBind { .. } => Cell::Null,
        // An object / collection mapped onto an entity field: its text form.
        v @ (Value::Object { .. } | Value::Collection { .. } | Value::AssocArray { .. }) => Cell::Owned(v.to_text().unwrap_or_default()),
    }
}

/// `YYYY-MM-DD HH:MM:SS` plus `digits` fractional digits (0–9).
fn fmt_datetime(d: &DateTime, digits: u8) -> String {
    let base = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second);
    match digits.min(9) {
        0 => base,
        n => format!("{base}.{}", &format!("{:09}", d.nanos)[..usize::from(n)]),
    }
}

/// A NUMBER's text as Int when it is integral and fits, else Decimal.
fn number_by_value(n: String) -> kernway_orm_sql::Value {
    use kernway_orm_sql::Value as V;
    if n.contains(['.', 'e', 'E']) {
        return V::Decimal(n);
    }
    n.parse::<i64>().map_or(V::Decimal(n), V::Int)
}

/// An Oracle value as a shared typed value, typed by its column where there
/// is one (OUT binds and cursor-less values are typed by the value):
/// NUMBER(p, 0) → Int (Decimal past i64), NUMBER(p, s) → Decimal, FLOAT →
/// Float, unconstrained NUMBER → Int when integral else Decimal (as
/// python-oracledb), DATE / TIMESTAMP(n) → DateTime with n fractional
/// digits, WITH TIME ZONE → DateTime with its offset, JSON → Json, INTERVAL
/// → Interval.
pub(crate) fn to_shared(v: Value, col: Option<&Column>) -> kernway_orm_sql::Value {
    use kernway_orm_sql::Value as V;
    let declared = col.map(|c| c.ora_type);
    let frac = col.map_or(9, |c| u8::try_from(c.scale).unwrap_or(9));
    match v {
        Value::Null => V::Null,
        Value::Str(s) => match declared {
            Some(ora_type::JSON) => V::Json(s),
            Some(ora_type::INTERVAL_YM | ora_type::INTERVAL_DS) => V::Interval(s),
            _ => V::Text(s),
        },
        Value::Number(n) => match col {
            Some(c) if c.ora_type == ora_type::NUMBER && c.precision > 0 && c.scale == 0 => {
                n.parse::<i64>().map_or(V::Decimal(n), V::Int)
            }
            Some(c) if c.ora_type == ora_type::NUMBER && c.precision > 0 && c.scale == -127 => {
                n.parse::<f64>().map_or(V::Decimal(n), V::Float)
            }
            Some(c) if c.ora_type == ora_type::NUMBER && c.scale > 0 => V::Decimal(n),
            _ => number_by_value(n),
        },
        Value::Float(f) => V::Float(f),
        Value::Date(d) => V::DateTime(fmt_datetime(&d, 0)),
        Value::Timestamp(d) => V::DateTime(fmt_datetime(&d, if col.is_some() { frac } else if d.nanos == 0 { 0 } else { 9 })),
        Value::TimestampTz(d, off) => V::DateTime(format!(
            "{} {}",
            fmt_datetime(&d, if col.is_some() { frac } else if d.nanos == 0 { 0 } else { 9 }),
            fmt_offset(off)
        )),
        Value::Raw(b) => V::Bytes(b),
        Value::Bool(b) => V::Bool(b),
        Value::Cursor(rs) => V::Rows(Box::new(result_set(rs))),
        Value::Lob(_) | Value::TimestampTzRegionId(..) | Value::ObjectImage { .. } | Value::ObjectBind { .. } => V::Null,
        Value::Object { fields, .. } => V::Object(fields.into_iter().map(|(n, v)| (n, to_shared(v, None))).collect()),
        Value::Collection { elements, .. } => V::Array(elements.into_iter().map(|v| to_shared(v, None)).collect()),
        Value::AssocArray { entries, .. } => V::Object(entries.into_iter().map(|(k, v)| (k.to_string(), to_shared(v, None))).collect()),
        Value::TimestampTzRegion(d, _, region) => V::DateTime(format!(
            "{} {region}",
            fmt_datetime(&d, if col.is_some() { frac } else if d.nanos == 0 { 0 } else { 9 })
        )),
        v @ (Value::IntervalYm { .. } | Value::IntervalDs { .. }) => {
            // Leading precision and fractional digits as declared (Oracle's
            // defaults: 2 and 6).
            let lead = col.map_or(2, |c| u8::try_from(c.precision).unwrap_or(2));
            let frac = col.map_or(6, |c| u8::try_from(c.scale).unwrap_or(6));
            V::Interval(fmt_interval(&v, lead, frac))
        }
    }
}

/// A fetched cursor as typed shared rows.
fn result_set(rs: ResultSet) -> Rows {
    let ResultSet { columns, rows } = rs;
    Rows {
        rows: rows.into_iter().map(|r| r.into_iter().zip(&columns).map(|(v, c)| to_shared(v, Some(c))).collect()).collect(),
        columns: columns.into_iter().map(|c| c.name).collect(),
    }
}

fn out_type(kind: OutKind) -> OutType {
    match kind {
        OutKind::Number => OutType::Number,
        OutKind::Float => OutType::Float,
        OutKind::Text => OutType::Str(8191),
        OutKind::Bytes => OutType::Raw(32767),
        OutKind::Bool => OutType::Bool,
        OutKind::Date => OutType::Date,
        OutKind::DateTime => OutType::Timestamp,
        OutKind::Cursor => OutType::Cursor,
        OutKind::DateTimeTz => OutType::TimestampTz,
        OutKind::IntervalYm => OutType::IntervalYm,
        OutKind::IntervalDs => OutType::IntervalDs,
        OutKind::LongText => OutType::Clob,
        OutKind::LongBytes => OutType::Blob,
        OutKind::Object => OutType::Object(String::new()),
    }
}

fn is_identifier(p: &str) -> bool {
    if let Some(inner) = p.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return !inner.is_empty() && !inner.contains('"');
    }
    let mut cs = p.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic()) && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'))
}

/// A call argument as its bind.
/// Whether a bind is an object value or OUT whose type is not known yet.
fn needs_declared_type(b: &Bind) -> bool {
    let untyped = |v: &Value| match v {
        Value::Object { type_name, .. } | Value::Collection { type_name, .. } | Value::AssocArray { type_name, .. } => type_name.is_empty(),
        _ => false,
    };
    match b {
        Bind::Out(OutType::Object(n)) | Bind::InOut(_, OutType::Object(n)) if n.is_empty() => true,
        Bind::In(v) | Bind::InOut(v, _) => untyped(v),
        _ => false,
    }
}

fn set_declared_type(b: &mut Bind, ty: &str) {
    let set = |v: &mut Value| {
        if let Value::Object { type_name, .. } | Value::Collection { type_name, .. } | Value::AssocArray { type_name, .. } = v {
            *type_name = ty.to_string();
        }
    };
    match b {
        Bind::Out(t) => *t = OutType::Object(ty.to_string()),
        Bind::InOut(v, t) => {
            set(v);
            *t = OutType::Object(ty.to_string());
        }
        Bind::In(v) => set(v),
    }
}

fn arg_bind(a: &CallArg) -> Bind {
    match a.inner() {
        CallArg::In(p) => bind(p),
        CallArg::Out(k) => Bind::Out(out_type(*k)),
        CallArg::InOut(p) => match bind(p) {
            Bind::In(v) => Bind::InOut(v, inout_type(p)),
            other => other,
        },
        CallArg::InOutAs(p, k) => match bind(p) {
            Bind::In(v) => Bind::InOut(v, out_type(*k)),
            other => other,
        },
        CallArg::Named(..) => unreachable!("inner() unwraps names"),
    }
}

/// `CREATE … PROCEDURE / FUNCTION / PACKAGE [BODY] / TRIGGER / TYPE [BODY]
/// [schema.]name`: (owner, name, USER_ERRORS type), as the dictionary
/// stores them.
fn plsql_unit(sql: &str) -> Option<(Option<String>, String, String)> {
    let text = strip_comments(sql);
    let words: Vec<&str> = text.split_whitespace().take(8).map(|w| w.split('(').next().unwrap_or(w)).collect();
    let up: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let mut k = 0;
    if up.first().map(String::as_str) != Some("CREATE") {
        return None;
    }
    k += 1;
    if up.get(k).map(String::as_str) == Some("OR") {
        k += 2;
    }
    if matches!(up.get(k).map(String::as_str), Some("EDITIONABLE" | "NONEDITIONABLE")) {
        k += 1;
    }
    let mut kind = up.get(k)?.clone();
    if !matches!(kind.as_str(), "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE") {
        return None;
    }
    k += 1;
    if matches!(kind.as_str(), "PACKAGE" | "TYPE") && up.get(k).map(String::as_str) == Some("BODY") {
        kind.push_str(" BODY");
        k += 1;
    }
    let full = *words.get(k)?;
    let norm = |p: &str| match p.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(q) => q.to_string(),
        None => p.to_ascii_uppercase(),
    };
    let (owner, name) = match full.split_once('.') {
        Some((o, n)) => (Some(norm(o)), norm(n)),
        None => (None, norm(full)),
    };
    Some((owner, name, kind))
}

/// The OUT type an `IN OUT` argument returns, from its input value.
fn inout_type(p: &Param) -> OutType {
    match p {
        Param::Int(_) | Param::UInt(_) => OutType::Number,
        Param::Float(_) => OutType::Float,
        Param::Bool(_) => OutType::Bool,
        // Past the PL/SQL bind limit an IN OUT value is a LOB both ways.
        Param::Bytes(b) if b.len() > 32767 => OutType::Blob,
        Param::Str(s) if s.len() > 32767 => OutType::Clob,
        Param::Bytes(_) => OutType::Raw(32767),
        Param::Str(_) | Param::Null => OutType::Str(8191),
        Param::Array(_) | Param::Object(_) => OutType::Object(String::new()),
    }
}

/// `name` as a procedure reference: one to three dot-separated parts
/// (schema.package.procedure), each an unquoted identifier or a quoted one.
fn procedure_name(name: &str) -> Result<String, OrmError> {
    let parts: Vec<&str> = name.split('.').collect();
    let ok = (1..=3).contains(&parts.len())
        && parts.iter().all(|p| {
            if let Some(inner) = p.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                !inner.is_empty() && !inner.contains('"')
            } else {
                let mut cs = p.chars();
                cs.next().is_some_and(|c| c.is_ascii_alphabetic())
                    && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'))
            }
        });
    if ok {
        Ok(name.to_string())
    } else {
        Err(OrmError::Query(format!("not a procedure name: {name:?}")))
    }
}

/// PL/SQL that drains `DBMS_OUTPUT` into three OUT binds: the lines read
/// (each ended by U+001F), one line that did not fit (if any), and whether
/// the buffer is now empty (then output is switched off again).
const DRAIN_OUTPUT: &str = "LOOP
    DBMS_OUTPUT.GET_LINE(kw_l, kw_s);
    IF kw_s = 1 THEN kw_done := 1; DBMS_OUTPUT.DISABLE; EXIT; END IF;
    IF NVL(LENGTHB(kw_buf), 0) + NVL(LENGTHB(kw_l), 0) + 1 > 32000 THEN
      kw_over := SUBSTRB(kw_l, 1, 32000); EXIT;
    END IF;
    kw_buf := kw_buf || kw_l || CHR(31);
  END LOOP;";
const DRAIN_DECLARE: &str =
    "DECLARE kw_l VARCHAR2(32767); kw_s INTEGER; kw_buf VARCHAR2(32767); kw_over VARCHAR2(32767); kw_done INTEGER := 0;";

/// Split a script into statements: `;` ends a SQL statement (dropped, as
/// Oracle refuses it); a PL/SQL unit (BEGIN / DECLARE / CREATE PROCEDURE,
/// FUNCTION, PACKAGE, TRIGGER, TYPE) runs to a line holding only `/`, as in
/// SQL*Plus. Quotes (incl. `q'[…]'`) and comments are respected.
pub(crate) fn split_script(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    let is_plsql_unit = |stmt: &str| {
        let words: Vec<String> = strip_comments(stmt).split_whitespace().take(5).map(str::to_ascii_uppercase).collect();
        let w = |k: usize| words.get(k).map_or("", String::as_str);
        match w(0) {
            "BEGIN" | "DECLARE" => true,
            "CREATE" => {
                let mut k = 1;
                if w(k) == "OR" && w(k + 1) == "REPLACE" {
                    k += 2;
                }
                if matches!(w(k), "EDITIONABLE" | "NONEDITIONABLE") {
                    k += 1;
                }
                matches!(w(k), "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY")
            }
            _ => false,
        }
    };
    let push = |stmt: &mut String, out: &mut Vec<String>| {
        let t = stmt.trim();
        if !strip_comments(t).trim().is_empty() {
            out.push(t.to_string());
        }
        stmt.clear();
    };
    let at_line_start = |cur: &str| cur.rsplit('\n').next().is_some_and(|l| l.trim().is_empty());
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // A line holding only "/" ends a PL/SQL unit (or is a stray run command).
        if c == '/' && at_line_start(&cur) {
            let rest_of_line: String = chars[i + 1..].iter().take_while(|c| **c != '\n').collect();
            if rest_of_line.trim().is_empty() {
                push(&mut cur, &mut out);
                i += 1 + rest_of_line.len();
                continue;
            }
        }
        match c {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                cur.push_str("/*");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    cur.push(chars[i]);
                    i += 1;
                }
                cur.push_str("*/");
                i += 2;
                continue;
            }
            'q' | 'Q' if next == Some('\'') && chars.get(i + 2).is_some() => {
                let open = chars[i + 2];
                let close = match open {
                    '[' => ']',
                    '{' => '}',
                    '<' => '>',
                    '(' => ')',
                    o => o,
                };
                cur.extend(&chars[i..i + 3]);
                i += 3;
                while i < chars.len() && !(chars[i] == close && chars.get(i + 1) == Some(&'\'')) {
                    cur.push(chars[i]);
                    i += 1;
                }
                cur.extend(chars[i..(i + 2).min(chars.len())].iter());
                i += 2;
                continue;
            }
            '\'' | '"' => {
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    cur.push(chars[i]);
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            cur.push(c);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            ';' if !is_plsql_unit(&cur) => {
                push(&mut cur, &mut out);
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    push(&mut cur, &mut out);
    out
}

fn strip_comments(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.lines();
    for line in it.by_ref() {
        let l = line.split("--").next().unwrap_or("");
        out.push_str(l);
        out.push('\n');
    }
    // Block comments are rare at a statement head; drop simple ones.
    while let (Some(a), Some(b)) = (out.find("/*"), out.find("*/")) {
        if b < a {
            break;
        }
        out.replace_range(a..b + 2, " ");
    }
    out
}

impl Conn {
    async fn run(&mut self, sql: &str, binds: &[Bind]) -> Result<crate::Executed, OrmError> {
        let r = if self.in_tx { self.execute(sql, binds).await? } else { self.execute_autocommit(sql, binds).await? };
        if let Some((24344, _)) = r.warning {
            return Err(self.compile_errors(sql).await);
        }
        Ok(r)
    }

    /// The error for a PL/SQL unit created with compilation errors, listing
    /// them from the data dictionary (line, column, message).
    async fn compile_errors(&mut self, sql: &str) -> OrmError {
        let Some((owner, name, kind)) = plsql_unit(sql) else {
            return OrmError::Query("ORA-24344: success with compilation error".into());
        };
        let listed = self
            .execute(
                "SELECT line, position, text FROM all_errors WHERE owner = NVL(:o, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) \
                 AND name = :n AND type = :t ORDER BY sequence",
                &[owner.map_or(Bind::In(Value::Null), Bind::from), Bind::from(name.as_str()), Bind::from(kind.as_str())],
            )
            .await;
        let mut msg = format!("ORA-24344: {kind} {name} was created with compilation errors");
        if let Ok(r) = listed {
            for row in r.rows {
                let t = |i: usize| row.get(i).and_then(Value::to_text).unwrap_or_default();
                msg.push_str(&format!("\n  line {}, column {}: {}", t(0), t(1), t(2).trim_end()));
            }
        }
        OrmError::Query(msg)
    }

    /// `BEGIN [:ret :=] name(args); END;` with `DBMS_OUTPUT` captured in the
    /// same round trip (more round trips only for output past ~32 KB).
    /// The declared object / collection / record types of procedure `name`'s
    /// arguments: (position — 0 for a function result, argument name, full
    /// type name or `None` for a scalar), resolved like PL/SQL resolves the
    /// name (synonyms, packages).
    async fn argument_types(&mut self, name: &str) -> Result<Vec<(u32, Option<String>, Option<String>)>, OrmError> {
        let r = self
            .execute(
                "DECLARE s VARCHAR2(128); p1 VARCHAR2(128); p2 VARCHAR2(128); l VARCHAR2(128); t NUMBER; o NUMBER;
                 BEGIN DBMS_UTILITY.NAME_RESOLVE(:n, 1, s, p1, p2, l, t, o); :s := s; :p1 := p1; :p2 := p2; END;",
                &[Bind::In(Value::Str(name.to_string())), Bind::Out(OutType::Str(128)), Bind::Out(OutType::Str(128)), Bind::Out(OutType::Str(128))],
            )
            .await?;
        let text = |v: &Value| v.to_text();
        let (owner, package, object) = (text(&r.out[1]), text(&r.out[2]), text(&r.out[3]));
        let rows = self
            .execute(
                "SELECT position, argument_name, type_owner, type_name, type_subname, data_type
                   FROM all_arguments
                  WHERE owner = :1 AND object_name = :2 AND NVL(package_name, '~') = NVL(:3, '~') AND data_level = 0
                    AND NVL(overload, '1') = (SELECT NVL(MIN(overload), '1') FROM all_arguments
                                               WHERE owner = :4 AND object_name = :5 AND NVL(package_name, '~') = NVL(:6, '~'))
                  ORDER BY sequence",
                // SQL binds by occurrence: each placeholder appears twice.
                &{
                    let three = [owner, object, package].map(|v| Bind::In(v.map_or(Value::Null, Value::Str)));
                    [three.clone(), three].concat()
                },
            )
            .await?;
        Ok(rows
            .rows
            .iter()
            .map(|row| {
                let pos = row[0].as_i64().unwrap_or(0) as u32;
                let arg = text(&row[1]);
                let ty = match (text(&row[2]), text(&row[3]), text(&row[4]), text(&row[5])) {
                    (_, _, _, Some(dt)) if dt == "XMLTYPE" || dt == "OPAQUE/XMLTYPE" => Some("SYS.XMLTYPE".to_string()),
                    (Some(o), Some(t), Some(sub), _) => Some(format!("{o}.{t}.{sub}")),
                    (Some(o), Some(t), None, _) => Some(format!("{o}.{t}")),
                    _ => None,
                };
                (pos, arg, ty)
            })
            .collect())
    }

    async fn invoke(&mut self, name: &str, returns: Option<OutKind>, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let name = procedure_name(name)?;
        // Placeholders are bound in order of appearance: the return value
        // (if any) first, then the arguments, then the output drain.
        let first = usize::from(returns.is_some()) + 1;
        let mut list = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let ph = format!(":{}", first + i);
            match a {
                CallArg::Named(param, _) if is_identifier(param) => list.push(format!("{param} => {ph}")),
                CallArg::Named(param, _) => return Err(OrmError::Query(format!("not a parameter name: {param:?}"))),
                _ => list.push(ph),
            }
        }
        let n = args.len() + usize::from(returns.is_some());
        let target = if returns.is_some() { format!(":1 := {name}") } else { name.clone() };
        let sql = format!(
            "{DRAIN_DECLARE} BEGIN DBMS_OUTPUT.ENABLE(NULL); {target}({}); {DRAIN_OUTPUT} :{} := kw_buf; :{} := kw_over; :{} := kw_done; END;",
            list.join(", "),
            n + 1,
            n + 2,
            n + 3
        );
        let mut b: Vec<Bind> = returns.map(|k| Bind::Out(out_type(k))).into_iter().collect();
        b.extend(args.iter().map(arg_bind));
        // Object / collection / record arguments take their types from the
        // procedure's declaration.
        if b.iter().any(needs_declared_type) {
            let declared = self.argument_types(&name).await?;
            let offset = usize::from(returns.is_some());
            for (i, bind) in b.iter_mut().enumerate() {
                if !needs_declared_type(bind) {
                    continue;
                }
                let found = if i < offset {
                    declared.iter().find(|d| d.0 == 0)
                } else {
                    match &args[i - offset] {
                        CallArg::Named(param, _) => declared.iter().find(|d| d.1.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(param))),
                        _ => declared.iter().find(|d| d.0 as usize == i - offset + 1),
                    }
                };
                let ty = found.and_then(|d| d.2.clone()).ok_or_else(|| {
                    OrmError::Query(format!("argument {} of {name} is not an object / collection / record type", i + 1 - offset))
                })?;
                set_declared_type(bind, &ty);
            }
        }
        b.extend([Bind::Out(OutType::Str(8191)), Bind::Out(OutType::Str(8191)), Bind::Out(OutType::Number)]);
        let r = match self.run(&sql, &b).await {
            Ok(r) => r,
            Err(e) => {
                // The block failed before draining: switch output off again
                // so the pooled session does not keep buffering.
                let _ = self.execute("BEGIN DBMS_OUTPUT.DISABLE; END;", &[]).await;
                return Err(e);
            }
        };
        let mut out = CallResult {
            results: r.implicit.into_iter().map(result_set).collect(),
            affected_rows: r.row_count,
            ..CallResult::default()
        };
        let mut values = r.out;
        let mut drain: Vec<Value> = values.split_off(n);
        if returns.is_some() {
            out.value = to_shared(values.remove(0), None);
        }
        out.out = args
            .iter()
            .zip(values)
            .filter(|(a, _)| !matches!(a.inner(), CallArg::In(_)))
            .map(|(_, v)| to_shared(v, None))
            .collect();
        loop {
            let text = |v: &Value| v.to_text().unwrap_or_default();
            out.output.extend(text(&drain[0]).split('\u{1f}').map(String::from));
            out.output.pop(); // after the last separator
            if drain[2].as_i64() == Some(1) {
                break;
            }
            out.output.push(text(&drain[1]));
            let more = format!("{DRAIN_DECLARE} BEGIN {DRAIN_OUTPUT} :1 := kw_buf; :2 := kw_over; :3 := kw_done; END;");
            let r = self
                .run(&more, &[Bind::Out(OutType::Str(8191)), Bind::Out(OutType::Str(8191)), Bind::Out(OutType::Number)])
                .await?;
            drain = r.out;
        }
        Ok(out)
    }

    /// The identity sequence of `table`.`column` (for a generated key that
    /// must be known before an INSERT whose values cannot go through PL/SQL).
    async fn identity_sequence(&mut self, table: &str, column: &str) -> Result<String, OrmError> {
        let r = self
            .execute(
                "SELECT sequence_name FROM user_tab_identity_cols WHERE table_name = :t AND column_name = :c",
                &[Bind::from(table), Bind::from(column)],
            )
            .await?;
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::to_text)
            .ok_or_else(|| OrmError::Query(format!("no identity column {table}.{column}")))
    }
}

/// Whether a bind is too long for PL/SQL (a long SQL bind instead).
fn is_long(p: &Param, max: u32) -> bool {
    match p {
        Param::Str(s) => s.len() as u64 * 4 > u64::from(max),
        Param::Bytes(b) => b.len() as u64 > u64::from(max),
        _ => false,
    }
}

impl SqlConn for Conn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        // Each row becomes an entity as its fetch round arrives.
        let cols = T::columns();
        let mut out = Vec::new();
        self.execute_each(sql, &binds(params), false, |_, row| {
            let cells: Vec<Cell<'_>> = row.iter().enumerate().map(|(i, v)| cell(v, cols.get(i))).collect();
            out.push(cells_to_entity::<T>(&cells, cols)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let r = self.execute(sql, &binds(params)).await?;
        Ok(r.rows.first().and_then(|row| row.first()).and_then(Value::as_i64).unwrap_or(0) as u64)
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        Ok(self.run(sql, &binds(params)).await?.row_count)
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], pk_col: &str) -> Result<Param, OrmError> {
        let quoted = DIALECT.quote_identifier(pk_col);
        if !params.iter().any(|p| is_long(p, self.max_string_size)) {
            // One round trip: the INSERT returns its key from a PL/SQL block.
            let sql = format!("BEGIN {insert_sql} RETURNING {quoted} INTO :kw_rid; END;");
            let mut b = binds(params);
            b.push(Bind::Out(OutType::Number));
            let r = self.run(&sql, &b).await?;
            let id = r.out.last().and_then(Value::as_i64).ok_or_else(|| OrmError::Query("no generated key returned".into()))?;
            return Ok(Param::Int(id));
        }
        // Long values cannot be PL/SQL binds: draw the key from the identity
        // sequence first, then insert it explicitly (the identity is
        // GENERATED BY DEFAULT, so an explicit key is accepted).
        let table = insert_sql
            .strip_prefix("INSERT INTO ")
            .and_then(|r| r.split_once(' '))
            .map(|(t, _)| t.trim_matches('"').to_string())
            .ok_or_else(|| OrmError::Query("unexpected INSERT shape".into()))?;
        let seq = self.identity_sequence(&table, pk_col).await?;
        let r = self.execute(&format!("SELECT \"{seq}\".NEXTVAL FROM dual"), &[]).await?;
        let id = r.rows.first().and_then(|row| row.first()).and_then(Value::as_i64).ok_or_else(|| OrmError::Query("no sequence value".into()))?;
        let (head, values) = insert_sql
            .split_once(") VALUES (")
            .ok_or_else(|| OrmError::Query("unexpected INSERT shape".into()))?;
        let sql = format!("{head}, {quoted}) VALUES ({values}", values = values.trim_end_matches(')').to_string() + &format!(", :{})", params.len() + 1));
        let mut b = binds(params);
        b.push(Bind::from(id));
        self.run(&sql, &b).await?;
        Ok(Param::Int(id))
    }

    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<kernway_orm_sql::Value>) -> Result<(), OrmError>,
    {
        let autocommit = !self.in_tx;
        let r = self
            .execute_each(sql, &binds(params), autocommit, |cols, row| {
                each(row.into_iter().zip(cols).map(|(v, c)| to_shared(v, Some(c))).collect())
            })
            .await?;
        let columns: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        let column_info = r.columns.iter().map(column_info).collect();
        let affected_rows = if columns.is_empty() { r.row_count } else { 0 };
        Ok(Outcome { columns, column_info, affected_rows, last_insert_id: None })
    }

    /// The query is executed; its first rows arrive with the execute and are
    /// buffered, the rest stay on the server until fetched (FETCH calls).
    /// Other statements may run on the connection in between.
    async fn cursor_open(&mut self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        let mut first = Vec::new();
        let (columns, open) = self.open_cursor(sql, &binds(params), &mut first).await?;
        let names = columns.iter().map(|c| c.name.clone()).collect();
        let id = self.next_cursor;
        self.next_cursor += 1;
        self.cursors.insert(id, CursorState { columns, buffered: first.into(), open });
        Ok(ServerCursor::new(id, names))
    }

    async fn cursor_fetch(&mut self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Vec<Vec<kernway_orm_sql::Value>>, OrmError> {
        if cursor.is_done() {
            return Ok(Vec::new());
        }
        let mut st = self
            .cursors
            .remove(&cursor.id())
            .ok_or_else(|| OrmError::Query("cursor is closed".into()))?;
        let want = max_rows.max(1);
        let mut failed = None;
        while st.buffered.len() < want {
            let Some(open) = st.open.as_mut() else { break };
            let mut more = Vec::new();
            let need = u32::try_from(want - st.buffered.len()).unwrap_or(u32::MAX);
            match self.fetch_open(open, need, &mut more).await {
                Ok(done) => {
                    st.buffered.extend(more);
                    if done {
                        st.open = None;
                    }
                }
                Err(e) => {
                    st.open = None;
                    failed = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failed {
            cursor.finish();
            return Err(e);
        }
        let n = want.min(st.buffered.len());
        let rows: Vec<Vec<kernway_orm_sql::Value>> = st
            .buffered
            .drain(..n)
            .map(|row| row.into_iter().zip(&st.columns).map(|(v, c)| to_shared(v, Some(c))).collect())
            .collect();
        if st.open.is_none() && st.buffered.is_empty() {
            cursor.finish();
        } else {
            self.cursors.insert(cursor.id(), st);
        }
        Ok(rows)
    }

    async fn cursor_close(&mut self, cursor: ServerCursor) -> Result<(), OrmError> {
        if let Some(st) = self.cursors.remove(&cursor.id()) {
            if let Some(open) = st.open {
                self.close_open(open);
            }
        }
        Ok(())
    }

    async fn execute_many(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        let rows: Vec<Vec<Bind>> = rows.iter().map(|r| binds(r)).collect();
        let autocommit = !self.in_tx;
        Conn::execute_many(self, sql, &rows, autocommit).await
    }

    async fn call(&mut self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        self.invoke(name, None, args).await
    }

    async fn call_function(&mut self, name: &str, returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        self.invoke(name, Some(returns), args).await
    }

    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        for (i, stmt) in split_script(script).iter().enumerate() {
            self.run(stmt, &[]).await.map_err(|e| match e {
                OrmError::Query(m) => OrmError::Query(format!("statement {} of the script: {m}", i + 1)),
                other => other,
            })?;
        }
        Ok(())
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        // Oracle starts a transaction implicitly; stop committing each call.
        self.in_tx = true;
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.in_tx = false;
        Conn::commit(self).await
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.in_tx = false;
        Conn::rollback(self).await
    }
}

/// A result column's type name, and whether it holds bytes (RAW, LONG RAW, BLOB, BFILE).
fn column_info(c: &crate::exec::Column) -> kernway_orm_sql::ColumnInfo {
    let (type_name, binary) = match c.ora_type {
        1 if c.csfrm == 2 => ("NVARCHAR2", false),
        1 => ("VARCHAR2", false),
        2 => ("NUMBER", false),
        8 => ("LONG", false),
        12 => ("DATE", false),
        23 => ("RAW", true),
        24 => ("LONG RAW", true),
        96 if c.csfrm == 2 => ("NCHAR", false),
        96 => ("CHAR", false),
        100 => ("BINARY_FLOAT", false),
        101 => ("BINARY_DOUBLE", false),
        104 => ("ROWID", false),
        112 if c.csfrm == 2 => ("NCLOB", false),
        112 => ("CLOB", false),
        113 => ("BLOB", true),
        114 => ("BFILE", true),
        119 => ("JSON", false),
        180 => ("TIMESTAMP", false),
        181 => ("TIMESTAMP WITH TIME ZONE", false),
        182 => ("INTERVAL YEAR TO MONTH", false),
        183 => ("INTERVAL DAY TO SECOND", false),
        231 => ("TIMESTAMP WITH LOCAL TIME ZONE", false),
        252 => ("BOOLEAN", false),
        _ => ("", false),
    };
    kernway_orm_sql::ColumnInfo { name: c.name.clone(), type_name: type_name.to_string(), binary }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_split_on_semicolons_and_slash_lines() {
        let script = "CREATE TABLE t (a VARCHAR2(10)); -- c;omment
INSERT INTO t VALUES ('x;y');
INSERT INTO t VALUES (q'[it's; ok]');
/* note; */ INSERT INTO t VALUES ('a''b');
CREATE OR REPLACE PROCEDURE p AS
BEGIN
  NULL; -- inside
END;
/
BEGIN p; END;
/
SELECT 1 FROM dual
";
        let parts = split_script(script);
        assert_eq!(parts.len(), 7, "{parts:#?}");
        assert_eq!(parts[0], "CREATE TABLE t (a VARCHAR2(10))");
        assert!(parts[1].ends_with("INSERT INTO t VALUES ('x;y')"));
        assert_eq!(parts[2], "INSERT INTO t VALUES (q'[it's; ok]')");
        assert!(parts[3].ends_with("INSERT INTO t VALUES ('a''b')"));
        assert!(parts[4].starts_with("CREATE OR REPLACE PROCEDURE p AS") && parts[4].ends_with("END;"));
        assert_eq!(parts[5], "BEGIN p; END;");
        assert_eq!(parts[6], "SELECT 1 FROM dual");
        assert!(split_script("  -- only a comment\n/\n").is_empty());
    }

    #[test]
    fn procedure_names_are_identifiers_only() {
        for ok in ["p", "pkg.p", "SCHEMA.PKG.PROC_1", "\"Mixed Case\".p"] {
            assert!(procedure_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "a.b.c.d", "p(1)", "p; DROP TABLE t", "1p", "\"a\"\"b\""] {
            assert!(procedure_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn values_are_typed_by_their_column() {
        use kernway_orm_sql::Value as V;
        let num = |p: i8, s: i8| Column { ora_type: ora_type::NUMBER, precision: p, scale: s, ..Column::default() };
        assert_eq!(to_shared(Value::Number("42".into()), Some(&num(10, 0))), V::Int(42));
        assert_eq!(to_shared(Value::Number("99999999999999999999".into()), Some(&num(38, 0))), V::Decimal("99999999999999999999".into()));
        assert_eq!(to_shared(Value::Number("1.50".into()), Some(&num(12, 2))), V::Decimal("1.50".into()));
        assert_eq!(to_shared(Value::Number("1.5".into()), Some(&num(126, -127))), V::Float(1.5));
        assert_eq!(to_shared(Value::Number("7".into()), Some(&num(0, -127))), V::Int(7));
        assert_eq!(to_shared(Value::Number("7.25".into()), None), V::Decimal("7.25".into()));
    }
}
