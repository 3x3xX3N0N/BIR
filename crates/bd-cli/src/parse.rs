//! Value parsers shared by clap and by config loading.
//!
//! These return `Result<_, String>` because that is what clap's `value_parser`
//! wants; the error text lands directly in the usage message.

use bd_core::DependencyType;
use chrono::{DateTime, Duration, NaiveDate, Utc};

/// A dependency named on the command line: `bd-x` or `bd-x:blocks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepSpec {
    pub id: String,
    pub dep_type: DependencyType,
}

pub fn dep_spec(s: &str) -> Result<DepSpec, String> {
    let (id, ty) = match s.split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    if id.is_empty() {
        return Err("expected <id> or <id>:<type>".to_string());
    }
    let dep_type = match ty {
        Some(t) => t.parse::<DependencyType>().map_err(|e| e.to_string())?,
        None => DependencyType::Blocks,
    };
    Ok(DepSpec {
        id: id.to_string(),
        dep_type,
    })
}

/// `90s`, `30m`, `2h`, `3d`, `1w`. A bare number is minutes.
pub fn duration(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty duration".to_string());
    }
    let (num, unit) = match t.char_indices().find(|(_, c)| c.is_alphabetic()) {
        Some((i, _)) => (&t[..i], &t[i..]),
        None => (t, "m"),
    };
    let n: i64 = num
        .parse()
        .map_err(|_| format!("invalid duration: {s} (try 30m, 2h, 3d)"))?;
    if n < 0 {
        return Err(format!("duration must not be negative: {s}"));
    }
    let d = match unit.to_lowercase().as_str() {
        "s" | "sec" | "secs" => Duration::seconds(n),
        "m" | "min" | "mins" => Duration::minutes(n),
        "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(n),
        "d" | "day" | "days" => Duration::days(n),
        "w" | "week" | "weeks" => Duration::weeks(n),
        other => return Err(format!("unknown duration unit: {other} (try s, m, h, d, w)")),
    };
    Ok(d)
}

/// An instant: RFC 3339, a bare `YYYY-MM-DD`, or an offset from now (`2d`).
pub fn when(s: &str) -> Result<DateTime<Utc>, String> {
    let t = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        // Midnight UTC. A date without a time is a date, and pretending
        // otherwise would silently shift deadlines across timezones.
        return Ok(d
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc());
    }
    duration(t)
        .map(|d| Utc::now() + d)
        .map_err(|_| format!("invalid time: {s} (try 2026-01-31, an RFC 3339 timestamp, or 3d)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_spec_defaults_to_blocks() {
        assert_eq!(
            dep_spec("bd-1").unwrap(),
            DepSpec {
                id: "bd-1".into(),
                dep_type: DependencyType::Blocks
            }
        );
        assert_eq!(
            dep_spec("bd-1:parent-child").unwrap().dep_type,
            DependencyType::ParentChild
        );
        assert!(dep_spec(":blocks").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(duration("2h").unwrap(), Duration::hours(2));
        assert_eq!(duration("90").unwrap(), Duration::minutes(90));
        assert_eq!(duration("1w").unwrap(), Duration::weeks(1));
        assert!(duration("tomorrow").is_err());
        assert!(duration("-1h").is_err());
    }

    #[test]
    fn when_accepts_dates_and_offsets() {
        let d = when("2026-01-31").unwrap();
        assert_eq!(d.date_naive().to_string(), "2026-01-31");
        assert!(when("3d").unwrap() > Utc::now());
        assert!(when("2026-13-99").is_err());
    }
}
