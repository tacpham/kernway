//! Wall-clock helpers — the Unix-epoch "now" every tier reaches for (a token's `iat`,
//! a row's `timestamp`, a cache entry's age) and a calendar formatter for rendering a
//! stored epoch back to a human date. Shared here in the base crate so a controller, a
//! security layer, and a background task all read the same clock instead of each
//! hand-rolling `SystemTime::now().duration_since(UNIX_EPOCH)`.
//!
//! All three "now" readings clamp a pre-epoch clock (a machine set before 1970) to `0`
//! rather than panicking — a wrong-but-monotonic zero beats a crash on a mis-set host.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. `0` if the clock is set before 1970.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Milliseconds since the Unix epoch. `0` if the clock is set before 1970. Cast to `i64`
/// at the call site when a signed millis timestamp is wanted (`now_millis() as i64`).
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch (`u128`, so it does not overflow). `0` if the clock is
/// set before 1970. Used where a value needs to be unique-ish per call (e.g. an id suffix).
#[must_use]
pub fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Render a Unix-epoch **seconds** value as `"Mon D, YYYY"` (e.g. `"Jul 31, 2027"`).
///
/// Accepts an `i64` so a stored timestamp casts in directly; a non-positive input returns
/// an empty string (a missing/zero date renders as nothing, not `"Jan 1, 1970"`). When the
/// value arrives as text that may be an integer or a float (a JSON number like
/// `"4897554600.0"`), parse it as `f64` first: `format_date(s.parse::<f64>().ok()? as i64)`.
#[must_use]
pub fn format_date(epoch_secs: i64) -> String {
    if epoch_secs <= 0 {
        return String::new();
    }
    let (y, m, d) = civil_from_days(epoch_secs.div_euclid(86_400));
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {}, {}", MON[(m as usize).clamp(1, 12) - 1], d, y)
}

/// Days since the Unix epoch → `(year, month, day)` in the proleptic Gregorian calendar.
/// Howard Hinnant's `civil_from_days` algorithm — no leap-second or timezone handling,
/// which is exactly right for a stored UTC date.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_readings_are_ordered_and_nonzero() {
        let s = now_secs();
        let ms = now_millis();
        let ns = now_nanos();
        assert!(s > 1_700_000_000, "past a 2023 sanity floor");
        assert!(ms >= s * 1_000);
        assert!(ns >= u128::from(ms) * 1_000_000);
    }

    #[test]
    fn format_date_known_epochs() {
        assert_eq!(format_date(0), ""); // epoch/zero → empty, not "Jan 1, 1970"
        assert_eq!(format_date(-5), ""); // negative → empty
        assert_eq!(format_date(1_817_049_203), "Jul 31, 2027");
        assert_eq!(format_date(4_897_554_600), "Mar 13, 2125"); // VIP-style far-future float source
    }

    #[test]
    fn civil_from_days_epoch_and_leap() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1)); // 1970 not a leap year
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // 2000 is a leap year
    }
}
