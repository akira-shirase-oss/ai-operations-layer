//! Time as data. The domain never reads a clock: callers pass `now` in. A [`Timestamp`]
//! is microseconds since the Unix epoch (UTC); the wire form is RFC 3339.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Microseconds since 1970-01-01T00:00:00Z.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Timestamp(i64);

/// Why a timestamp was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    #[error("not an RFC 3339 timestamp with a UTC offset")]
    Format,
    #[error("timestamp out of the representable range")]
    Range,
}

impl Timestamp {
    pub const fn from_unix_micros(micros: i64) -> Self {
        Self(micros)
    }

    pub const fn unix_micros(self) -> i64 {
        self.0
    }

    /// Parses RFC 3339 (`2026-11-16T10:00:00.123456Z` or with a numeric offset).
    pub fn parse_rfc3339(text: &str) -> Result<Self, TimeError> {
        let parsed = OffsetDateTime::parse(text, &Rfc3339).map_err(|_| TimeError::Format)?;
        let micros = parsed.unix_timestamp_nanos() / 1_000;
        i64::try_from(micros)
            .map(Self)
            .map_err(|_| TimeError::Range)
    }

    /// RFC 3339 in UTC with microsecond precision.
    pub fn to_rfc3339(self) -> String {
        let nanos = i128::from(self.0) * 1_000;
        match OffsetDateTime::from_unix_timestamp_nanos(nanos) {
            Ok(t) => t
                .format(&Rfc3339)
                .unwrap_or_else(|_| format!("@{}us", self.0)),
            Err(_) => format!("@{}us", self.0),
        }
    }

    /// `self - earlier` in microseconds (saturating).
    pub fn micros_since(self, earlier: Timestamp) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    #[must_use]
    pub fn plus_micros(self, micros: i64) -> Self {
        Self(self.0.saturating_add(micros))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse_rfc3339(&text).map_err(serde::de::Error::custom)
    }
}

pub const MICROS_PER_SECOND: i64 = 1_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offsets_and_keeps_microseconds() {
        let a = Timestamp::parse_rfc3339("2026-11-16T10:00:00.123456Z").unwrap();
        let b = Timestamp::parse_rfc3339("2026-11-16T15:30:00.123456+05:30").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_rfc3339(), "2026-11-16T10:00:00.123456Z");
    }

    #[test]
    fn refuses_non_rfc3339() {
        assert_eq!(
            Timestamp::parse_rfc3339("2026-11-16 10:00:00"),
            Err(TimeError::Format)
        );
        assert_eq!(
            Timestamp::parse_rfc3339("1731751200"),
            Err(TimeError::Format)
        );
    }
}
