//! Identifiers. All are lowercase hex on the wire, OpenTelemetry-compatible in width:
//! operation, trace, event and instance IDs are 128-bit (32 hex), step IDs are 64-bit
//! (16 hex, the width of an OTel span ID). The all-zero value is invalid, as in W3C
//! Trace Context.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a hex identifier was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("expected {expected} lowercase hex characters")]
    Length { expected: usize },
    #[error("expected lowercase hex characters only")]
    NotHex,
    #[error("the all-zero identifier is invalid")]
    Zero,
}

fn parse_hex<const N: usize>(text: &str) -> Result<[u8; N], IdError> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return Err(IdError::Length { expected: N * 2 });
    }
    let mut out = [0u8; N];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    if out.iter().all(|b| *b == 0) {
        return Err(IdError::Zero);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, IdError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(IdError::NotHex),
    }
}

fn write_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 32];
    for (i, b) in bytes.iter().enumerate() {
        buf[i * 2] = DIGITS[usize::from(b >> 4)];
        buf[i * 2 + 1] = DIGITS[usize::from(b & 0x0f)];
    }
    let text = std::str::from_utf8(&buf[..bytes.len() * 2]).map_err(|_| fmt::Error)?;
    f.write_str(text)
}

macro_rules! hex_id {
    ($(#[$meta:meta])* $name:ident, $bytes:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; $bytes]);

        impl $name {
            /// Parses the wire form (lowercase hex, exact width, not all zero).
            pub fn parse(text: &str) -> Result<Self, IdError> {
                parse_hex::<$bytes>(text).map(Self)
            }

            /// Builds an identifier from raw bytes; `None` for the invalid all-zero value.
            pub fn from_bytes(bytes: [u8; $bytes]) -> Option<Self> {
                if bytes.iter().all(|b| *b == 0) { None } else { Some(Self(bytes)) }
            }

            pub fn as_bytes(&self) -> &[u8; $bytes] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_id!(
    /// One application operation (request → steps → outcome). The SDK uses it as the
    /// W3C trace ID, so the operation correlates with OpenTelemetry traces.
    OperationId,
    16
);
hex_id!(
    /// A W3C trace ID when the operation belongs to a trace that started elsewhere.
    TraceId,
    16
);
hex_id!(
    /// One event, unique per project and stable across re-delivery (the dedup key).
    EventId,
    16
);
hex_id!(
    /// One SDK process (a source instance); random per process start.
    InstanceId,
    16
);
hex_id!(
    /// One logical step (span-ID width). Retries of the step keep the ID and raise `attempt`.
    StepId,
    8
);

impl OperationId {
    pub fn as_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }
}

impl EventId {
    pub fn as_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }
}

impl InstanceId {
    pub fn as_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }
}

impl StepId {
    /// The step ID as a signed 64-bit integer (how the store keeps it).
    pub fn as_i64(&self) -> i64 {
        i64::from_be_bytes(self.0)
    }

    pub fn from_i64(value: i64) -> Option<Self> {
        Self::from_bytes(value.to_be_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_lowercase_hex() {
        let id = OperationId::parse("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(id.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
        let step = StepId::parse("00f067aa0ba902b7").unwrap();
        assert_eq!(StepId::from_i64(step.as_i64()), Some(step));
    }

    #[test]
    fn refuses_wrong_width_uppercase_and_zero() {
        assert_eq!(StepId::parse("abc"), Err(IdError::Length { expected: 16 }));
        assert_eq!(
            OperationId::parse("4BF92F3577B34DA6A3CE929D0E0E4736"),
            Err(IdError::NotHex)
        );
        assert_eq!(
            EventId::parse("00000000000000000000000000000000"),
            Err(IdError::Zero)
        );
        assert!(OperationId::from_bytes([0; 16]).is_none());
    }
}
