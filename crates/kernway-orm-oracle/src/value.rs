//! Oracle value encodings: NUMBER (base-100), DATE / TIMESTAMP, binary floats,
//! BOOLEAN, strings and RAW — both directions.

use std::fmt;

/// Oracle type numbers used on the wire.
pub(crate) mod ora_type {
    #![allow(dead_code)]
    pub const VARCHAR: u8 = 1;
    pub const NUMBER: u8 = 2;
    pub const BINARY_INTEGER: u8 = 3;
    pub const LONG: u8 = 8;
    pub const ROWID: u8 = 11;
    pub const DATE: u8 = 12;
    pub const RAW: u8 = 23;
    pub const LONG_RAW: u8 = 24;
    pub const CHAR: u8 = 96;
    pub const BINARY_FLOAT: u8 = 100;
    pub const BINARY_DOUBLE: u8 = 101;
    pub const CURSOR: u8 = 102;
    pub const CLOB: u8 = 112;
    pub const BLOB: u8 = 113;
    pub const JSON: u8 = 119;
    pub const VECTOR: u8 = 127;
    pub const OBJECT: u8 = 109;
    pub const BFILE: u8 = 114;
    pub const TIMESTAMP: u8 = 180;
    pub const TIMESTAMP_TZ: u8 = 181;
    pub const INTERVAL_YM: u8 = 182;
    pub const INTERVAL_DS: u8 = 183;
    pub const UROWID: u8 = 208;
    pub const TIMESTAMP_LTZ: u8 = 231;
    pub const BOOLEAN: u8 = 252;
}

/// A calendar date-time as Oracle stores it (no time zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateTime {
    /// Year (e.g. 2026).
    pub year: i32,
    /// Month, 1-12.
    pub month: u8,
    /// Day of month, 1-31.
    pub day: u8,
    /// Hour, 0-23.
    pub hour: u8,
    /// Minute, 0-59.
    pub minute: u8,
    /// Second, 0-59.
    pub second: u8,
    /// Fractional seconds in nanoseconds (0 for a DATE).
    pub nanos: u32,
}

/// An interval as Oracle's TO_CHAR prints it: `+YY-MM` / `+DD HH:MM:SS.ff`,
/// the leading field padded to `lead` digits, `frac` fractional digits.
pub fn fmt_interval(v: &Value, lead: u8, frac: u8) -> String {
    match *v {
        Value::IntervalYm { years, months } => {
            let neg = years < 0 || months < 0;
            format!(
                "{}{:0w$}-{:02}",
                if neg { '-' } else { '+' },
                years.unsigned_abs(),
                months.unsigned_abs(),
                w = usize::from(lead)
            )
        }
        Value::IntervalDs { days, hours, minutes, seconds, nanos } => {
            let neg = days < 0 || hours < 0 || minutes < 0 || seconds < 0 || nanos < 0;
            let mut s = format!(
                "{}{:0w$} {:02}:{:02}:{:02}",
                if neg { '-' } else { '+' },
                days.unsigned_abs(),
                hours.unsigned_abs(),
                minutes.unsigned_abs(),
                seconds.unsigned_abs(),
                w = usize::from(lead)
            );
            if frac > 0 {
                s.push('.');
                s.push_str(&format!("{:09}", nanos.unsigned_abs())[..usize::from(frac.min(9))]);
            }
            s
        }
        _ => String::new(),
    }
}

/// `+HH:MM` / `-HH:MM` for an offset in minutes.
pub fn fmt_offset(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", m / 60, m % 60)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    ((yoe + era * 400 + i64::from(m <= 2)) as i32, m, d)
}

impl DateTime {
    /// This wall time shifted by `minutes` (dates roll over as needed).
    pub fn add_minutes(&self, minutes: i64) -> DateTime {
        let total = days_from_civil(self.year, self.month, self.day) * 1440
            + i64::from(self.hour) * 60
            + i64::from(self.minute)
            + minutes;
        let (days, mins) = (total.div_euclid(1440), total.rem_euclid(1440));
        let (year, month, day) = civil_from_days(days);
        DateTime { year, month, day, hour: (mins / 60) as u8, minute: (mins % 60) as u8, ..*self }
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )?;
        if self.nanos != 0 {
            write!(f, ".{:09}", self.nanos)?;
        }
        Ok(())
    }
}

/// A value read from or bound to Oracle.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL (Oracle also treats an empty string as NULL).
    Null,
    /// VARCHAR2 / CHAR / NVARCHAR2 / LONG text.
    Str(String),
    /// NUMBER, kept as its exact decimal text (e.g. `"-123.45"`).
    Number(String),
    /// BINARY_FLOAT / BINARY_DOUBLE.
    Float(f64),
    /// DATE (second precision).
    Date(DateTime),
    /// TIMESTAMP (nanosecond precision; WITH (LOCAL) TIME ZONE values are read
    /// as their UTC wall time).
    Timestamp(DateTime),
    /// RAW / LONG RAW bytes.
    Raw(Vec<u8>),
    /// BOOLEAN (Oracle 23ai+).
    Bool(bool),
    /// TIMESTAMP WITH TIME ZONE: the local wall time and its UTC offset in
    /// minutes.
    TimestampTz(DateTime, i16),
    /// INTERVAL YEAR TO MONTH (both parts carry the sign).
    IntervalYm {
        /// Years.
        years: i32,
        /// Months (-11 to 11).
        months: i32,
    },
    /// INTERVAL DAY TO SECOND (every part carries the sign).
    IntervalDs {
        /// Days.
        days: i32,
        /// Hours.
        hours: i32,
        /// Minutes.
        minutes: i32,
        /// Seconds.
        seconds: i32,
        /// Fractional seconds in nanoseconds.
        nanos: i32,
    },
    /// A REF CURSOR returned by PL/SQL, already fetched to the end.
    Cursor(ResultSet),
}

/// The columns and rows of a REF CURSOR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSet {
    /// Result columns.
    pub columns: Vec<crate::exec::Column>,
    /// Every row.
    pub rows: Vec<Vec<Value>>,
}

impl Value {
    /// The value as text, as SQL*Plus-like output would show it.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Str(s) | Value::Number(s) => Some(s.clone()),
            Value::Float(f) => Some(f.to_string()),
            Value::Date(d) | Value::Timestamp(d) => Some(d.to_string()),
            Value::TimestampTz(d, off) => Some(format!("{d} {}", fmt_offset(*off))),
            Value::IntervalYm { .. } | Value::IntervalDs { .. } => Some(fmt_interval(self, 9, 9)),
            Value::Raw(b) => Some(b.iter().map(|x| format!("{x:02X}")).collect()),
            Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            Value::Cursor(rs) => Some(format!("<cursor: {} rows>", rs.rows.len())),
        }
    }

    /// The value as an integer, if it is an integral NUMBER.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(s) | Value::Str(s) => s.parse().ok(),
            Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Number(v.to_string())
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

// --- NUMBER -----------------------------------------------------------------

/// Encode decimal text (`[-]digits[.digits][e[+-]digits]`) as an Oracle NUMBER.
pub(crate) fn encode_number(text: &str) -> Result<Vec<u8>, String> {
    let s = text.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().map_err(|_| format!("bad number {text:?}"))?),
        None => (s, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int_part.is_empty() && frac_part.is_empty()
        || !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit())
    {
        return Err(format!("bad number {text:?}"));
    }
    let mut digits: Vec<u8> = Vec::new();
    for b in int_part.bytes() {
        if b == b'0' && digits.is_empty() {
            continue;
        }
        digits.push(b - b'0');
    }
    let mut dpi = digits.len() as i32;
    for b in frac_part.bytes() {
        if b == b'0' && digits.is_empty() {
            dpi -= 1;
            continue;
        }
        digits.push(b - b'0');
    }
    dpi += exponent;
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        return Ok(vec![0x80]);
    }
    if digits.len() > 40 || !(-129..=126).contains(&dpi) {
        return Err(format!("{text:?} has no Oracle NUMBER representation"));
    }
    let mut prepend_zero = false;
    if dpi.rem_euclid(2) == 1 {
        prepend_zero = true;
        digits.push(0);
        dpi += 1;
    }
    if digits.len() % 2 == 1 {
        digits.push(0);
    }
    let mut exp = (dpi / 2 + 192) as u8;
    if neg {
        exp = !exp;
    }
    let mut out = vec![exp];
    let mut pos = 0;
    for pair in 0..digits.len() / 2 {
        let d = if pair == 0 && prepend_zero {
            pos += 1;
            digits[pos - 1]
        } else {
            pos += 2;
            digits[pos - 2] * 10 + digits[pos - 1]
        };
        out.push(if neg { 101 - d } else { d + 1 });
    }
    if neg && digits.len() < 40 {
        out.push(102);
    }
    Ok(out)
}

/// Decode an Oracle NUMBER to exact decimal text.
pub(crate) fn decode_number(b: &[u8]) -> Result<String, String> {
    if b.is_empty() || b.len() > 21 {
        return Err(format!("bad NUMBER length {}", b.len()));
    }
    let positive = b[0] & 0x80 != 0;
    let e = if positive { i32::from(b[0]) - 193 } else { i32::from(!b[0]) - 193 };
    let mut dpi = e * 2 + 2;
    if b.len() == 1 {
        return Ok(if positive { "0".into() } else { "-1e126".into() });
    }
    let mut n = b.len();
    if !positive && b[n - 1] == 102 {
        n -= 1;
    }
    let mut digits: Vec<u8> = Vec::new();
    for (i, &byte) in b.iter().enumerate().take(n).skip(1) {
        let v = if positive { byte.wrapping_sub(1) } else { 101u8.wrapping_sub(byte) };
        let hi = v / 10;
        if hi == 0 && digits.is_empty() {
            dpi -= 1;
        } else if hi == 10 {
            digits.extend_from_slice(&[1, 0]);
            dpi += 1;
        } else {
            digits.push(hi);
        }
        let lo = v % 10;
        if lo != 0 || i < n - 1 {
            digits.push(lo);
        }
    }
    while digits.len() > 1 && digits.last() == Some(&0) && (digits.len() as i32) > dpi {
        digits.pop();
    }
    let mut out = String::new();
    if !positive {
        out.push('-');
    }
    let len = digits.len() as i32;
    if dpi <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-dpi) as usize));
        out.extend(digits.iter().map(|d| (b'0' + d) as char));
    } else {
        for (i, d) in digits.iter().enumerate() {
            if i as i32 == dpi {
                out.push('.');
            }
            out.push((b'0' + d) as char);
        }
        if dpi > len {
            out.extend(std::iter::repeat_n('0', (dpi - len) as usize));
        }
    }
    Ok(out)
}

// --- DATE / TIMESTAMP ---------------------------------------------------------

pub(crate) fn encode_date(d: &DateTime) -> Vec<u8> {
    vec![
        (d.year / 100 + 100) as u8,
        (d.year % 100 + 100) as u8,
        d.month,
        d.day,
        d.hour + 1,
        d.minute + 1,
        d.second + 1,
    ]
}

pub(crate) fn encode_timestamp(d: &DateTime) -> Vec<u8> {
    let mut b = encode_date(d);
    if d.nanos != 0 {
        b.extend_from_slice(&d.nanos.to_be_bytes());
    }
    b
}

/// TIMESTAMP WITH TIME ZONE: 11 bytes of UTC date-time, then the offset
/// (hour + 20, minute + 60). A named region (high bit) is refused rather
/// than guessed.
pub(crate) fn decode_timestamp_tz(b: &[u8]) -> Result<Value, String> {
    let utc = decode_date(b)?;
    if b.len() <= 11 || b[11] == 0 || b[12] == 0 {
        return Ok(Value::TimestampTz(utc, 0));
    }
    if b[11] & 0x80 != 0 {
        return Err("TIMESTAMP WITH TIME ZONE in a named region (e.g. 'Asia/Ho_Chi_Minh') is not \
                    supported; select it with TO_CHAR(…, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM') or \
                    SYS_EXTRACT_UTC(…)"
            .into());
    }
    let off = (i16::from(b[11]) - 20) * 60 + (i16::from(b[12]) - 60);
    Ok(Value::TimestampTz(utc.add_minutes(i64::from(off)), off))
}

/// Encode a local wall time with its offset as TIMESTAMP WITH TIME ZONE.
pub(crate) fn encode_timestamp_tz(local: &DateTime, offset_minutes: i16) -> Vec<u8> {
    let utc = local.add_minutes(-i64::from(offset_minutes));
    let mut b = encode_date(&utc);
    b.extend_from_slice(&utc.nanos.to_be_bytes());
    let (h, m) = (offset_minutes / 60, offset_minutes % 60);
    b.push((h + 20) as u8);
    b.push((m + 60) as u8);
    b
}

pub(crate) fn decode_date(b: &[u8]) -> Result<DateTime, String> {
    if b.len() < 7 {
        return Err(format!("bad DATE length {}", b.len()));
    }
    let nanos = if b.len() >= 11 { u32::from_be_bytes([b[7], b[8], b[9], b[10]]) } else { 0 };
    Ok(DateTime {
        year: (i32::from(b[0]) - 100) * 100 + i32::from(b[1]) - 100,
        month: b[2],
        day: b[3],
        hour: b[4].wrapping_sub(1),
        minute: b[5].wrapping_sub(1),
        second: b[6].wrapping_sub(1),
        nanos,
    })
}

// --- binary floats ---------------------------------------------------------------

pub(crate) fn encode_double(v: f64) -> Vec<u8> {
    let mut b = v.to_be_bytes();
    if b[0] & 0x80 == 0 {
        b[0] |= 0x80;
    } else {
        b.iter_mut().for_each(|x| *x = !*x);
    }
    b.to_vec()
}

pub(crate) fn decode_double(b: &[u8]) -> Result<f64, String> {
    let mut a: Vec<u8> = b.to_vec();
    if a.is_empty() {
        return Err("empty binary float".into());
    }
    if a[0] & 0x80 != 0 {
        a[0] &= 0x7F;
    } else {
        a.iter_mut().for_each(|x| *x = !*x);
    }
    match a.len() {
        8 => Ok(f64::from_be_bytes(a.try_into().unwrap())),
        4 => Ok(f64::from(f32::from_be_bytes(a.try_into().unwrap()))),
        n => Err(format!("bad binary float length {n}")),
    }
}

/// Decode a fetched value of `ora_type` / `csfrm`.
pub(crate) fn decode(ora_type: u8, csfrm: u8, b: Vec<u8>) -> Result<Value, String> {
    use ora_type::*;
    Ok(match ora_type {
        NUMBER | BINARY_INTEGER => Value::Number(decode_number(&b)?),
        DATE => Value::Date(decode_date(&b)?),
        TIMESTAMP | TIMESTAMP_LTZ => Value::Timestamp(decode_date(&b)?),
        TIMESTAMP_TZ => decode_timestamp_tz(&b)?,
        INTERVAL_YM if b.len() >= 5 => Value::IntervalYm {
            years: (i64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) - 0x8000_0000) as i32,
            months: i32::from(b[4]) - 60,
        },
        INTERVAL_DS if b.len() >= 11 => Value::IntervalDs {
            days: (i64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) - 0x8000_0000) as i32,
            hours: i32::from(b[4]) - 60,
            minutes: i32::from(b[5]) - 60,
            seconds: i32::from(b[6]) - 60,
            nanos: (i64::from(u32::from_be_bytes([b[7], b[8], b[9], b[10]])) - 0x8000_0000) as i32,
        },
        INTERVAL_YM | INTERVAL_DS => return Err(format!("bad INTERVAL length {}", b.len())),
        BINARY_FLOAT | BINARY_DOUBLE => Value::Float(decode_double(&b)?),
        RAW | LONG_RAW => Value::Raw(b),
        BOOLEAN => Value::Bool(b.first() == Some(&1)),
        _ if csfrm == 2 => {
            let units: Vec<u16> = b.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
            Value::Str(String::from_utf16_lossy(&units))
        }
        _ => Value::Str(String::from_utf8_lossy(&b).into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_tz_round_trips_with_offsets_and_day_rollover() {
        let local = DateTime { year: 2026, month: 1, day: 1, hour: 0, minute: 30, second: 5, nanos: 120_000_000 };
        for off in [0i16, 420, -330, 840, -720] {
            let b = encode_timestamp_tz(&local, off);
            assert_eq!(decode_timestamp_tz(&b).unwrap(), Value::TimestampTz(local, off), "offset {off}");
        }
        // 00:30 local at +07:00 is 17:30 UTC the previous day, previous year.
        let utc = decode_date(&encode_timestamp_tz(&local, 420)).unwrap();
        assert_eq!((utc.year, utc.month, utc.day, utc.hour, utc.minute), (2025, 12, 31, 17, 30));
        let leap = DateTime { year: 2028, month: 2, day: 28, hour: 23, minute: 0, ..DateTime::default() };
        assert_eq!(leap.add_minutes(90).day, 29);
        assert_eq!(fmt_offset(-330), "-05:30");
    }

    #[test]
    fn intervals_format_like_to_char() {
        let ym = Value::IntervalYm { years: 1, months: 2 };
        assert_eq!(fmt_interval(&ym, 2, 0), "+01-02");
        let ds = Value::IntervalDs { days: -3, hours: -4, minutes: -5, seconds: -6, nanos: -789_000_000 };
        assert_eq!(fmt_interval(&ds, 2, 3), "-03 04:05:06.789");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn numbers_encode_like_the_reference_table() {
        for (v, want) in [
            ("0", "80"),
            ("1", "C1 02"),
            ("-1", "3E 64 66"),
            ("5", "C1 06"),
            ("-5", "3E 60 66"),
            ("100", "C2 02"),
            ("12345", "C3 02 18 2E"),
            ("123.45", "C2 02 18 2E"),
            ("-123.45", "3D 64 4E 38 66"),
            ("0.5", "C0 33"),
            ("1e20", "CB 02"),
            ("-0.001", "40 5B 66"),
        ] {
            assert_eq!(hex(&encode_number(v).unwrap()), want, "{v}");
        }
    }

    #[test]
    fn numbers_round_trip() {
        for v in [
            "0", "1", "-1", "5", "100", "12345", "123.45", "-123.45", "0.5", "-0.001", "100000000000000000000",
            "9223372036854775807", "-9223372036854775808", "3.14159265358979", "0.000123", "1000.01", "10",
        ] {
            assert_eq!(decode_number(&encode_number(v).unwrap()).unwrap(), v, "{v}");
        }
        assert_eq!(decode_number(&encode_number("1e20").unwrap()).unwrap(), "100000000000000000000");
        assert!(encode_number("12a").is_err());
    }

    #[test]
    fn dates_and_doubles_round_trip() {
        let d = DateTime { year: 2024, month: 3, day: 15, hour: 13, minute: 45, second: 30, nanos: 0 };
        assert_eq!(hex(&encode_date(&d)), "78 7C 03 0F 0E 2E 1F");
        assert_eq!(decode_date(&encode_date(&d)).unwrap(), d);
        let t = DateTime { nanos: 123_456_000, ..d };
        assert_eq!(encode_timestamp(&t).len(), 11);
        assert_eq!(encode_timestamp(&d).len(), 7, "zero fraction is sent as 7 bytes");
        assert_eq!(decode_date(&encode_timestamp(&t)).unwrap(), t);
        for v in [0.0, 1.5, -2.25, 1e300, -1e-300] {
            assert_eq!(decode_double(&encode_double(v)).unwrap(), v);
        }
        assert_eq!(d.to_string(), "2024-03-15 13:45:30");
    }
}
