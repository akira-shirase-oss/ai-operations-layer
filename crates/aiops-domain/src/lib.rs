//! The AI Operations Layer's pure model (no I/O, enforced by
//! `scripts/check-architecture.sh`).
//!
//! - [`wire`]: schema v1 as it travels (`docs/INGEST_CONTRACT.md`).
//! - [`validate`]: wire → [`event::Event`], with a named [`validate::Refusal`] for every
//!   way an event can be wrong, plus the labeled policy changes ([`policy`]).
//! - [`timeline`]: one operation's honest timeline — declared status, exact and unknown
//!   loss, both clocks, clock-skew flags.
//! - [`summary`], [`names`], [`ratelimit`], [`context`]: the pure parts of the ingest path.
//!
//! **Compatibility rule.** A collector accepts exactly the schema versions it lists in
//! [`wire::SCHEMA_VERSION`] (v1 today) and refuses unknown fields, so any new field is a
//! version bump. A new version is added beside the old one, never in place of it, and
//! the SDK always writes the lowest version that carries what it sends.

pub mod context;
pub mod correlate;
pub mod event;
pub mod ids;
pub mod limits;
pub mod names;
pub mod policy;
pub mod ratelimit;
pub mod summary;
pub mod time;
pub mod timeline;
pub mod validate;
pub mod wire;

pub use event::{Event, EventBody, FailureClass, GapReason, Outcome, StepKind, UncertainReason};
pub use ids::{EventId, InstanceId, OperationId, StepId, TraceId};
pub use limits::Limits;
pub use time::Timestamp;
pub use validate::{Mode, PolicyMarks, Refusal, RefusalCode, Validated, Validator};
