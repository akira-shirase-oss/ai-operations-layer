//! Controlled recovery (A3, D-030): the rules, with no I/O.
//!
//! A recovery never reruns a run. It submits a **new** run of the same work under a key
//! derived from the original business key (`<key>#r<n>`), so the runner's rule that an
//! `op_key` returns its first run stays intact. Before anything is submitted, the external
//! effects the run may have caused are checked against their own authority (the
//! publisher's record, by business key); only "not landed" from every check permits a
//! resubmit. A person plans, then confirms. Success is the runner's word, never this
//! layer's.

/// The runner's longest `op_key`, in bytes.
pub const MAX_KEY_BYTES: usize = 256;
/// Recoveries per business key: past this, a person must look at why it keeps failing.
pub const MAX_RECOVERIES: u32 = 9;

/// Why a recovery is refused before anything is checked or submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The run finished every step: there is nothing to recover, and no generic
    /// "run again" exists (D-007).
    Done,
    /// The run is still queued, running, waiting to retry or reconciling.
    Live,
    /// A state this layer does not know.
    UnknownState,
    /// The runner's state is not the one the person saw.
    StateChanged,
    /// The runner's key for the run is not the business key the person named.
    KeyMismatch,
    /// The derived key would be over the runner's limit, or the key is not usable.
    BadKey,
    /// `MAX_RECOVERIES` were already planned for this business key.
    TooMany,
}

impl Refused {
    pub fn code(self) -> &'static str {
        match self {
            Self::Done => "done_nothing_to_recover",
            Self::Live => "still_running",
            Self::UnknownState => "unknown_state",
            Self::StateChanged => "state_changed",
            Self::KeyMismatch => "business_key_mismatch",
            Self::BadKey => "bad_business_key",
            Self::TooMany => "too_many_recoveries",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Done => {
                "the runner says this run finished every step: there is nothing to recover, and no generic run-again exists"
            }
            Self::Live => "the run is still in progress: wait for the runner to finish it",
            Self::UnknownState => "the runner reports a state this layer does not know",
            Self::StateChanged => {
                "the runner's state is no longer the one you saw: reload and decide again"
            }
            Self::KeyMismatch => "the runner's key for this run is not the business key given",
            Self::BadKey => {
                "the business key cannot carry a recovery suffix within the runner's limit"
            }
            Self::TooMany => {
                "this business key was recovered too often: find out why before trying again"
            }
        }
    }
}

/// Whether a run in the runner's state `state` may be recovered at all.
pub fn recoverable(state: &str) -> Result<(), Refused> {
    match state {
        "done" => Err(Refused::Done),
        "cancelled" => Ok(()),
        s if s
            .strip_prefix("failed:step:")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Ok(())
        }
        "queued" => Err(Refused::Live),
        s if s.starts_with("running:")
            || s.starts_with("retry_waiting:")
            || s.starts_with("reconciling:") =>
        {
            Err(Refused::Live)
        }
        _ => Err(Refused::UnknownState),
    }
}

/// The business key a run's key stands for: a recovery's key (`<root>#r<n>`) stands for
/// its root, so recoveries of recoveries are numbered from the root and every effect
/// check asks about the same business key.
pub fn root_key(key: &str) -> &str {
    match key.rsplit_once("#r") {
        Some((root, n))
            if !root.is_empty()
                && !n.is_empty()
                && n.len() <= 3
                && n.bytes().all(|b| b.is_ascii_digit())
                && !n.starts_with('0') =>
        {
            root
        }
        _ => key,
    }
}

/// The key the `n`-th recovery of `root` is submitted under.
pub fn derived_key(root: &str, n: u32) -> Result<String, Refused> {
    if n == 0 || n > MAX_RECOVERIES {
        return Err(Refused::TooMany);
    }
    let key = format!("{root}#r{n}");
    if root.is_empty() || key.len() > MAX_KEY_BYTES || key.chars().any(char::is_control) {
        return Err(Refused::BadKey);
    }
    Ok(key)
}

/// What one effect check (the effect's own authority, asked by business key) said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The authority has no record of the effect.
    NotLanded,
    /// The authority recorded the effect: resubmitting would repeat it.
    Landed,
    /// The authority could not be asked, or answered something else.
    Unknown(String),
}

impl Effect {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotLanded => "not_landed",
            Self::Landed => "landed",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// The verdict over every configured check: resubmit only when there is at least one
/// check and every one says "not landed". No check is not a pass: an effect nobody asked
/// about is unknown (invariant 4: never rerun blind).
pub fn verdict(checks: &[Effect]) -> Effect {
    if checks.is_empty() {
        return Effect::Unknown("no effect check is configured for this runner".into());
    }
    if checks.contains(&Effect::Landed) {
        return Effect::Landed;
    }
    checks
        .iter()
        .find(|c| matches!(c, Effect::Unknown(_)))
        .cloned()
        .unwrap_or(Effect::NotLanded)
}

/// A recovery's life. `Planned` waits for a person to confirm; `Blocked` is final (an
/// effect landed or is unknown); `Submitting` is held by one confirm; `Submitted` names
/// the new run, whose outcome only the runner reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Planned,
    Blocked,
    Submitting,
    Submitted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Blocked => "blocked",
            Self::Submitting => "submitting",
            Self::Submitted => "submitted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "planned" => Some(Self::Planned),
            "blocked" => Some(Self::Blocked),
            "submitting" => Some(Self::Submitting),
            "submitted" => Some(Self::Submitted),
            _ => None,
        }
    }

    /// The status a plan starts in, from its verdict.
    pub fn of_plan(verdict: &Effect) -> Self {
        if *verdict == Effect::NotLanded {
            Self::Planned
        } else {
            Self::Blocked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_or_cancelled_runs_are_recoverable() {
        assert_eq!(recoverable("failed:step:3"), Ok(()));
        assert_eq!(recoverable("cancelled"), Ok(()));
        assert_eq!(
            recoverable("done"),
            Err(Refused::Done),
            "no generic run again"
        );
        for live in ["queued", "running:1", "retry_waiting:0", "reconciling:2"] {
            assert_eq!(recoverable(live), Err(Refused::Live), "{live}");
        }
        for odd in ["failed:step:", "failed:step:x", "failed", "", "DONE"] {
            assert_eq!(recoverable(odd), Err(Refused::UnknownState), "{odd}");
        }
    }

    #[test]
    fn recoveries_are_numbered_from_the_root_key() {
        assert_eq!(root_key("article/x"), "article/x");
        assert_eq!(root_key("article/x#r1"), "article/x");
        assert_eq!(root_key("article/x#r12"), "article/x");
        for not_suffix in [
            "article/x#r",
            "article/x#r01",
            "article/x#rx",
            "#r1",
            "a#r1234",
        ] {
            assert_eq!(root_key(not_suffix), not_suffix, "{not_suffix}");
        }
        assert_eq!(derived_key("article/x", 1).as_deref(), Ok("article/x#r1"));
        assert_eq!(derived_key("article/x", 0), Err(Refused::TooMany));
        assert_eq!(
            derived_key("article/x", MAX_RECOVERIES + 1),
            Err(Refused::TooMany)
        );
        assert_eq!(derived_key(&"k".repeat(254), 1), Err(Refused::BadKey));
        assert_eq!(derived_key("", 1), Err(Refused::BadKey));
        assert_eq!(derived_key("a\nb", 1), Err(Refused::BadKey));
    }

    #[test]
    fn only_every_check_saying_not_landed_permits_a_resubmit() {
        assert_eq!(verdict(&[Effect::NotLanded]), Effect::NotLanded);
        assert_eq!(
            verdict(&[Effect::NotLanded, Effect::Landed]),
            Effect::Landed
        );
        assert!(matches!(
            verdict(&[Effect::NotLanded, Effect::Unknown("timeout".into())]),
            Effect::Unknown(_)
        ));
        assert_eq!(
            verdict(&[Effect::Unknown("x".into()), Effect::Landed]),
            Effect::Landed,
            "a landed effect is the stronger answer"
        );
        assert!(
            matches!(verdict(&[]), Effect::Unknown(_)),
            "no check is not a pass"
        );
        assert_eq!(Status::of_plan(&Effect::NotLanded), Status::Planned);
        assert_eq!(Status::of_plan(&Effect::Landed), Status::Blocked);
        for s in [
            Status::Planned,
            Status::Blocked,
            Status::Submitting,
            Status::Submitted,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
    }
}
