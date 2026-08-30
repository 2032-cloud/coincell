//! Converting between the server's `"YYYY-MM-DD HH:MM:SS"` UTC timestamps and
//! Unix seconds. Howard Hinnant's days-from-civil / civil-from-days; no calendar
//! crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Parse `"YYYY-MM-DD HH:MM:SS"` (a `T` separator and a trailing `Z` are
/// tolerated) to Unix seconds.
pub fn parse_utc(ts: &str) -> Option<i64> {
    let (date, time) = ts.split_once([' ', 'T'])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.trim().parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").trim_end_matches('Z').parse().ok()?;

    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Unix seconds → `"YYYY-MM-DD HH:MM:SS"` UTC.
pub fn format_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86400);
    let secs_of_day = unix_secs.rem_euclid(86400);
    let (hour, min, sec) = (secs_of_day / 3600, secs_of_day / 60 % 60, secs_of_day % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

pub fn now_epoch() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A coarse "5 minutes ago" / "7 months ago" rendering of a server timestamp,
/// relative to now. Falls back to the raw string if it won't parse. A timestamp
/// in the future (clock skew) reads as "just now".
pub fn humanize_since(ts: &str) -> String {
    let Some(then) = parse_utc(ts) else { return ts.to_owned() };
    humanize_delta(now_epoch() - then)
}

/// Bucket a seconds delta into a human phrase. Pulled out for testing. Unit
/// crossovers follow the usual "45 minutes rounds up to an hour" convention; a
/// month is treated as 30 days, a year as 365.
fn humanize_delta(secs: i64) -> String {
    let s = secs.max(0);
    if s < 45 {
        return "just now".to_owned();
    }
    if s < 45 * 60 {
        return plural((s + 30) / 60, "minute");
    }
    if s < 22 * 3600 {
        return plural((s + 1800) / 3600, "hour");
    }
    if s < 26 * 86_400 {
        return plural((s + 43_200) / 86_400, "day");
    }
    if s < 11 * 2_592_000 {
        return plural((s + 1_296_000) / 2_592_000, "month");
    }
    plural((s + 15_768_000) / 31_536_000, "year")
}

fn plural(n: i64, unit: &str) -> String {
    let n = n.max(1);
    if n == 1 { format!("1 {unit} ago") } else { format!("{n} {unit}s ago") }
}

/// `"YYYY-MM-DD HH:MM:SS"` for right now, UTC. Best-effort stand-in for a
/// server timestamp we don't have yet (e.g. straight after an upload).
pub fn now_utc_string() -> String {
    format_utc(now_epoch())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_values() {
        assert_eq!(parse_utc("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_utc("1970-01-01 00:00:01"), Some(1));
        assert_eq!(parse_utc("1970-01-02 00:00:00"), Some(86_400));
        assert_eq!(parse_utc("garbage"), None);
        // one day apart across a real leap day
        let feb29 = parse_utc("2000-02-29 00:00:00").unwrap();
        let mar01 = parse_utc("2000-03-01 00:00:00").unwrap();
        assert_eq!(mar01 - feb29, 86_400);
        // `T` + `Z` tolerated
        assert_eq!(parse_utc("2026-08-28T12:00:00Z"), parse_utc("2026-08-28 12:00:00"));
    }

    #[test]
    fn humanize_buckets() {
        assert_eq!(humanize_delta(0), "just now");
        assert_eq!(humanize_delta(40), "just now");
        assert_eq!(humanize_delta(60), "1 minute ago");
        assert_eq!(humanize_delta(90), "2 minutes ago");
        assert_eq!(humanize_delta(5 * 60), "5 minutes ago");
        assert_eq!(humanize_delta(60 * 60), "1 hour ago");
        assert_eq!(humanize_delta(3 * 3600), "3 hours ago");
        assert_eq!(humanize_delta(26 * 3600), "1 day ago");
        assert_eq!(humanize_delta(3 * 86_400), "3 days ago");
        assert_eq!(humanize_delta(40 * 86_400), "1 month ago");
        assert_eq!(humanize_delta(210 * 86_400), "7 months ago");
        assert_eq!(humanize_delta(400 * 86_400), "1 year ago");
        assert_eq!(humanize_delta(800 * 86_400), "2 years ago");
        // future timestamps (clock skew) clamp to "just now"
        assert_eq!(humanize_delta(-500), "just now");
    }

    #[test]
    fn humanize_since_falls_back_on_junk() {
        assert_eq!(humanize_since("not a date"), "not a date");
    }

    #[test]
    fn round_trips() {
        for s in ["1970-01-01 00:00:00", "2000-02-29 12:34:56", "2026-08-28 22:16:50", "2038-01-19 03:14:07"] {
            assert_eq!(format_utc(parse_utc(s).unwrap()), s, "round trip {s}");
        }
    }
}
