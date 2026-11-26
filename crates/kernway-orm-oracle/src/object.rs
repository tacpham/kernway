//! Oracle object types, collections (VARRAY, nested table, PL/SQL
//! index-by table), PL/SQL records and XMLType: the type metadata (looked up
//! with `DBMS_PICKLER.GET_TYPE_SHAPE` and cached per session) and the
//! "pickled" image values travel in. Ported from python-oracledb's
//! `dbobject.pyx` / `dbobject_cache.pyx`; checked against the server.

use std::collections::HashMap;
use std::rc::Rc;

use kernway_orm_core::error::OrmError;

use crate::exec::{Bind, OutType};
use crate::lob::Lob;
use crate::value::{self, ora_type, Value};
use crate::Conn;

/// Collection kinds (from the type descriptor).
pub(crate) mod coll {
    pub const PLSQL_INDEX_TABLE: u8 = 1;
    #[allow(dead_code)]
    pub const NESTED_TABLE: u8 = 2;
    #[allow(dead_code)]
    pub const VARRAY: u8 = 3;
}

/// Pickle image constants.
const IS_DEGENERATE: u8 = 0x10;
const NO_PREFIX_SEG: u8 = 0x04;
const IS_COLLECTION: u8 = 0x08;
const IS_VERSION_81: u8 = 0x80;
const IMAGE_VERSION: u8 = 1;
const LONG_LENGTH: u8 = 0xFE;
const NULL_LENGTH: u8 = 0xFF;
const ATOMIC_NULL: u8 = 253;
const MAX_SHORT_LENGTH: usize = 245;
const HAS_INDEXES: u8 = 0x10;
const TOP_LEVEL: u16 = 0x01;
/// XMLType image flags.
const XML_LOB: u32 = 0x0001;
const XML_STRING: u32 = 0x0004;
const XML_SKIP_NEXT_4: u32 = 0x10_0000;

/// Type-descriptor (TDS) codes.
mod tds {
    pub const CHAR: u8 = 1;
    pub const DATE: u8 = 2;
    pub const FLOAT: u8 = 5;
    pub const NUMBER: u8 = 6;
    pub const VARCHAR: u8 = 7;
    pub const BOOLEAN: u8 = 8;
    pub const RAW: u8 = 19;
    pub const TIMESTAMP: u8 = 21;
    pub const TIMESTAMP_TZ: u8 = 23;
    pub const OBJ: u8 = 27;
    pub const COLL: u8 = 28;
    pub const CLOB: u8 = 29;
    pub const BLOB: u8 = 30;
    pub const TIMESTAMP_LTZ: u8 = 33;
    pub const BINARY_FLOAT: u8 = 37;
    pub const START_EMBED_ADT: u8 = 39;
    pub const SUBTYPE_MARKER: u8 = 43;
    pub const EMBED_ADT_INFO: u8 = 44;
    pub const BINARY_DOUBLE: u8 = 45;
}

/// The type of an attribute or collection element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttrType {
    /// A scalar: Oracle type number and character set form.
    Scalar { ora_type: u8, csfrm: u8 },
    /// An object / collection type, by full name (`SCHEMA.[PACKAGE.]NAME`).
    Object(String),
    /// `SYS.XMLTYPE`.
    Xml,
}

/// An object type, collection type or PL/SQL record type.
#[derive(Debug, Clone)]
pub(crate) struct ObjType {
    pub(crate) full_name: String,
    pub(crate) oid: Vec<u8>,
    pub(crate) version: u32,
    pub(crate) is_collection: bool,
    pub(crate) coll_kind: u8,
    pub(crate) max_elements: u32,
    pub(crate) element: Option<AttrType>,
    pub(crate) attrs: Vec<(String, AttrType)>,
}

/// Session cache of types, by full name.
#[derive(Default)]
pub(crate) struct TypeCache {
    by_name: HashMap<String, Rc<ObjType>>,
}

impl TypeCache {
    /// A type already loaded.
    pub(crate) fn get(&self, name: &str) -> Option<Rc<ObjType>> {
        self.by_name.get(name).cloned()
    }
}

/// A scalar type from its dictionary name (`VARCHAR2`, `NUMBER`, …).
fn scalar_from_name(name: &str) -> Result<AttrType, OrmError> {
    use ora_type::*;
    let base = name.split('(').next().unwrap_or(name).trim();
    // PL/SQL-only types are named like "PL/SQL PLS INTEGER", "PL/SQL BOOLEAN".
    let plsql = base.strip_prefix("PL/SQL ").map(|b| b.replace(' ', "_"));
    let base = plsql.as_deref().unwrap_or(base);
    let (t, csfrm) = match base {
        "VARCHAR2" | "VARCHAR" | "STRING" => (VARCHAR, 1),
        "NVARCHAR2" => (VARCHAR, 2),
        "CHAR" | "CHARACTER" => (CHAR, 1),
        "NCHAR" => (CHAR, 2),
        "NUMBER" | "INTEGER" | "INT" | "SMALLINT" | "FLOAT" | "REAL" | "DOUBLE PRECISION" | "DECIMAL" | "NUMERIC" => (NUMBER, 0),
        "PLS_INTEGER" | "BINARY_INTEGER" | "NATURAL" | "POSITIVE" | "SIMPLE_INTEGER" => (BINARY_INTEGER, 0),
        "BINARY_FLOAT" => (BINARY_FLOAT, 0),
        "BINARY_DOUBLE" => (BINARY_DOUBLE, 0),
        "DATE" => (DATE, 0),
        "RAW" => (RAW, 0),
        "CLOB" => (CLOB, 1),
        "NCLOB" => (CLOB, 2),
        "BLOB" => (BLOB, 0),
        "BOOLEAN" | "PL/SQL BOOLEAN" => (BOOLEAN, 0),
        b if b.starts_with("TIMESTAMP") && b.ends_with("WITH LOCAL TIME ZONE") => (TIMESTAMP_LTZ, 0),
        b if b.starts_with("TIMESTAMP") && b.ends_with("WITH TIME ZONE") => (TIMESTAMP_TZ, 0),
        b if b.starts_with("TIMESTAMP") => (TIMESTAMP, 0),
        b if b.starts_with("INTERVAL DAY") => (INTERVAL_DS, 0),
        b if b.starts_with("INTERVAL YEAR") => (INTERVAL_YM, 0),
        other => return Err(OrmError::Unsupported(format!("attribute type {other} inside an object"))),
    };
    Ok(AttrType::Scalar { ora_type: t, csfrm })
}

/// The element type of a collection from its type descriptor (TDS), and
/// the collection kind / limit; `None` for an object type.
fn parse_tds(tds: &[u8]) -> Result<Option<(u8, u32, AttrType)>, OrmError> {
    let bad = || OrmError::TypeConversion("malformed type descriptor".into());
    let mut p = 4 + 2 + 2; // end offset, version, unknown
    let num_attrs = u16::from_be_bytes([*tds.get(p).ok_or_else(bad)?, *tds.get(p + 1).ok_or_else(bad)?]);
    p += 2 + 1 + 1 + 2 + 4; // count, attrs?, ADT op, ADT number, index offset
    if num_attrs != 1 || tds.get(p) != Some(&tds::COLL) {
        return Ok(None);
    }
    p += 1;
    let be32 = |i: usize| -> Result<u32, OrmError> { Ok(u32::from_be_bytes(tds.get(i..i + 4).ok_or_else(bad)?.try_into().unwrap())) };
    let elem_pos = be32(p)? as usize;
    let max = be32(p + 4)?;
    let kind = *tds.get(p + 8).ok_or_else(bad)?;
    let mut q = elem_pos;
    let mut code;
    loop {
        code = *tds.get(q).ok_or_else(bad)?;
        q += 1;
        if code == tds::EMBED_ADT_INFO {
            q += 1;
        } else if code != tds::SUBTYPE_MARKER {
            break;
        }
    }
    use ora_type::*;
    let elem = match code {
        tds::NUMBER | tds::FLOAT => AttrType::Scalar { ora_type: NUMBER, csfrm: 0 },
        tds::VARCHAR | tds::CHAR => {
            let csfrm = tds.get(q + 2).ok_or_else(bad)? & 0x7F;
            AttrType::Scalar { ora_type: if code == tds::VARCHAR { VARCHAR } else { CHAR }, csfrm }
        }
        tds::RAW => AttrType::Scalar { ora_type: RAW, csfrm: 0 },
        tds::BINARY_FLOAT => AttrType::Scalar { ora_type: BINARY_FLOAT, csfrm: 0 },
        tds::BINARY_DOUBLE => AttrType::Scalar { ora_type: BINARY_DOUBLE, csfrm: 0 },
        tds::DATE => AttrType::Scalar { ora_type: DATE, csfrm: 0 },
        tds::TIMESTAMP => AttrType::Scalar { ora_type: TIMESTAMP, csfrm: 0 },
        tds::TIMESTAMP_TZ => AttrType::Scalar { ora_type: TIMESTAMP_TZ, csfrm: 0 },
        tds::TIMESTAMP_LTZ => AttrType::Scalar { ora_type: TIMESTAMP_LTZ, csfrm: 0 },
        tds::BOOLEAN => AttrType::Scalar { ora_type: BOOLEAN, csfrm: 0 },
        tds::CLOB => AttrType::Scalar { ora_type: CLOB, csfrm: 1 },
        tds::BLOB => AttrType::Scalar { ora_type: BLOB, csfrm: 0 },
        // Its name comes from the dictionary (see `load_type`).
        tds::OBJ | tds::START_EMBED_ADT => AttrType::Object(String::new()),
        other => return Err(OrmError::Unsupported(format!("collection element type code {other}"))),
    };
    Ok(Some((kind, max, elem)))
}

/// `"SCHEMA.NAME"` / `"SCHEMA.PACKAGE.NAME"`.
fn full_name(schema: &str, package: Option<&str>, name: &str) -> String {
    match package {
        Some(p) => format!("{schema}.{p}.{name}"),
        None => format!("{schema}.{name}"),
    }
}

const GET_TYPE_SHAPE: &str = "declare
    t_inst varchar2(3); t_sowner varchar2(128); t_sname varchar2(128); t_sub sys_refcursor;
begin
    :ret := dbms_pickler.get_type_shape(:full_name, :oid, :version, :tds, t_inst, t_sowner, t_sname, :attrs, t_sub);
    :pkg := null;
    begin
        select owner, type_name into :sch, :nm from all_types where type_oid = :oid;
    exception when no_data_found then
        begin
            select owner, package_name, type_name into :sch, :pkg, :nm from all_plsql_types where type_oid = :oid;
        exception when no_data_found then null;
        end;
    end;
end;";

impl Conn {
    /// The type `name` (`[SCHEMA.][PACKAGE.]NAME`, as the dictionary
    /// spells it — upper case unless created quoted), from the session cache
    /// or the server.
    pub(crate) async fn object_type(&mut self, name: &str) -> Result<Rc<ObjType>, OrmError> {
        if let Some(t) = self.types.by_name.get(name) {
            return Ok(Rc::clone(t));
        }
        let ex = Box::pin(self.execute(
            GET_TYPE_SHAPE,
            &[
                Bind::Out(OutType::Number),
                Bind::In(Value::Str(name.to_string())),
                Bind::Out(OutType::Raw(64)),
                Bind::Out(OutType::Number),
                Bind::Out(OutType::Raw(32767)),
                Bind::Out(OutType::Cursor),
                Bind::Out(OutType::Str(128)),
                Bind::Out(OutType::Str(128)),
                Bind::Out(OutType::Str(128)),
            ],
        ))
        .await?;
        let out = &ex.out;
        if out[0].as_i64() != Some(0) {
            return Err(OrmError::Query(format!("no object type named {name} (or no privilege to see it)")));
        }
        let text = |v: &Value| v.to_text();
        let (schema, package, tname) = (text(&out[7]), text(&out[6]), text(&out[8]));
        let Some((schema, tname)) = schema.zip(tname) else {
            return Err(OrmError::Query(format!("type {name} has no dictionary entry")));
        };
        let canonical = full_name(&schema, package.as_deref(), &tname);
        let oid = match &out[2] {
            Value::Raw(b) => b.clone(),
            _ => Vec::new(),
        };
        let version = out[3].as_i64().unwrap_or(1) as u32;
        let tds = match &out[4] {
            Value::Raw(b) => b.clone(),
            _ => Vec::new(),
        };
        let mut t = ObjType {
            full_name: canonical.clone(),
            oid,
            version,
            is_collection: false,
            coll_kind: 0,
            max_elements: 0,
            element: None,
            attrs: Vec::new(),
        };
        // Attributes, by position (the columns are named by expressions):
        // version, name, number, type name, type owner, type package, …
        if let Value::Cursor(rs) = &out[5] {
            let (an, tn, to, tp) = (Some(1), Some(3), Some(4), Some(5));
            for row in &rs.rows {
                let get = |i: Option<usize>| i.and_then(|i| row.get(i)).and_then(Value::to_text);
                let Some(attr) = get(an) else { continue };
                let type_name = get(tn).unwrap_or_default();
                let ty = match get(to) {
                    Some(owner) if owner == "SYS" && type_name == "XMLTYPE" => AttrType::Xml,
                    Some(owner) => AttrType::Object(full_name(&owner, get(tp).as_deref(), &type_name)),
                    None => scalar_from_name(&type_name)?,
                };
                t.attrs.push((attr, ty));
            }
        }
        if let Some((kind, max, elem)) = parse_tds(&tds)? {
            t.is_collection = true;
            t.coll_kind = kind;
            t.max_elements = max;
            t.element = Some(match elem {
                AttrType::Object(_) => self.element_object_type(&schema, package.as_deref(), &tname).await?,
                AttrType::Scalar { ora_type: ora_type::CLOB, .. } => {
                    if self.element_type_name(&schema, package.as_deref(), &tname).await?.as_deref() == Some("NCLOB") {
                        AttrType::Scalar { ora_type: ora_type::CLOB, csfrm: 2 }
                    } else {
                        elem
                    }
                }
                other => other,
            });
        }
        let t = Rc::new(t);
        self.types.by_name.insert(name.to_string(), Rc::clone(&t));
        self.types.by_name.insert(canonical, Rc::clone(&t));
        Ok(t)
    }

    async fn element_type_name(&mut self, schema: &str, package: Option<&str>, name: &str) -> Result<Option<String>, OrmError> {
        let (sql, binds) = match package {
            Some(p) => (
                "select elem_type_name from all_plsql_coll_types where owner = :1 and package_name = :2 and type_name = :3",
                vec![Bind::from(schema), Bind::from(p), Bind::from(name)],
            ),
            None => ("select elem_type_name from all_coll_types where owner = :1 and type_name = :2", vec![Bind::from(schema), Bind::from(name)]),
        };
        let ex = Box::pin(self.execute(sql, &binds)).await?;
        Ok(ex.rows.first().and_then(|r| r.first()).and_then(Value::to_text))
    }

    async fn element_object_type(&mut self, schema: &str, package: Option<&str>, name: &str) -> Result<AttrType, OrmError> {
        let (sql, binds) = match package {
            Some(p) => (
                "select elem_type_owner, elem_type_package, elem_type_name from all_plsql_coll_types where owner = :1 and package_name = :2 and type_name = :3",
                vec![Bind::from(schema), Bind::from(p), Bind::from(name)],
            ),
            None => (
                "select elem_type_owner, null, elem_type_name from all_coll_types where owner = :1 and type_name = :2",
                vec![Bind::from(schema), Bind::from(name)],
            ),
        };
        let ex = Box::pin(self.execute(sql, &binds)).await?;
        let row = ex.rows.first().ok_or_else(|| OrmError::Query(format!("no element type for {schema}.{name}")))?;
        let owner = row[0].to_text().unwrap_or_default();
        let ename = row[2].to_text().unwrap_or_default();
        if owner == "SYS" && ename == "XMLTYPE" {
            return Ok(AttrType::Xml);
        }
        Ok(AttrType::Object(full_name(&owner, row[1].to_text().as_deref(), &ename)))
    }

    /// Decode an object / collection image of type `name` into a value
    /// (nested types looked up as needed, LOB attributes read).
    pub(crate) async fn unpack_object(&mut self, name: &str, image: &[u8]) -> Result<Value, OrmError> {
        if name == "SYS.XMLTYPE" {
            return self.unpack_xml(image).await;
        }
        let t = self.object_type(name).await?;
        let mut r = Pickle { b: image, p: 0 };
        r.header()?;
        let mut v = Box::pin(self.unpack_body(&t, &mut r)).await?;
        Box::pin(self.read_object_lobs(&mut v)).await?;
        Ok(v)
    }

    async fn unpack_body(&mut self, t: &ObjType, r: &mut Pickle<'_>) -> Result<Value, OrmError> {
        if t.is_collection {
            let elem = t.element.clone().ok_or_else(|| OrmError::TypeConversion("collection without element type".into()))?;
            r.u8()?; // collection flags
            let n = r.length()?;
            let indexed = t.coll_kind == coll::PLSQL_INDEX_TABLE;
            let (mut elements, mut entries) = (Vec::new(), Vec::new());
            for _ in 0..n {
                let key = if indexed { Some(r.i32be()?) } else { None };
                let v = Box::pin(self.unpack_value(&elem, true, r)).await?;
                match key {
                    Some(k) => entries.push((k, v)),
                    None => elements.push(v),
                }
            }
            return Ok(if indexed {
                Value::AssocArray { type_name: t.full_name.clone(), entries }
            } else {
                Value::Collection { type_name: t.full_name.clone(), elements }
            });
        }
        let mut fields = Vec::with_capacity(t.attrs.len());
        for (name, ty) in &t.attrs {
            fields.push((name.clone(), Box::pin(self.unpack_value(ty, false, r)).await?));
        }
        Ok(Value::Object { type_name: t.full_name.clone(), fields })
    }

    async fn unpack_value(&mut self, ty: &AttrType, in_collection: bool, r: &mut Pickle<'_>) -> Result<Value, OrmError> {
        match ty {
            AttrType::Scalar { ora_type: t @ (ora_type::CLOB | ora_type::BLOB), .. } => Ok(match r.bytes()? {
                Some(locator) => Value::Lob(Lob { locator, blob: *t == ora_type::BLOB, bfile: false }),
                None => Value::Null,
            }),
            AttrType::Scalar { ora_type: t, csfrm } => Ok(match r.bytes()? {
                None => Value::Null,
                // A 4-byte integer 0 / 1 — in the server's byte order (seen
                // little-endian, where python-oracledb assumes big-endian).
                Some(b) if *t == ora_type::BOOLEAN => Value::Bool(b.iter().any(|x| *x != 0)),
                Some(b) if *t == ora_type::BINARY_INTEGER => {
                    let v = b.iter().fold(0i64, |a, x| (a << 8) | i64::from(*x));
                    let v = if b.len() == 4 { i64::from(v as u32 as i32) } else { v };
                    Value::Number(v.to_string())
                }
                Some(b) => value::decode(*t, *csfrm, b).map_err(OrmError::TypeConversion)?,
            }),
            AttrType::Xml => match r.bytes()? {
                Some(img) => Box::pin(self.unpack_xml(&img)).await,
                None => Ok(Value::Null),
            },
            AttrType::Object(name) => {
                let t = self.object_type(name).await?;
                let collection = t.is_collection || in_collection;
                if r.atomic_null(collection)? {
                    return Ok(Value::Null);
                }
                if collection {
                    let Some(img) = r.bytes()? else { return Ok(Value::Null) };
                    let mut inner = Pickle { b: &img, p: 0 };
                    inner.header()?;
                    Box::pin(self.unpack_body(&t, &mut inner)).await
                } else {
                    Box::pin(self.unpack_body(&t, r)).await
                }
            }
        }
    }

    /// XMLType image: text inline, or a CLOB locator.
    async fn unpack_xml(&mut self, image: &[u8]) -> Result<Value, OrmError> {
        let mut r = Pickle { b: image, p: 0 };
        r.header()?;
        r.u8()?; // XML version
        let flag = r.u32be()?;
        if flag & XML_SKIP_NEXT_4 != 0 {
            r.take(4)?;
        }
        let rest = r.b[r.p..].to_vec();
        if flag & XML_STRING != 0 {
            return Ok(Value::Str(String::from_utf8_lossy(&rest).into_owned()));
        }
        if flag & XML_LOB != 0 {
            let lob = Lob { locator: rest, blob: false, bfile: false };
            let v = self.lob_read_all(&lob).await?;
            if lob.is_temporary() {
                let _ = self.lob_free_temp(&lob).await;
            }
            return Ok(v);
        }
        Err(OrmError::TypeConversion(format!("unexpected XMLType image flag {flag:#x}")))
    }

    /// Read the content of LOB attributes / elements in place.
    async fn read_object_lobs(&mut self, v: &mut Value) -> Result<(), OrmError> {
        match v {
            Value::Lob(lob) => {
                let lob = lob.clone();
                *v = self.lob_read_all(&lob).await?;
                if lob.is_temporary() {
                    let _ = self.lob_free_temp(&lob).await;
                }
            }
            Value::Object { fields, .. } => {
                for (_, f) in fields {
                    Box::pin(self.read_object_lobs(f)).await?;
                }
            }
            Value::Collection { elements, .. } => {
                for e in elements {
                    Box::pin(self.read_object_lobs(e)).await?;
                }
            }
            Value::AssocArray { entries, .. } => {
                for (_, e) in entries {
                    Box::pin(self.read_object_lobs(e)).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// A cursor over a pickled image.
struct Pickle<'a> {
    b: &'a [u8],
    p: usize,
}

fn short() -> OrmError {
    OrmError::TypeConversion("truncated object image".into())
}

impl Pickle<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], OrmError> {
        let s = self.b.get(self.p..self.p + n).ok_or_else(short)?;
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, OrmError> {
        Ok(self.take(1)?[0])
    }
    fn u32be(&mut self) -> Result<u32, OrmError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32be(&mut self) -> Result<i32, OrmError> {
        Ok(self.u32be()? as i32)
    }
    /// A length: one byte, or `0xFE` and four.
    fn length(&mut self) -> Result<usize, OrmError> {
        match self.u8()? {
            LONG_LENGTH => Ok(self.u32be()? as usize),
            n => Ok(n as usize),
        }
    }
    /// Length-prefixed bytes; `None` for NULL (length 0 or 0xFF).
    fn bytes(&mut self) -> Result<Option<Vec<u8>>, OrmError> {
        let n = match self.u8()? {
            0 | NULL_LENGTH => return Ok(None),
            LONG_LENGTH => self.u32be()? as usize,
            n => n as usize,
        };
        Ok(Some(self.take(n)?.to_vec()))
    }
    /// Whether the next value is an atomically NULL object.
    fn atomic_null(&mut self, collection: bool) -> Result<bool, OrmError> {
        let b = *self.b.get(self.p).ok_or_else(short)?;
        if b == ATOMIC_NULL || (collection && b == NULL_LENGTH) {
            self.p += 1;
            return Ok(true);
        }
        Ok(false)
    }
    fn header(&mut self) -> Result<(), OrmError> {
        let flags = self.u8()?;
        self.u8()?; // version
        self.length()?; // total size
        if flags & IS_DEGENERATE != 0 {
            return Err(OrmError::Unsupported("an object image stored in a LOB".into()));
        }
        if flags & NO_PREFIX_SEG == 0 {
            let n = self.length()?;
            self.take(n)?;
        }
        Ok(())
    }
}

/// Write side of the pickle format (IN binds).
pub(crate) struct PickleWriter {
    pub(crate) buf: Vec<u8>,
}

impl PickleWriter {
    /// Start an image: flags, version, a size patched in by [`finish`](Self::finish).
    pub(crate) fn new(collection: bool) -> Self {
        let mut buf = vec![IS_VERSION_81 | if collection { IS_COLLECTION } else { 0 }, IMAGE_VERSION, LONG_LENGTH, 0, 0, 0, 0];
        if collection {
            buf.extend([1, 1]); // prefix segment
        } else {
            buf[0] |= NO_PREFIX_SEG;
        }
        Self { buf }
    }
    pub(crate) fn finish(mut self) -> Vec<u8> {
        let n = self.buf.len() as u32;
        self.buf[3..7].copy_from_slice(&n.to_be_bytes());
        self.buf
    }
    pub(crate) fn length(&mut self, n: usize) {
        if n <= MAX_SHORT_LENGTH {
            self.buf.push(n as u8);
        } else {
            self.buf.push(LONG_LENGTH);
            self.buf.extend((n as u32).to_be_bytes());
        }
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.length(b.len());
        self.buf.extend_from_slice(b);
    }
    pub(crate) fn null(&mut self) {
        self.buf.push(NULL_LENGTH);
    }
    pub(crate) fn atomic_null(&mut self) {
        self.buf.push(ATOMIC_NULL);
    }
    pub(crate) fn collection_header(&mut self, kind: u8, n: usize) {
        self.buf.push(if kind == coll::PLSQL_INDEX_TABLE { HAS_INDEXES } else { 0 });
        self.length(n);
    }
}

impl Conn {
    /// Pickle `v` as a value of type `name` for an IN / IN OUT bind: the
    /// full bind image (type OID, lengths, flags, pickled data). Collections
    /// take a [`Value::Collection`] (or, for an index-by table, a
    /// [`Value::AssocArray`] / an object keyed by index); objects and records
    /// a [`Value::Object`] (fields by name, case-insensitive; missing ones
    /// NULL).
    pub(crate) async fn pack_bind(&mut self, name: &str, v: &Value) -> Result<Vec<u8>, OrmError> {
        let t = self.object_type(name).await?;
        let mut w = PickleWriter::new(t.is_collection);
        Box::pin(self.pack_body(&t, v, &mut w)).await?;
        Ok(object_bind_bytes(&t, &w.finish()))
    }

    async fn pack_body(&mut self, t: &ObjType, v: &Value, w: &mut PickleWriter) -> Result<(), OrmError> {
        let mismatch = || OrmError::TypeConversion(format!("a value of type {} expected, got {}", t.full_name, v.to_text().unwrap_or_default()));
        if t.is_collection {
            let elem = t.element.clone().ok_or_else(mismatch)?;
            if t.coll_kind == coll::PLSQL_INDEX_TABLE {
                let entries: Vec<(i32, Value)> = match v {
                    Value::AssocArray { entries, .. } => entries.clone(),
                    Value::Collection { elements, .. } => elements.iter().cloned().enumerate().map(|(i, e)| (i as i32 + 1, e)).collect(),
                    Value::Object { fields, .. } => fields
                        .iter()
                        .map(|(k, e)| k.parse::<i32>().map(|k| (k, e.clone())).map_err(|_| mismatch()))
                        .collect::<Result<_, _>>()?,
                    _ => return Err(mismatch()),
                };
                let mut entries = entries;
                entries.sort_by_key(|(k, _)| *k);
                w.collection_header(t.coll_kind, entries.len());
                for (k, e) in &entries {
                    w.buf.extend(k.to_be_bytes());
                    Box::pin(self.pack_value(&elem, true, e, w)).await?;
                }
                return Ok(());
            }
            let Value::Collection { elements, .. } = v else { return Err(mismatch()) };
            if t.max_elements > 0 && elements.len() > t.max_elements as usize {
                return Err(OrmError::TypeConversion(format!("{} holds at most {} elements, got {}", t.full_name, t.max_elements, elements.len())));
            }
            w.collection_header(t.coll_kind, elements.len());
            for e in elements {
                Box::pin(self.pack_value(&elem, true, e, w)).await?;
            }
            return Ok(());
        }
        let Value::Object { fields, .. } = v else { return Err(mismatch()) };
        for f in fields {
            if !t.attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(&f.0)) {
                return Err(OrmError::TypeConversion(format!("{} has no attribute {}", t.full_name, f.0)));
            }
        }
        for (name, ty) in &t.attrs {
            let fv = fields.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map_or(&Value::Null, |(_, v)| v);
            Box::pin(self.pack_value(ty, false, fv, w)).await?;
        }
        Ok(())
    }

    async fn pack_value(&mut self, ty: &AttrType, in_collection: bool, v: &Value, w: &mut PickleWriter) -> Result<(), OrmError> {
        if matches!(v, Value::Null) {
            match ty {
                AttrType::Object(name) => {
                    let t = self.object_type(name).await?;
                    if t.is_collection || in_collection {
                        w.null();
                    } else {
                        w.atomic_null();
                    }
                }
                _ => w.null(),
            }
            return Ok(());
        }
        let conv = |e: String| OrmError::TypeConversion(e);
        match ty {
            AttrType::Xml => return Err(OrmError::Unsupported("XMLType inside an IN object".into())),
            AttrType::Object(name) => {
                let t = self.object_type(name).await?;
                if t.is_collection || in_collection {
                    let mut inner = PickleWriter::new(t.is_collection);
                    Box::pin(self.pack_body(&t, v, &mut inner)).await?;
                    w.bytes(&inner.finish());
                } else {
                    Box::pin(self.pack_body(&t, v, w)).await?;
                }
            }
            AttrType::Scalar { ora_type: t, csfrm } => {
                use ora_type::*;
                let text = || v.to_text().unwrap_or_default();
                match *t {
                    VARCHAR | CHAR if *csfrm == 2 => {
                        let b: Vec<u8> = text().encode_utf16().flat_map(u16::to_be_bytes).collect();
                        w.bytes(&b);
                    }
                    VARCHAR | CHAR => w.bytes(text().as_bytes()),
                    NUMBER => w.bytes(&value::encode_number(&text()).map_err(conv)?),
                    BINARY_INTEGER => {
                        let i: i32 = text().parse().map_err(|_| conv(format!("not an integer: {}", text())))?;
                        w.bytes(&i.to_be_bytes());
                    }
                    BOOLEAN => {
                        let b = match v {
                            Value::Bool(b) => *b,
                            other => other.as_i64().is_some_and(|i| i != 0),
                        };
                        w.bytes(&u32::from(b).to_be_bytes());
                    }
                    BINARY_DOUBLE | BINARY_FLOAT => {
                        let f = match v {
                            Value::Float(f) => *f,
                            other => text().parse().map_err(|_| conv(format!("not a number: {other:?}")))?,
                        };
                        w.bytes(&value::encode_double(f));
                    }
                    RAW => match v {
                        Value::Raw(b) => w.bytes(b),
                        _ => return Err(conv("RAW attribute takes bytes".into())),
                    },
                    DATE | TIMESTAMP | TIMESTAMP_LTZ | TIMESTAMP_TZ => {
                        let b = match v {
                            Value::Date(d) => value::encode_date(d),
                            Value::Timestamp(d) if *t == DATE => value::encode_date(d),
                            Value::Timestamp(d) => value::encode_timestamp(d),
                            Value::TimestampTz(d, off) => value::encode_timestamp_tz(d, *off),
                            Value::Str(s) => {
                                let d = value::parse_datetime(s).ok_or_else(|| conv(format!("not a date / time: {s}")))?;
                                if *t == DATE { value::encode_date(&d) } else { value::encode_timestamp(&d) }
                            }
                            other => return Err(conv(format!("not a date / time: {other:?}"))),
                        };
                        w.bytes(&b);
                    }
                    CLOB | BLOB => {
                        // A temporary LOB with the value, bound by its locator.
                        let mut lob = self.lob_create_temp(*t == BLOB).await?;
                        self.call_temp_lobs.push(lob.clone());
                        self.lob_write(&mut lob, v).await?;
                        w.bytes(&lob.locator);
                    }
                    other => return Err(OrmError::Unsupported(format!("attribute type {other} in an IN object"))),
                }
            }
        }
        Ok(())
    }
}

/// Flags of an object bind (top-level image).
pub(crate) const BIND_FLAGS: u16 = TOP_LEVEL;

/// The image (TOID, OID, …) of an object value as it travels in a bind.
pub(crate) fn object_bind_bytes(t: &ObjType, image: &[u8]) -> Vec<u8> {
    let mut w = crate::buf::Writer::new();
    w.bytes_2l(&t.oid); // type OID
    w.ub(0); // OID
    w.ub(0); // snapshot
    w.ub(0); // version
    w.ub(image.len() as u64);
    w.ub(u64::from(BIND_FLAGS));
    w.bytes_wl(image);
    w.buf
}
