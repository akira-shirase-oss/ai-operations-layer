//! W3C Trace Context (`traceparent`) — the OpenTelemetry-compatible propagation format
//! (D-003). Version `00` only; anything else is refused rather than guessed.

use crate::ids::{OperationId, StepId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceParent {
    /// The trace ID; the SDK uses it as the operation ID.
    pub trace_id: OperationId,
    /// The parent span (step) ID.
    pub parent_id: StepId,
    pub sampled: bool,
}

impl TraceParent {
    pub fn parse(header: &str) -> Option<Self> {
        let mut parts = header.trim().split('-');
        let (version, trace, parent, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || version != "00" || flags.len() != 2 {
            return None;
        }
        let flags = u8::from_str_radix(flags, 16).ok()?;
        Some(Self {
            trace_id: OperationId::parse(trace).ok()?,
            parent_id: StepId::parse(parent).ok()?,
            sampled: flags & 1 == 1,
        })
    }

    pub fn header(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.parent_id,
            if self.sampled { "01" } else { "00" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_the_spec_example() {
        let h = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tp = TraceParent::parse(h).unwrap();
        assert!(tp.sampled);
        assert_eq!(tp.header(), h);
    }

    #[test]
    fn refuses_other_versions_zero_ids_and_extra_fields() {
        assert!(
            TraceParent::parse("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            TraceParent::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .is_none()
        );
    }
}
