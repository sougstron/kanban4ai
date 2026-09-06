//! Timestamps in the Python `datetime.isoformat()` dialect the on-disk format
//! uses: naive local time, `T` separator, microseconds omitted when zero
//! (e.g. `2026-07-01T10:13:22.036493` or `2026-06-01T10:00:00`).

use chrono::{NaiveDateTime, NaiveTime, Timelike};

const FMT: &str = "%Y-%m-%dT%H:%M:%S%.f";
const FMT_NO_FRAC: &str = "%Y-%m-%dT%H:%M:%S";

pub fn format(dt: &NaiveDateTime) -> String {
    dt.format(FMT).to_string()
}

pub fn parse(s: &str) -> Result<NaiveDateTime, chrono::ParseError> {
    NaiveDateTime::parse_from_str(s, FMT).or_else(|_| NaiveDateTime::parse_from_str(s, FMT_NO_FRAC))
}

/// Current local time truncated to microseconds, matching Python `datetime.now()`
/// resolution so a value survives a format/parse round-trip unchanged.
pub fn now() -> NaiveDateTime {
    let now = chrono::Local::now().naive_local();
    now.with_nanosecond(now.nanosecond() / 1000 * 1000)
        .unwrap_or(now)
}

/// Parse an `HH:MM` time-of-day (the planned-launch field). Seconds are
/// deliberately not part of the input: the timer compares at minute
/// resolution.
pub fn parse_hhmm(value: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(value.trim(), "%H:%M").ok()
}

/// Next local occurrence of an `HH:MM` time-of-day: today at that time, or
/// tomorrow once that moment has already passed (an exact match counts as
/// passed — a launch typed at its own second is re-armed for tomorrow).
pub fn next_launch_at(time: NaiveTime) -> NaiveDateTime {
    let now = now();
    let today = now.date().and_time(time);
    if today > now {
        today
    } else {
        today + chrono::Duration::days(1)
    }
}

/// Quote timestamp-looking YAML scalars so Python's YAML loader keeps them as
/// strings for the legacy CLI's `datetime.fromisoformat(...)` calls.
pub fn quote_yaml_timestamp_fields(yaml: &str, keys: &[&str]) -> String {
    yaml.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let indent_len = line.len() - trimmed.len();
            if indent_len <= 2 {
                for key in keys {
                    let prefix = format!("{key}: ");
                    if let Some(value) = trimmed.strip_prefix(&prefix)
                        && !value.starts_with('\'')
                        && !value.starts_with('"')
                        && value != "null"
                    {
                        return format!(
                            "{}{}'{}'",
                            &line[..indent_len],
                            prefix,
                            value.replace('\'', "''")
                        );
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

pub mod serde_naive {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format(dt))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveDateTime, D::Error> {
        let raw = String::deserialize(d)?;
        super::parse(&raw).map_err(serde::de::Error::custom)
    }
}

pub mod serde_naive_opt {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &Option<NaiveDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match dt {
            Some(dt) => s.serialize_str(&super::format(dt)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDateTime>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        match raw {
            None => Ok(None),
            Some(raw) if raw.is_empty() => Ok(None),
            Some(raw) => super::parse(&raw)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_isoformat_with_microseconds() {
        let dt = parse("2026-07-01T10:13:22.036493").unwrap();
        assert_eq!(format(&dt), "2026-07-01T10:13:22.036493");
    }

    #[test]
    fn parses_python_isoformat_without_fraction() {
        let dt = parse("2026-06-01T10:00:00").unwrap();
        assert_eq!(format(&dt), "2026-06-01T10:00:00");
    }

    #[test]
    fn now_round_trips() {
        let dt = now();
        assert_eq!(parse(&format(&dt)).unwrap(), dt);
    }

    #[test]
    fn parses_hhmm_and_rejects_garbage() {
        assert_eq!(
            parse_hhmm("09:05"),
            Some(NaiveTime::from_hms_opt(9, 5, 0).unwrap())
        );
        assert_eq!(
            parse_hhmm(" 23:59 "),
            Some(NaiveTime::from_hms_opt(23, 59, 0).unwrap())
        );
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("24:00"), None);
        // chrono's %H:%M is lenient about padding: "9:5" means 09:05.
        assert_eq!(
            parse_hhmm("9:5"),
            Some(NaiveTime::from_hms_opt(9, 5, 0).unwrap())
        );
        assert_eq!(parse_hhmm("10:60"), None);
        assert_eq!(parse_hhmm("10:10:10"), None);
        assert_eq!(parse_hhmm("10am"), None);
    }

    #[test]
    fn next_launch_at_rolls_to_tomorrow_once_the_moment_has_passed() {
        let now = now();
        // A time two minutes ahead (truncated to the minute) still fires
        // today — or tomorrow exactly when the truncation crossed midnight,
        // which the roll does for us.
        let ahead_dt = (now + chrono::Duration::minutes(2)).with_second(0).unwrap();
        let ahead = ahead_dt.time();
        let at = next_launch_at(ahead);
        assert!(at > now);
        assert_eq!(at.time(), ahead);
        assert!(at.date() - now.date() <= chrono::Duration::days(1));

        // A time already past today rolls to tomorrow.
        let past_hour = if now.hour() > 0 { now.hour() - 1 } else { 23 };
        let past = NaiveTime::from_hms_opt(past_hour, now.minute(), 0).unwrap();
        let rolled = next_launch_at(past);
        assert_eq!(rolled.date(), now.date() + chrono::Duration::days(1));
        assert_eq!(rolled.time(), past);

        // The exact current minute counts as passed: re-armed for tomorrow.
        let exact = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).unwrap();
        let re_armed = next_launch_at(exact);
        assert_eq!(re_armed.date(), now.date() + chrono::Duration::days(1));
    }

    #[test]
    fn quotes_yaml_timestamp_fields_without_touching_block_content() {
        let yaml = "created_at: 2026-07-01T10:13:22\n  updated_at: 2026-07-01T10:13:23\n    created_at: keep body text\nstatus: open\n";

        let quoted = quote_yaml_timestamp_fields(yaml, &["created_at", "updated_at"]);

        assert!(quoted.contains("created_at: '2026-07-01T10:13:22'"));
        assert!(quoted.contains("  updated_at: '2026-07-01T10:13:23'"));
        assert!(quoted.contains("    created_at: keep body text"));
    }
}
