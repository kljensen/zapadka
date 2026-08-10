//! A small duration type for timeouts declared in `zapadka.toml`.
//!
//! Durations are written the way PostgreSQL writes them (`5s`, `500ms`,
//! `2min`), because the same values are handed to `lock_timeout` and
//! `statement_timeout`. Zapadka also needs them as real durations to decide how
//! long to wait for its advisory lock, so it parses rather than passes through.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A non-negative duration written in PostgreSQL's timeout spelling.
///
/// Zero means "no timeout", matching PostgreSQL's own convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, JsonSchema)]
#[schemars(
    with = "String",
    description = "A duration such as \"5s\", \"500ms\", or \"0\"."
)]
pub struct Timeout {
    milliseconds: u64,
}

impl Timeout {
    /// A timeout of `milliseconds`.
    pub const fn from_millis(milliseconds: u64) -> Self {
        Self { milliseconds }
    }

    /// A timeout of `seconds`.
    pub const fn from_secs(seconds: u64) -> Self {
        Self {
            milliseconds: seconds * 1000,
        }
    }

    /// Disables the timeout.
    pub const ZERO: Self = Self { milliseconds: 0 };

    /// Whether this timeout means "wait indefinitely" / "no limit".
    pub const fn is_zero(self) -> bool {
        self.milliseconds == 0
    }

    /// The duration in milliseconds.
    pub const fn as_millis(self) -> u64 {
        self.milliseconds
    }

    /// The duration as a [`std::time::Duration`].
    pub const fn as_std(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.milliseconds)
    }

    /// The value to pass to PostgreSQL's `lock_timeout` or `statement_timeout`,
    /// which are measured in milliseconds.
    pub fn as_postgres_setting(self) -> String {
        self.milliseconds.to_string()
    }

    /// Parses `5s`, `500ms`, `2min`, `1h`, or a bare integer meaning
    /// milliseconds.
    pub fn parse(text: &str) -> Result<Self, TimeoutParseError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(TimeoutParseError::new(text));
        }
        let digits = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(digits);
        let number: u64 = number.parse().map_err(|_| TimeoutParseError::new(text))?;
        // A bare number is milliseconds, matching PostgreSQL's own reading of
        // `SET lock_timeout = 5000`.
        let multiplier = match unit.trim() {
            "" | "ms" => 1,
            "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
            "min" | "mins" | "minute" | "minutes" => 60_000,
            "h" | "hr" | "hour" | "hours" => 3_600_000,
            _ => return Err(TimeoutParseError::new(text)),
        };
        number
            .checked_mul(multiplier)
            .map(Self::from_millis)
            .ok_or_else(|| TimeoutParseError::new(text))
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.milliseconds;
        if ms == 0 {
            f.write_str("0")
        } else if ms.is_multiple_of(3_600_000) {
            write!(f, "{}h", ms / 3_600_000)
        } else if ms.is_multiple_of(60_000) {
            write!(f, "{}min", ms / 60_000)
        } else if ms.is_multiple_of(1_000) {
            write!(f, "{}s", ms / 1_000)
        } else {
            write!(f, "{ms}ms")
        }
    }
}

/// A timeout string Zapadka could not understand.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "invalid duration {value:?}; write a number with a unit such as \"5s\", \"500ms\", or \"0\""
)]
pub struct TimeoutParseError {
    value: String,
}

impl TimeoutParseError {
    fn new(value: &str) -> Self {
        Self {
            value: value.to_owned(),
        }
    }
}

impl Serialize for Timeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn parses_every_supported_unit() {
        assert_eq!(Timeout::parse("0").unwrap(), Timeout::ZERO);
        assert_eq!(Timeout::parse("250ms").unwrap().as_millis(), 250);
        assert_eq!(Timeout::parse("5s").unwrap().as_millis(), 5_000);
        assert_eq!(Timeout::parse("2min").unwrap().as_millis(), 120_000);
        assert_eq!(Timeout::parse("1h").unwrap().as_millis(), 3_600_000);
    }

    #[test]
    fn a_bare_number_is_milliseconds_like_postgresql() {
        assert_eq!(Timeout::parse("5000").unwrap().as_millis(), 5_000);
    }

    #[test]
    fn rejects_values_it_cannot_interpret_unambiguously() {
        // Fractions, signs, and unknown units are rejected rather than rounded
        // or reinterpreted: a misread timeout is a silent production hazard.
        for bad in ["", "  ", "-5s", "s", "5x", "1.5s", "many", "5s5"] {
            assert!(Timeout::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn accepts_the_spacing_and_spellings_people_actually_write() {
        for text in ["5 seconds", " 5s ", "5 s", "2 minutes"] {
            assert!(Timeout::parse(text).is_ok(), "{text:?} should be accepted");
        }
        assert_eq!(Timeout::parse("5 seconds").unwrap(), Timeout::from_secs(5));
    }

    #[test]
    fn round_trips_through_display_and_parse() {
        for text in ["0", "250ms", "5s", "2min", "1h"] {
            let parsed = Timeout::parse(text).unwrap();
            assert_eq!(parsed.to_string(), text);
            assert_eq!(Timeout::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn postgres_settings_are_expressed_in_milliseconds() {
        assert_eq!(Timeout::parse("5s").unwrap().as_postgres_setting(), "5000");
        assert_eq!(Timeout::ZERO.as_postgres_setting(), "0");
    }

    #[test]
    fn overflow_is_rejected_rather_than_wrapping() {
        assert!(Timeout::parse("99999999999999999999h").is_err());
        assert!(Timeout::parse(&format!("{}h", u64::MAX)).is_err());
    }
}
