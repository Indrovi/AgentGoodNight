//! Time handling. All timestamps are Unix epoch milliseconds so that events
//! from hooks, transcripts and rollouts can be ordered on one axis.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Millis = i64;

pub const SECOND: Millis = 1_000;
pub const MINUTE: Millis = 60 * SECOND;
pub const HOUR: Millis = 60 * MINUTE;

pub fn now_millis() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as Millis).unwrap_or(0)
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Millis;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        now_millis()
    }
}

/// Manually driven clock for deterministic simulations and tests.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(start: Millis) -> Self {
        Self(AtomicI64::new(start))
    }

    pub fn set(&self, t: Millis) {
        self.0.store(t, Ordering::SeqCst);
    }

    pub fn advance(&self, by: Millis) -> Millis {
        self.0.fetch_add(by, Ordering::SeqCst) + by
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Millis {
        self.0.load(Ordering::SeqCst)
    }
}

/// Parses an RFC 3339 / ISO 8601 UTC timestamp such as
/// `2026-10-29T12:34:56.789Z` or `2026-10-29T12:34:56+00:00` into epoch millis.
/// Only the subset emitted by agent transcripts is supported; anything else
/// returns `None` so callers can fall back to the observation time.
pub fn parse_rfc3339_millis(s: &str) -> Option<Millis> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut idx = 19;
    let mut frac_ms: i64 = 0;
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        let digits = &s[start..idx];
        let mut ms = String::from(digits);
        ms.truncate(3);
        while ms.len() < 3 {
            ms.push('0');
        }
        frac_ms = ms.parse().ok()?;
    }
    let offset_min: i64 = match s.get(idx..) {
        Some("Z") | Some("z") | Some("") => 0,
        Some(rest) if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            let total = h * 60 + m;
            if rest.starts_with('-') { -total } else { total }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec - offset_min * 60;
    Some(secs * 1_000 + frac_ms)
}

/// Howard Hinnant's days-from-civil algorithm (proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:01.5Z"), Some(1_500));
        assert_eq!(parse_rfc3339_millis("2026-10-29T12:00:00.123Z"), Some(1_793_275_200_123));
        assert_eq!(parse_rfc3339_millis("2026-10-29T14:00:00+02:00"), Some(1_793_275_200_000));
        assert_eq!(parse_rfc3339_millis("2026-10-29T12:00:00.123456789Z"), Some(1_793_275_200_123));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_millis(""), None);
        assert_eq!(parse_rfc3339_millis("yesterday"), None);
        assert_eq!(parse_rfc3339_millis("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_millis("2026-10-29T12:00:00 GMT"), None);
    }

    #[test]
    fn manual_clock_advances() {
        let c = ManualClock::new(10);
        assert_eq!(c.advance(5), 15);
        assert_eq!(c.now(), 15);
    }
}
