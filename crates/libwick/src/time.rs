//! RFC 3339 timestamps in UTC, without a date-time dependency.
//!
//! Provenance entries need a timestamp and nothing else needs a calendar, so
//! pulling in a full date-time crate for one format would be the tail wagging
//! the dog. The civil-from-days algorithm below is Howard Hinnant's, which is
//! the same one `chrono` and `time` use underneath.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as `2026-08-14T10:22:00Z`.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    from_unix(secs)
}

pub fn from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a civil (year, month, day). Shifts the epoch to
/// March 1st so that the leap day lands at the end of the year and the
/// month-length arithmetic becomes a single expression.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants() {
        assert_eq!(from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(from_unix(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(from_unix(1_755_167_400), "2025-08-14T10:30:00Z");
    }

    #[test]
    fn now_is_well_formed() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert!(s.starts_with("20"));
    }
}
