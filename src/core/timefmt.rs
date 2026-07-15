//! Timestamps in the Python `datetime.isoformat()` dialect the on-disk format
//! uses: naive local time, `T` separator, microseconds omitted when zero
//! (e.g. `2026-07-01T10:13:22.036493` or `2026-06-01T10:00:00`).

use chrono::{NaiveDateTime, Timelike};

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
    fn quotes_yaml_timestamp_fields_without_touching_block_content() {
        let yaml = "created_at: 2026-07-01T10:13:22\n  updated_at: 2026-07-01T10:13:23\n    created_at: keep body text\nstatus: open\n";

        let quoted = quote_yaml_timestamp_fields(yaml, &["created_at", "updated_at"]);

        assert!(quoted.contains("created_at: '2026-07-01T10:13:22'"));
        assert!(quoted.contains("  updated_at: '2026-07-01T10:13:23'"));
        assert!(quoted.contains("    created_at: keep body text"));
    }
}
