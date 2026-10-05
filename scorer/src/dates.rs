//! Just enough calendar arithmetic for ISO-8601 UTC strings, without a date
//! library (the Worker is wasm and should stay small). Times in this project
//! are `YYYY-MM-DDTHH:MM:SS[.sss]Z`, so they compare correctly as strings once
//! they share a form; these helpers produce that form.

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Milliseconds since the epoch for `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS[.sss]]Z`.
pub fn parse_ms(iso: &str) -> Option<i64> {
    let b = iso.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = iso.get(r)?;
        if s.bytes().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    if b.len() > 10 {
        if b[10] != b'T' || b.len() < 16 || b[13] != b':' {
            return None;
        }
        ms += (n(11..13)? * 3600 + n(14..16)? * 60) * 1000;
        if b.len() >= 19 && b[16] == b':' {
            ms += n(17..19)? * 1000;
            if b.len() >= 23 && b[19] == b'.' {
                ms += n(20..23)?;
            }
        }
    }
    Some(ms)
}

/// `YYYY-MM-DDTHH:MM:SS.sssZ` for milliseconds since the epoch.
pub fn format_ms(total: i64) -> String {
    let (secs, ms) = (total.div_euclid(1000), total.rem_euclid(1000));
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// `iso` minus `hours`, in millisecond ISO form. `None` when `iso` is not a date.
pub fn iso_minus_hours(iso: &str, hours: i64) -> Option<String> {
    Some(format_ms(parse_ms(iso)? - hours * 3_600_000))
}

/// The calendar date (`YYYY-MM-DD`) `days` before `iso`.
pub fn date_minus_days(iso: &str, days: i64) -> Option<String> {
    Some(format_ms(parse_ms(iso)? - days * 86_400_000)[..10].to_string())
}

/// True for a well-formed ISO date or date-time.
pub fn is_iso(s: &str) -> bool {
    parse_ms(s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minus_hours_crosses_days_months_and_leap_years() {
        assert_eq!(iso_minus_hours("2026-09-30T12:00:00.000Z", 24).as_deref(), Some("2026-09-29T12:00:00.000Z"));
        assert_eq!(iso_minus_hours("2026-03-01T05:30:00.250Z", 24).as_deref(), Some("2026-02-28T05:30:00.250Z"));
        assert_eq!(iso_minus_hours("2028-03-01T00:00:00Z", 24).as_deref(), Some("2028-02-29T00:00:00.000Z"));
        assert_eq!(iso_minus_hours("2027-01-01T01:00:00.000Z", 2).as_deref(), Some("2026-12-31T23:00:00.000Z"));
        assert!(iso_minus_hours("yesterday", 24).is_none());
    }

    #[test]
    fn dates_and_partial_times_parse() {
        assert_eq!(date_minus_days("2026-10-05T11:00:00Z", 365).as_deref(), Some("2025-10-05"));
        assert_eq!(date_minus_days("2026-10-05", 5).as_deref(), Some("2026-09-30"));
        assert!(is_iso("2026-10-05T11:00Z"));
        assert!(!is_iso("2026-13-05"));
        assert!(!is_iso("2026-1x-05"));
        assert_eq!(parse_ms("1970-01-01T00:00:01.500Z"), Some(1500));
    }
}
