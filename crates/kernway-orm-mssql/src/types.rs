//! TDS data types: column / parameter `TYPE_INFO`, value decoding into
//! [`Value`] / entity [`Cell`]s, and parameter encoding for RPC calls.
//!
//! Integers are little-endian. Dates count days from 0001-01-01 (or
//! 1900-01-01 for the legacy `datetime`); times count units of 10^-scale
//! seconds. `decimal` is a sign byte and a little-endian magnitude.

use kernway_orm_core::decode::Cell;
use kernway_orm_core::error::OrmError;
use kernway_orm_sql::{Param, Value};

/// Type codes.
#[allow(missing_docs)]
pub mod ty {
    pub const NULL: u8 = 0x1F;
    pub const INT1: u8 = 0x30;
    pub const BIT: u8 = 0x32;
    pub const INT2: u8 = 0x34;
    pub const INT4: u8 = 0x38;
    pub const DATETIM4: u8 = 0x3A;
    pub const FLT4: u8 = 0x3B;
    pub const MONEY: u8 = 0x3C;
    pub const DATETIME: u8 = 0x3D;
    pub const FLT8: u8 = 0x3E;
    pub const MONEY4: u8 = 0x7A;
    pub const INT8: u8 = 0x7F;
    pub const GUID: u8 = 0x24;
    pub const INTN: u8 = 0x26;
    pub const BITN: u8 = 0x68;
    pub const DECIMALN: u8 = 0x6A;
    pub const NUMERICN: u8 = 0x6C;
    pub const FLTN: u8 = 0x6D;
    pub const MONEYN: u8 = 0x6E;
    pub const DATETIMN: u8 = 0x6F;
    pub const DATEN: u8 = 0x28;
    pub const TIMEN: u8 = 0x29;
    pub const DATETIME2N: u8 = 0x2A;
    pub const DATETIMEOFFSETN: u8 = 0x2B;
    pub const BIGVARBIN: u8 = 0xA5;
    pub const BIGVARCHR: u8 = 0xA7;
    pub const BIGBINARY: u8 = 0xAD;
    pub const BIGCHAR: u8 = 0xAF;
    pub const NVARCHAR: u8 = 0xE7;
    pub const NCHAR: u8 = 0xEF;
    pub const XML: u8 = 0xF1;
    pub const UDT: u8 = 0xF0;
    pub const TEXT: u8 = 0x23;
    pub const IMAGE: u8 = 0x22;
    pub const NTEXT: u8 = 0x63;
    pub const SSVARIANT: u8 = 0x62;
}

/// How a value's length is carried on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LenKind {
    /// A fixed size, no prefix.
    Fixed(usize),
    /// A 1-byte length (0 = NULL).
    Byte,
    /// A 2-byte length (0xFFFF = NULL).
    UShort,
    /// Partially length-prefixed (`(max)` types): 8-byte total, then chunks.
    Plp,
    /// `text` / `ntext` / `image`: text pointer, timestamp, 4-byte length.
    Long,
    /// `sql_variant`: a 4-byte length (0 = NULL).
    Variant,
}

/// A column's or parameter's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    /// The type code.
    pub ty: u8,
    /// Declared maximum length in bytes (0xFFFF for `(max)`).
    pub max_len: u32,
    /// Precision (decimal) .
    pub precision: u8,
    /// Scale (decimal, time, datetime2, datetimeoffset).
    pub scale: u8,
    /// Collation of character data.
    pub collation: Option<[u8; 5]>,
}

impl TypeInfo {
    /// How its values are length-prefixed.
    pub fn len_kind(&self) -> LenKind {
        use ty::*;
        match self.ty {
            NULL => LenKind::Fixed(0),
            INT1 | BIT => LenKind::Fixed(1),
            INT2 => LenKind::Fixed(2),
            INT4 | DATETIM4 | FLT4 | MONEY4 => LenKind::Fixed(4),
            MONEY | DATETIME | FLT8 | INT8 => LenKind::Fixed(8),
            BIGVARBIN | BIGVARCHR | NVARCHAR | BIGBINARY | BIGCHAR if self.max_len == 0xFFFF => LenKind::Plp,
            BIGVARBIN | BIGVARCHR | NVARCHAR | BIGBINARY | BIGCHAR | NCHAR => LenKind::UShort,
            XML | UDT => LenKind::Plp,
            TEXT | IMAGE | NTEXT => LenKind::Long,
            SSVARIANT => LenKind::Variant,
            _ => LenKind::Byte,
        }
    }

    /// Whether the column holds character data in a code page (not UTF-16).
    pub fn is_ansi_text(&self) -> bool {
        matches!(self.ty, ty::BIGVARCHR | ty::BIGCHAR | ty::TEXT)
    }
}

/// Windows-1252 (Latin1_General and the other western collations) bytes
/// 0x80–0x9F; the rest of the code page is Latin-1.
const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™',
    'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// Character data of a non-Unicode column, by its collation: UTF-8 for the
/// `_UTF8` collations (SQL Server 2019+), else Windows-1252.
pub fn ansi_text(bytes: &[u8], collation: Option<[u8; 5]>) -> String {
    // The UTF-8 flag is bit 27 of the 4-byte collation info (MS-TDS 2.2.5.1.2).
    let utf8 = collation.is_some_and(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> 27) & 1 == 1);
    if utf8 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    bytes.iter().map(|&b| if (0x80..0xA0).contains(&b) { CP1252_HIGH[(b - 0x80) as usize] } else { b as char }).collect()
}

/// UTF-16LE bytes as a string.
pub fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units)
}

/// A string as UTF-16LE bytes.
pub fn to_utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn le_uint(b: &[u8]) -> u64 {
    b.iter().rev().fold(0u64, |a, &x| (a << 8) | u64::from(x))
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days from 0001-01-01 to 1970-01-01.
const DAYS_0001_TO_1970: i64 = 719_162;
/// Days from 1900-01-01 to 1970-01-01.
const DAYS_1900_TO_1970: i64 = 25_567;

fn date_text(days_since_0001: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_0001 - DAYS_0001_TO_1970);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM:SS[.fraction]` from units of 10^-scale seconds.
fn time_text(units: u64, scale: u8) -> String {
    let div = 10u64.pow(u32::from(scale));
    let secs = units / div;
    let frac = units % div;
    let base = format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60);
    if scale == 0 {
        base
    } else {
        format!("{base}.{frac:0w$}", w = scale as usize)
    }
}

fn time_bytes(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3 | 4 => 4,
        _ => 5,
    }
}

/// A `decimal` magnitude and scale as text.
fn decimal_text(positive: bool, magnitude: u128, scale: u8) -> String {
    let digits = magnitude.to_string();
    let s = scale as usize;
    let body = if s == 0 {
        digits
    } else if digits.len() > s {
        format!("{}.{}", &digits[..digits.len() - s], &digits[digits.len() - s..])
    } else {
        format!("0.{}{digits}", "0".repeat(s - digits.len()))
    };
    if positive || magnitude == 0 {
        body
    } else {
        format!("-{body}")
    }
}

fn guid_text(b: &[u8]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// A value of type `ti` from its bytes (`None`: NULL).
pub fn value(ti: &TypeInfo, raw: Option<&[u8]>) -> Value {
    use ty::*;
    let Some(b) = raw else { return Value::Null };
    let int = |b: &[u8]| -> i64 {
        match b.len() {
            1 => i64::from(b[0]),
            2 => i64::from(i16::from_le_bytes([b[0], b[1]])),
            4 => i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            8 => i64::from_le_bytes(b.try_into().unwrap_or([0; 8])),
            _ => 0,
        }
    };
    match ti.ty {
        INT1 | INT2 | INT4 | INT8 | INTN => Value::Int(int(b)),
        BIT | BITN => Value::Bool(b.first().is_some_and(|x| *x != 0)),
        FLT4 | FLT8 | FLTN => match b.len() {
            4 => {
                let f = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                Value::Float(if f.is_finite() { f.to_string().parse().unwrap_or(f64::from(f)) } else { f64::from(f) })
            }
            _ => Value::Float(f64::from_le_bytes(b.try_into().unwrap_or([0; 8]))),
        },
        MONEY | MONEY4 | MONEYN => {
            let cents4 = if b.len() == 4 {
                i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                let hi = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let lo = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                (i64::from(hi) << 32) | i64::from(lo)
            };
            Value::Decimal(decimal_text(cents4 >= 0, u128::from(cents4.unsigned_abs()), 4))
        }
        DECIMALN | NUMERICN => {
            let positive = b.first() == Some(&1);
            let mag = b[1..].iter().rev().fold(0u128, |a, &x| (a << 8) | u128::from(x));
            Value::Decimal(decimal_text(positive, mag, ti.scale))
        }
        DATETIME | DATETIM4 | DATETIMN => {
            if b.len() == 4 {
                let days = i64::from(u16::from_le_bytes([b[0], b[1]]));
                let mins = u64::from(u16::from_le_bytes([b[2], b[3]]));
                let (y, m, d) = civil_from_days(days - DAYS_1900_TO_1970);
                Value::DateTime(format!("{y:04}-{m:02}-{d:02} {}", time_text(mins * 60, 0)))
            } else {
                let days = i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                let ticks = u64::from(u32::from_le_bytes([b[4], b[5], b[6], b[7]]));
                // 1/300 s ticks, shown to the millisecond as SQL Server does.
                let ms = (ticks * 10 + 1) / 3;
                let (y, m, d) = civil_from_days(days - DAYS_1900_TO_1970);
                Value::DateTime(format!("{y:04}-{m:02}-{d:02} {}", time_text(ms, 3)))
            }
        }
        DATEN => Value::Date(date_text(le_uint(&b[..3]) as i64)),
        TIMEN => Value::Time(time_text(le_uint(b), ti.scale)),
        DATETIME2N => {
            let tb = time_bytes(ti.scale);
            let t = time_text(le_uint(&b[..tb]), ti.scale);
            Value::DateTime(format!("{} {t}", date_text(le_uint(&b[tb..tb + 3]) as i64)))
        }
        DATETIMEOFFSETN => {
            let tb = time_bytes(ti.scale);
            let units = le_uint(&b[..tb]);
            let days = le_uint(&b[tb..tb + 3]) as i64;
            let off = i64::from(i16::from_le_bytes([b[tb + 3], b[tb + 4]]));
            // Stored in UTC: show local time with its offset.
            let div = 10i64.pow(u32::from(ti.scale));
            let day_units = 86_400 * div;
            let mut local = units as i64 + off * 60 * div;
            let mut d = days;
            if local < 0 {
                local += day_units;
                d -= 1;
            } else if local >= day_units {
                local -= day_units;
                d += 1;
            }
            let sign = if off < 0 { '-' } else { '+' };
            Value::DateTime(format!("{} {} {sign}{:02}:{:02}", date_text(d), time_text(local as u64, ti.scale), off.abs() / 60, off.abs() % 60))
        }
        GUID => Value::Text(guid_text(b)),
        BIGVARCHR | BIGCHAR | TEXT => Value::Text(ansi_text(b, ti.collation)),
        NVARCHAR | NCHAR | NTEXT | XML => Value::Text(utf16(b)),
        BIGVARBIN | BIGBINARY | IMAGE | UDT => Value::Bytes(b.to_vec()),
        SSVARIANT => variant(b),
        _ => Value::Bytes(b.to_vec()),
    }
}

/// A `sql_variant`: base type, property bytes, then the value.
fn variant(b: &[u8]) -> Value {
    if b.len() < 2 {
        return Value::Null;
    }
    let base = b[0];
    let props = b[1] as usize;
    let Some(data) = b.get(2 + props..) else { return Value::Null };
    let p = &b[2..2 + props];
    let mut ti = TypeInfo { ty: base, max_len: data.len() as u32, precision: 0, scale: 0, collation: None };
    match base {
        ty::DECIMALN | ty::NUMERICN if props >= 2 => {
            ti.precision = p[0];
            ti.scale = p[1];
        }
        ty::TIMEN | ty::DATETIME2N | ty::DATETIMEOFFSETN if props >= 1 => ti.scale = p[0],
        ty::BIGVARCHR | ty::BIGCHAR | ty::NVARCHAR | ty::NCHAR if props >= 5 => ti.collation = Some([p[0], p[1], p[2], p[3], p[4]]),
        _ => {}
    }
    value(&ti, Some(data))
}

/// A cell for the entity decoder (binary data borrowed from the row).
pub fn cell<'a>(ti: &TypeInfo, raw: Option<&'a [u8]>) -> Cell<'a> {
    if let (Some(b), ty::BIGVARBIN | ty::BIGBINARY | ty::IMAGE | ty::UDT) = (raw, ti.ty) {
        return Cell::Bytes(b);
    }
    match value(ti, raw) {
        Value::Null => Cell::Null,
        Value::Int(i) => Cell::Int(i),
        Value::Bool(b) => Cell::Bool(b),
        Value::Float(f) => Cell::F64(f),
        other => Cell::Owned(other.to_text().unwrap_or_default()),
    }
}

/// What a parameter is sent as.
pub struct Encoded {
    /// The SQL type for the `@params` declaration (`bigint`, `nvarchar(4000)`, …).
    pub decl: String,
    /// `TYPE_INFO` bytes.
    pub type_info: Vec<u8>,
    /// The value bytes, length prefix included.
    pub value: Vec<u8>,
}

fn plp(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    for chunk in data.chunks(0x7FFF_FFFF) {
        if !chunk.is_empty() {
            out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn nvarchar(s: &str, collation: [u8; 5]) -> Encoded {
    let data = to_utf16(s);
    let mut ti = vec![ty::NVARCHAR];
    let mut value = Vec::with_capacity(data.len() + 8);
    let decl = if data.len() <= 8000 {
        ti.extend_from_slice(&8000u16.to_le_bytes());
        value.extend_from_slice(&(data.len() as u16).to_le_bytes());
        value.extend_from_slice(&data);
        "nvarchar(4000)".to_string()
    } else {
        ti.extend_from_slice(&0xFFFFu16.to_le_bytes());
        plp(&mut value, &data);
        "nvarchar(max)".to_string()
    };
    ti.extend_from_slice(&collation);
    Encoded { decl, type_info: ti, value }
}

fn varbinary(b: &[u8]) -> Encoded {
    let mut ti = vec![ty::BIGVARBIN];
    let mut value = Vec::with_capacity(b.len() + 8);
    let decl = if b.len() <= 8000 {
        ti.extend_from_slice(&8000u16.to_le_bytes());
        value.extend_from_slice(&(b.len() as u16).to_le_bytes());
        value.extend_from_slice(b);
        "varbinary(8000)".to_string()
    } else {
        ti.extend_from_slice(&0xFFFFu16.to_le_bytes());
        plp(&mut value, b);
        "varbinary(max)".to_string()
    };
    Encoded { decl, type_info: ti, value }
}

/// What a parameter's target is, as far as the driver knows (from
/// `sp_describe_undeclared_parameters`, when it ran).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    /// Unknown, or anything else.
    #[default]
    Other,
    /// `binary` / `varbinary` / `image`: text and NULLs must travel as binary.
    Binary,
}

/// A parameter as an RPC argument.
pub fn encode(p: &Param, target: Target, collation: [u8; 5]) -> Result<Encoded, OrmError> {
    Ok(match p {
        Param::Null => match target {
            Target::Binary => Encoded { decl: "varbinary(8000)".into(), type_info: [vec![ty::BIGVARBIN], 8000u16.to_le_bytes().to_vec()].concat(), value: 0xFFFFu16.to_le_bytes().to_vec() },
            Target::Other => Encoded {
                decl: "nvarchar(4000)".into(),
                type_info: [vec![ty::NVARCHAR], 8000u16.to_le_bytes().to_vec(), collation.to_vec()].concat(),
                value: 0xFFFFu16.to_le_bytes().to_vec(),
            },
        },
        Param::Int(i) => Encoded { decl: "bigint".into(), type_info: vec![ty::INTN, 8], value: [vec![8], i.to_le_bytes().to_vec()].concat() },
        Param::UInt(u) => match i64::try_from(*u) {
            Ok(i) => return encode(&Param::Int(i), target, collation),
            Err(_) => {
                let mut v = vec![17, 1];
                v.extend_from_slice(&u128::from(*u).to_le_bytes());
                Encoded { decl: "decimal(20,0)".into(), type_info: vec![ty::DECIMALN, 17, 20, 0], value: v }
            }
        },
        Param::Float(f) => {
            if !f.is_finite() {
                return Err(OrmError::Query(format!("SQL Server has no {f} (float columns hold finite numbers only)")));
            }
            Encoded { decl: "float".into(), type_info: vec![ty::FLTN, 8], value: [vec![8], f.to_le_bytes().to_vec()].concat() }
        }
        Param::Bool(b) => Encoded { decl: "bit".into(), type_info: vec![ty::BITN, 1], value: vec![1, u8::from(*b)] },
        Param::Str(s) if target == Target::Binary => varbinary(s.as_bytes()),
        Param::Str(s) => nvarchar(s, collation),
        Param::Bytes(b) => varbinary(b),
        p @ (Param::Array(_) | Param::Object(_)) => nvarchar(&p.to_json().to_string(), collation),
    })
}

/// A NULL of the type an `OUTPUT` parameter of kind `k` needs.
pub fn out_null(k: kernway_orm_sql::OutKind, collation: [u8; 5]) -> Encoded {
    use kernway_orm_sql::OutKind as K;
    match k {
        K::Number => Encoded { decl: "decimal(38,10)".into(), type_info: vec![ty::DECIMALN, 17, 38, 10], value: vec![0] },
        K::Float => Encoded { decl: "float".into(), type_info: vec![ty::FLTN, 8], value: vec![0] },
        K::Bytes => Encoded { decl: "varbinary(max)".into(), type_info: [vec![ty::BIGVARBIN], 0xFFFFu16.to_le_bytes().to_vec()].concat(), value: u64::MAX.to_le_bytes().to_vec() },
        K::Date | K::DateTime => Encoded { decl: "datetime2(7)".into(), type_info: vec![ty::DATETIME2N, 7], value: vec![0] },
        _ => Encoded {
            decl: "nvarchar(max)".into(),
            type_info: [vec![ty::NVARCHAR], 0xFFFFu16.to_le_bytes().to_vec(), collation.to_vec()].concat(),
            value: u64::MAX.to_le_bytes().to_vec(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ti(ty: u8, scale: u8) -> TypeInfo {
        TypeInfo { ty, max_len: 0, precision: 0, scale, collation: None }
    }

    #[test]
    fn values_decode() {
        assert_eq!(value(&ti(ty::INTN, 0), Some(&(-5i32).to_le_bytes())), Value::Int(-5));
        assert_eq!(value(&ti(ty::BITN, 0), Some(&[1])), Value::Bool(true));
        assert_eq!(value(&ti(ty::FLTN, 0), Some(&0.1f32.to_le_bytes())), Value::Float(0.1));
        // decimal(10,2) -12.34: sign 0, magnitude 1234.
        let mut d = vec![0u8];
        d.extend_from_slice(&1234u32.to_le_bytes());
        assert_eq!(value(&ti(ty::DECIMALN, 2), Some(&d)), Value::Decimal("-12.34".into()));
        let mut d = vec![1u8];
        d.extend_from_slice(&5u32.to_le_bytes());
        assert_eq!(value(&ti(ty::DECIMALN, 3), Some(&d)), Value::Decimal("0.005".into()));
        // money 12.3456 = 123456 / 10^4.
        let m = 123_456i64;
        let mut b = ((m >> 32) as i32).to_le_bytes().to_vec();
        b.extend_from_slice(&(m as u32).to_le_bytes());
        assert_eq!(value(&ti(ty::MONEYN, 0), Some(&b)), Value::Decimal("12.3456".into()));
        // date 2024-02-29: days since 0001-01-01.
        let days = 738_944u32;
        assert_eq!(value(&ti(ty::DATEN, 0), Some(&days.to_le_bytes()[..3])), Value::Date("2024-02-29".into()));
        // time(7) 13:14:15.1234567.
        let units: u64 = (13 * 3600 + 14 * 60 + 15) * 10_000_000 + 1_234_567;
        assert_eq!(value(&ti(ty::TIMEN, 7), Some(&units.to_le_bytes()[..5])), Value::Time("13:14:15.1234567".into()));
        // datetime 1900-01-02 00:00:01.000 (300 ticks).
        let mut dt = 1i32.to_le_bytes().to_vec();
        dt.extend_from_slice(&300u32.to_le_bytes());
        assert_eq!(value(&ti(ty::DATETIMN, 0), Some(&dt)), Value::DateTime("1900-01-02 00:00:01.000".into()));
        // datetimeoffset(0) 2024-01-01 00:30:00 UTC at +07:00 → 07:30 local.
        let mut dto = (30u64 * 60).to_le_bytes()[..3].to_vec();
        dto.extend_from_slice(&738_885u32.to_le_bytes()[..3]);
        dto.extend_from_slice(&420i16.to_le_bytes());
        assert_eq!(value(&ti(ty::DATETIMEOFFSETN, 0), Some(&dto)), Value::DateTime("2024-01-01 07:30:00 +07:00".into()));
        let g = [0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(value(&ti(ty::GUID, 0), Some(&g)), Value::Text("00112233-4455-6677-8899-AABBCCDDEEFF".into()));
        assert_eq!(value(&ti(ty::NVARCHAR, 0), Some(&to_utf16("Việt 😀"))), Value::Text("Việt 😀".into()));
        assert_eq!(ansi_text(&[0x80, b'a', 0xE9], None), "€aé");
        assert_eq!(value(&ti(ty::INTN, 0), None), Value::Null);
    }

    #[test]
    fn params_encode() {
        let c = [0x09, 0x04, 0xD0, 0x00, 0x34];
        let e = encode(&Param::Int(-2), Target::Other, c).unwrap();
        assert_eq!((e.decl.as_str(), e.type_info, e.value), ("bigint", vec![ty::INTN, 8], [vec![8], (-2i64).to_le_bytes().to_vec()].concat()));
        let e = encode(&Param::Str("ab".into()), Target::Other, c).unwrap();
        assert_eq!(e.decl, "nvarchar(4000)");
        assert_eq!(e.value, vec![4, 0, b'a', 0, b'b', 0]);
        let long = "x".repeat(5000);
        assert_eq!(encode(&Param::Str(long), Target::Other, c).unwrap().decl, "nvarchar(max)");
        assert_eq!(encode(&Param::Null, Target::Binary, c).unwrap().decl, "varbinary(8000)");
        assert_eq!(encode(&Param::Str("hi".into()), Target::Binary, c).unwrap().decl, "varbinary(8000)");
        assert_eq!(encode(&Param::UInt(u64::MAX), Target::Other, c).unwrap().decl, "decimal(20,0)");
        assert!(encode(&Param::Float(f64::NAN), Target::Other, c).is_err());
        let big = encode(&Param::Bytes(vec![7; 9000]), Target::Other, c).unwrap();
        assert_eq!(big.decl, "varbinary(max)");
        assert_eq!(&big.value[..8], &9000u64.to_le_bytes());
    }
}
