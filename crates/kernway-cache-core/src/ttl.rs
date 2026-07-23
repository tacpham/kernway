use std::time::Duration;

/// Time-To-Live for cache entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Entry never expires.
    Forever,
    /// Entry expires after the given duration.
    Seconds(u64),
}

impl Ttl {
    /// Expire after `n` seconds.
    pub fn seconds(n: u64) -> Self { Self::Seconds(n) }
    /// Expire after `n` minutes.
    pub fn minutes(n: u64) -> Self { Self::Seconds(n * 60) }
    /// Expire after `n` hours.
    pub fn hours(n: u64) -> Self { Self::Seconds(n * 3600) }
    /// Never expire — the entry leaves only by eviction or `clear`.
    pub fn never() -> Self { Self::Forever }

    /// Convert to Duration. Returns None for Forever.
    pub fn as_duration(self) -> Option<Duration> {
        match self {
            Ttl::Forever => None,
            Ttl::Seconds(s) => Some(Duration::from_secs(s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_seconds_as_duration() {
        assert_eq!(Ttl::seconds(30).as_duration(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn ttl_forever_as_duration_is_none() {
        assert_eq!(Ttl::never().as_duration(), None);
    }

    #[test]
    fn ttl_minutes_converts() {
        assert_eq!(Ttl::minutes(2).as_duration(), Some(Duration::from_secs(120)));
    }

    #[test]
    fn ttl_hours_converts() {
        assert_eq!(Ttl::hours(1).as_duration(), Some(Duration::from_secs(3600)));
    }
}
