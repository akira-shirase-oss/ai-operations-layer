//! Safe control (invariant 4, D-007): the control-record state machine, pure. A record
//! is created `requested` and every change is a named transition this module validates,
//! so the store and the server cannot move a record sideways — a denial is a first-class
//! terminal state, and a retry's submit is observed against the authority, never assumed.
//!
//! No I/O here: the states, names and rules only.

use serde::{Deserialize, Serialize};

/// What one control action does. `Cancel` arrives with a host that owns it; `step_id`
/// scoping is reserved — v1 acts on a whole operation only (D-032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// Read the effect's authority and store its answer; changes nothing outside.
    Reconcile,
    /// Submit replacement work under the operation's business key after the checks.
    Retry,
}

impl ControlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Retry => "retry",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reconcile" => Some(Self::Reconcile),
            "retry" => Some(Self::Retry),
            _ => None,
        }
    }
}

/// A reconcile's stored answer about the external effect — never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The authority's record shows the effect landed.
    EffectPresent,
    /// The authority's record shows no effect.
    EffectAbsent,
    /// The authority could not answer (down, unreadable, ambiguous). `uncertain` stays
    /// `uncertain` (invariant 2): the record says so instead of resolving.
    StillUncertain,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EffectPresent => "effect_present",
            Self::EffectAbsent => "effect_absent",
            Self::StillUncertain => "still_uncertain",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "effect_present" => Some(Self::EffectPresent),
            "effect_absent" => Some(Self::EffectAbsent),
            "still_uncertain" => Some(Self::StillUncertain),
            _ => None,
        }
    }
}

/// Where a control record can be. `Denied`, `Reconciled`, `Confirmed`,
/// `SubmitFailed` and `ObservedUnknown` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlStatus {
    /// Committed; nothing decided yet.
    Requested,
    /// (reconcile) the authority read is in flight.
    Reconciling,
    /// (reconcile, terminal) the authority's snapshot is stored with a [`Resolution`].
    Reconciled,
    /// (retry) the authorization decision committed; the executor may submit.
    Authorized,
    /// (retry) the submit call was sent and accepted; its outcome is read next.
    Submitted,
    /// (retry, terminal) the authority afterwards shows the intended effect.
    Confirmed,
    /// (retry, terminal) the submit failed before a definitive answer, its budget spent.
    SubmitFailed,
    /// (retry, terminal) the post-submit authority read could not prove the outcome
    /// within its bound — honest, never upgraded without evidence.
    ObservedUnknown,
    /// (terminal) a decision: denied with the reason recorded, never an absence.
    Denied,
}

impl ControlStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Reconciling => "reconciling",
            Self::Reconciled => "reconciled",
            Self::Authorized => "authorized",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
            Self::SubmitFailed => "submit_failed",
            Self::ObservedUnknown => "observed_unknown",
            Self::Denied => "denied",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "requested" => Self::Requested,
            "reconciling" => Self::Reconciling,
            "reconciled" => Self::Reconciled,
            "authorized" => Self::Authorized,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "submit_failed" => Self::SubmitFailed,
            "observed_unknown" => Self::ObservedUnknown,
            "denied" => Self::Denied,
            _ => return None,
        })
    }

    /// No further transition is possible.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Reconciled
                | Self::Confirmed
                | Self::SubmitFailed
                | Self::ObservedUnknown
                | Self::Denied
        )
    }

    /// A crash may have stranded the record mid-work; the executor re-drives it on boot
    /// (safe: the external effect is idempotent under the business key, a repeated
    /// authority read is only a read, and a `requested` record never got an external
    /// call, so driving it is just the original path).
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::Reconciling | Self::Authorized | Self::Submitted
        )
    }
}

/// A named transition attempt. The driver names what happened; [`advance`] says whether
/// the record may move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// The record starts its kind's work (requested → in flight).
    Begin,
    /// The authority read completed (reconciling → reconciled).
    ReadDone,
    /// The authorization decision committed (requested → authorized).
    Authorize,
    /// The decision is a denial (any non-terminal → denied).
    Deny,
    /// The submit reached the host (authorized → submitted).
    SubmitSent,
    /// The post-submit authority read answered (submitted → `confirmed`|`observed_unknown`).
    Observed {
        /// `true`: the authority shows the effect — `confirmed`. `false`: it could not
        /// prove it in the bound — `observed_unknown`.
        effect_proven: bool,
    },
    /// The submit failed before a definitive answer (authorized → `submit_failed`).
    SubmitFailed,
    /// Boot re-drive: a resumable record returns to its re-runnable state
    /// (reconciling → reconciling; authorized|submitted → authorized).
    Redrive,
}

/// A transition the state machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a {kind:?} record in {from:?} cannot take {event:?}")]
pub struct TransitionError {
    pub kind: ControlKind,
    pub from: ControlStatus,
    pub event: ControlEvent,
}

/// The one way a record's status may change. Every illegal move is an error, never a
/// silent no-op: a record that went sideways is evidence something else is wrong.
pub fn advance(
    kind: ControlKind,
    from: ControlStatus,
    event: ControlEvent,
) -> Result<ControlStatus, TransitionError> {
    let illegal = || TransitionError { kind, from, event };
    // A denial lands from any non-terminal state, for either kind.
    if event == ControlEvent::Deny {
        return if from.is_terminal() {
            Err(illegal())
        } else {
            Ok(ControlStatus::Denied)
        };
    }
    let to = match (kind, from, event) {
        (ControlKind::Reconcile, ControlStatus::Requested, ControlEvent::Begin)
        | (ControlKind::Reconcile, ControlStatus::Reconciling, ControlEvent::Redrive) => {
            ControlStatus::Reconciling
        }
        (ControlKind::Reconcile, ControlStatus::Reconciling, ControlEvent::ReadDone) => {
            ControlStatus::Reconciled
        }
        (ControlKind::Retry, ControlStatus::Requested, ControlEvent::Authorize) => {
            ControlStatus::Authorized
        }
        (ControlKind::Retry, ControlStatus::Authorized, ControlEvent::SubmitSent) => {
            ControlStatus::Submitted
        }
        (
            ControlKind::Retry,
            ControlStatus::Submitted,
            ControlEvent::Observed { effect_proven },
        ) => {
            if effect_proven {
                ControlStatus::Confirmed
            } else {
                ControlStatus::ObservedUnknown
            }
        }
        (ControlKind::Retry, ControlStatus::Authorized, ControlEvent::SubmitFailed) => {
            ControlStatus::SubmitFailed
        }
        (
            ControlKind::Retry,
            ControlStatus::Authorized | ControlStatus::Submitted,
            ControlEvent::Redrive,
        ) => ControlStatus::Authorized,
        _ => return Err(illegal()),
    };
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ControlEvent as E;
    use ControlKind as K;
    use ControlStatus as S;

    fn ok(kind: K, from: S, event: E, to: S) {
        assert_eq!(
            advance(kind, from, event),
            Ok(to),
            "{kind:?} {from:?} {event:?}"
        );
    }

    fn bad(kind: K, from: S, event: E) {
        assert!(
            advance(kind, from, event).is_err(),
            "{kind:?} {from:?} {event:?} must be refused"
        );
    }

    #[test]
    fn the_reconcile_walk() {
        ok(K::Reconcile, S::Requested, E::Begin, S::Reconciling);
        ok(K::Reconcile, S::Reconciling, E::ReadDone, S::Reconciled);
        // Nothing moves a reconciled record.
        for e in [
            E::Begin,
            E::ReadDone,
            E::Authorize,
            E::Deny,
            E::SubmitSent,
            E::Observed {
                effect_proven: true,
            },
            E::SubmitFailed,
            E::Redrive,
        ] {
            bad(K::Reconcile, S::Reconciled, e);
        }
    }

    #[test]
    fn the_retry_walk() {
        ok(K::Retry, S::Requested, E::Authorize, S::Authorized);
        ok(K::Retry, S::Authorized, E::SubmitSent, S::Submitted);
        ok(
            K::Retry,
            S::Submitted,
            E::Observed {
                effect_proven: true,
            },
            S::Confirmed,
        );
        ok(
            K::Retry,
            S::Submitted,
            E::Observed {
                effect_proven: false,
            },
            S::ObservedUnknown,
        );
        ok(K::Retry, S::Authorized, E::SubmitFailed, S::SubmitFailed);
        // A submitted record is never re-decided and never "unsubmitted" backward.
        bad(K::Retry, S::Submitted, E::Authorize);
        bad(K::Retry, S::Submitted, E::SubmitSent);
    }

    #[test]
    fn denial_lands_from_any_non_terminal_state_and_is_terminal() {
        for s in [S::Requested, S::Reconciling, S::Authorized, S::Submitted] {
            ok(K::Retry, s, E::Deny, S::Denied);
            ok(K::Reconcile, s, E::Deny, S::Denied);
        }
        // Denied records never move again — a denial is a decision, not an absence.
        for e in [E::Deny, E::Authorize, E::SubmitSent, E::Redrive] {
            bad(K::Retry, S::Denied, e);
        }
        assert!(S::Denied.is_terminal());
        assert!(!S::Requested.is_terminal());
    }

    #[test]
    fn redrive_only_rewinds_resumable_states() {
        ok(K::Reconcile, S::Reconciling, E::Redrive, S::Reconciling);
        ok(K::Retry, S::Authorized, E::Redrive, S::Authorized);
        ok(K::Retry, S::Submitted, E::Redrive, S::Authorized);
        for s in [
            S::Reconciled,
            S::Confirmed,
            S::SubmitFailed,
            S::ObservedUnknown,
            S::Denied,
        ] {
            assert!(!s.is_resumable());
        }
        for s in [S::Requested, S::Reconciling, S::Authorized, S::Submitted] {
            assert!(s.is_resumable());
        }
    }

    #[test]
    fn a_retry_cannot_be_begun_like_a_reconcile_or_skip_the_submit() {
        bad(K::Retry, S::Requested, E::Begin);
        bad(K::Retry, S::Requested, E::SubmitSent);
        bad(
            K::Retry,
            S::Authorized,
            E::Observed {
                effect_proven: true,
            },
        );
        bad(K::Reconcile, S::Requested, E::Authorize);
        bad(K::Reconcile, S::Reconciling, E::SubmitSent);
    }

    #[test]
    fn names_round_trip() {
        for s in [
            S::Requested,
            S::Reconciling,
            S::Reconciled,
            S::Authorized,
            S::Submitted,
            S::Confirmed,
            S::SubmitFailed,
            S::ObservedUnknown,
            S::Denied,
        ] {
            assert_eq!(S::parse(s.as_str()), Some(s));
        }
        for k in [K::Reconcile, K::Retry] {
            assert_eq!(K::parse(k.as_str()), Some(k));
        }
        for r in [
            Resolution::EffectPresent,
            Resolution::EffectAbsent,
            Resolution::StillUncertain,
        ] {
            assert_eq!(Resolution::parse(r.as_str()), Some(r));
        }
        assert_eq!(K::parse("run_again"), None);
        assert_eq!(S::parse("done"), None);
    }
}
