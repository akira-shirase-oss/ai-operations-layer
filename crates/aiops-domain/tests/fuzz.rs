//! Never-panics fuzz targets for every parser/decoder of outside input in the
//! domain crate (standing rule, 2026-11-20). Each target runs its SEEDS —
//! the corpus of shapes that hurt parsers before — then arbitrary bytes under
//! proptest (bounded by `PROPTEST_CASES`; CI smokes at 512). A panic is a
//! red-first bug; a regression seed is kept in `proptest-regressions/fuzz.txt`.

use aiops_domain::context::TraceParent;
use aiops_domain::control::{ControlKind, ControlStatus, Resolution};
use aiops_domain::ids::{EventId, InstanceId, OperationId, StepId, TraceId};
use aiops_domain::limits::Limits;
use aiops_domain::policy::{Policy, redact};
use aiops_domain::validate::{Mode, Validator};
use aiops_domain::wire::RawBatch;
use aiops_domain::{FailureClass, GapReason, StepKind, Timestamp, UncertainReason};
use proptest::prelude::*;

/// Shapes that hurt parsers: truncation, escapes, unicode edges, huge runs,
/// and the N1–N7 findings' actual payloads.
const SEEDS: &[&str] = &[
    "",
    "\0",
    "\"",
    "\\",
    "{",
    "{}",
    "{}\"",
    "token: {\"\\",
    "[]",
    "null",
    "\u{202e}",
    "\u{feff}",
    "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01",
    "2026-11-20T00:00:00Z",
    "2026-11-20T00:00:00.999999999999+99:99",
    "https://a.io/token?client_secret=Zq81aa",
    "postgres://aaa:%0C$Aa@aaa.aa/db",
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
    "mistralai/Mistral-7B-Instruct-v0.3",
    "auth:login",
    "ffffffffffffffffffffffffffffffffffffffff",
];

fn seed_corpus() -> Vec<String> {
    let mut v: Vec<String> = SEEDS.iter().map(|s| (*s).to_owned()).collect();
    v.push("a".repeat(4096));
    v.push("{\"events\":[".repeat(64));
    v
}

proptest! {
    /// The ingest batch decoder: `serde_json` over `deny_unknown_fields` with
    /// borrowed `RawValue` events — the first bytes an attacker controls.
    #[test]
    fn batch_decode_never_panics(s in any::<Vec<u8>>()) {
        if let Ok(text) = std::str::from_utf8(&s) {
            let _ = serde_json::from_str::<RawBatch<'_>>(text);
        }
        // Invalid UTF-8 is input too — decode it losslessly and still refuse.
        let _ = serde_json::from_slice::<RawBatch<'_>>(&s);
    }

    /// The per-event path: size gate, JSON decode, id and field validation,
    /// policy marks — `validate_raw` is the deepest outside-input decoder.
    #[test]
    fn event_decode_and_validate_never_panics(s in "\\PC{0,1024}") {
        let validator = Validator {
            limits: &Limits::default(),
            policy: &Policy::default(),
            mode: Mode::Stored,
        };
        let _ = validator.validate_raw(&s);
    }

    /// RFC 3339 is the timestamp contract on every event.
    #[test]
    fn rfc3339_parse_never_panics(s in "\\PC{0,128}") {
        let _ = Timestamp::parse_rfc3339(&s);
    }

    /// The hex id family: exact width, lowercase, non-zero.
    #[test]
    fn id_parsers_never_panic(s in "\\PC{0,64}") {
        let _ = OperationId::parse(&s);
        let _ = TraceId::parse(&s);
        let _ = EventId::parse(&s);
        let _ = InstanceId::parse(&s);
        let _ = StepId::parse(&s);
    }

    /// What parses must round-trip through Display.
    #[test]
    fn ids_round_trip(bytes in any::<[u8; 16]>()) {
        if let Some(id) = OperationId::from_bytes(bytes) {
            assert_eq!(OperationId::parse(&id.to_string()).unwrap(), id);
        }
        if let Some(id) = EventId::from_bytes(bytes) {
            assert_eq!(EventId::parse(&id.to_string()).unwrap(), id);
        }
    }

    /// `traceparent`: the W3C header an SDK takes from its caller.
    #[test]
    fn traceparent_parse_never_panics(s in "\\PC{0,128}") {
        let _ = TraceParent::parse(&s);
    }

    /// Every enum that arrives as wire text: control + event vocabularies.
    #[test]
    fn enum_parsers_never_panic(s in "\\PC{0,64}") {
        let _ = ControlKind::parse(&s);
        let _ = Resolution::parse(&s);
        let _ = ControlStatus::parse(&s);
        let _ = StepKind::parse(&s);
        let _ = FailureClass::parse(&s);
        let _ = UncertainReason::parse(&s);
        let _ = GapReason::parse(&s);
    }

    /// Privacy redaction over free text — N2's panic lived here.
    #[test]
    fn redact_never_panics(s in "\\PC{0,1024}") {
        let _ = redact(&s);
    }
}

/// The seed corpus runs deterministically on every invocation — a regression
/// found in the wild becomes a literal here and in `proptest-regressions/`.
#[test]
fn seeds_never_panic() {
    let validator = Validator {
        limits: &Limits::default(),
        policy: &Policy::default(),
        mode: Mode::Stored,
    };
    for s in seed_corpus() {
        let _ = serde_json::from_str::<RawBatch<'_>>(&s);
        let _ = validator.validate_raw(&s);
        let _ = Timestamp::parse_rfc3339(&s);
        let _ = OperationId::parse(&s);
        let _ = TraceParent::parse(&s);
        let _ = ControlKind::parse(&s);
        let _ = StepKind::parse(&s);
        let _ = redact(&s);
    }
}
