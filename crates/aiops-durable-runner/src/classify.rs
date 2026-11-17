//! What a handler call's result means, from the run's point of view (D-020, D-027). Pure:
//! the proxy measures, this decides.

use std::time::Duration;

use aiops_sdk::{FailureClass, Finish, UncertainReason};

use crate::config::{Price, RouteKind};

/// What the proxy saw of one forwarded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// The handler answered with this status (its reply read within the bound).
    Answered {
        status: u16,
        usage: Option<(u64, u64)>,
    },
    /// The handler's reply was over the runner's output limit (the runner fails it).
    ReplyOverLimit { status: u16 },
    /// Nothing reached the handler: connecting failed.
    Unreachable,
    /// The request was sent and the connection was lost before a readable answer.
    ConnectionLost,
    /// No answer by the adapter's own deadline.
    NoAnswer,
}

/// The step outcome to report (an HTTP status the handler gave is in `error_code` as
/// `http_<status>`, or in the message when the runner had already given up).
pub fn call_outcome(
    seen: &Seen,
    kind: RouteKind,
    elapsed: Duration,
    runner_deadline: Duration,
    price: Option<&Price>,
) -> Finish {
    let late = elapsed > runner_deadline;
    let ms = |d: Duration| d.as_millis();
    match seen {
        // The runner gave up on the call before it ended: whatever the handler did, the run
        // does not know, and the effect may have happened. The runner reconciles next.
        _ if late => {
            let what = match seen {
                Seen::Answered { status, .. } | Seen::ReplyOverLimit { status } => {
                    format!("the handler answered {status} after {} ms", ms(elapsed))
                }
                Seen::NoAnswer => format!("the handler had not answered after {} ms", ms(elapsed)),
                Seen::ConnectionLost => "the connection to the handler was lost".to_owned(),
                Seen::Unreachable => "the handler could not be reached".to_owned(),
            };
            Finish::uncertain(UncertainReason::TimeoutAfterSend, "runner_deadline_passed").message(
                &format!(
                    "the runner's {} ms deadline passed first; {what}",
                    ms(runner_deadline)
                ),
            )
        }
        Seen::Answered { status, usage } if (200..300).contains(status) => {
            let mut f = Finish::succeeded();
            if let Some((input, output)) = *usage {
                f = f.usage(input, output);
                if let Some(p) = price {
                    let micros = input
                        .saturating_mul(p.input_micros_per_token)
                        .saturating_add(output.saturating_mul(p.output_micros_per_token));
                    f = f.cost_estimate(micros, &p.currency, &p.basis);
                }
            }
            f
        }
        Seen::Answered { status, .. } => {
            // A definitive answer. A check that refused its input is a validation failure;
            // anything else the handler refused or failed at is `rejected` (the runner
            // retries both, under the same idempotency key).
            let class = if kind == RouteKind::Validation && (400..500).contains(status) {
                FailureClass::Validation
            } else {
                FailureClass::Rejected
            };
            Finish::failed(class, &format!("http_{status}"))
        }
        Seen::ReplyOverLimit { status } => {
            Finish::failed(FailureClass::Internal, "reply_over_limit").message(&format!(
                "the handler answered {status} with a reply over the runner's 256 KiB output limit"
            ))
        }
        Seen::Unreachable => Finish::failed(FailureClass::Transport, "handler_unreachable"),
        Seen::ConnectionLost => Finish::uncertain(
            UncertainReason::ConnectionLostAfterSend,
            "handler_connection_lost",
        ),
        Seen::NoAnswer => Finish::uncertain(UncertainReason::TimeoutAfterSend, "handler_no_answer"),
    }
}

/// The class of a failed attempt (kept for the operation's finish if the run fails at that
/// step); `None` for a success and for an attempt whose outcome is unknown.
pub fn failure_of(
    seen: &Seen,
    kind: RouteKind,
    elapsed: Duration,
    runner_deadline: Duration,
) -> Option<FailureClass> {
    if elapsed > runner_deadline {
        return None;
    }
    match seen {
        Seen::Answered { status, .. } if (200..300).contains(status) => None,
        Seen::Answered { status, .. }
            if kind == RouteKind::Validation && (400..500).contains(status) =>
        {
            Some(FailureClass::Validation)
        }
        Seen::Answered { .. } => Some(FailureClass::Rejected),
        Seen::ReplyOverLimit { .. } => Some(FailureClass::Internal),
        Seen::Unreachable => Some(FailureClass::Transport),
        Seen::ConnectionLost | Seen::NoAnswer => None,
    }
}

/// What a reconcile probe found: the runner asks "did the call under this key land?", and
/// takes anything but a 2xx as "no".
pub fn probe_outcome(seen: &Seen) -> Finish {
    match seen {
        Seen::Answered { status, .. } | Seen::ReplyOverLimit { status }
            if (200..300).contains(status) =>
        {
            Finish::succeeded()
        }
        Seen::Answered { status, .. } | Seen::ReplyOverLimit { status } => {
            Finish::failed(FailureClass::Rejected, "effect_not_found").message(&format!(
                "the handler answered {status}: the call did not land; the runner retries it"
            ))
        }
        Seen::Unreachable => Finish::failed(FailureClass::Transport, "handler_unreachable").message(
            "the probe could not reach the handler; the runner reads that as not landed and retries",
        ),
        Seen::ConnectionLost | Seen::NoAnswer => {
            Finish::uncertain(UncertainReason::TimeoutAfterSend, "probe_no_answer").message(
                "the probe got no answer; the runner reads that as not landed and retries",
            )
        }
    }
}

/// A run state as `GET /jobs/{id}` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Done,
    Failed {
        step: u32,
    },
    Cancelled,
    /// Not terminal: queued, running, waiting to retry or reconciling.
    Live,
}

impl RunState {
    pub fn parse(state: &str) -> Option<Self> {
        match state {
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            s if s.starts_with("failed:step:") => s["failed:step:".len()..]
                .parse()
                .ok()
                .map(|step| Self::Failed { step }),
            s if s == "queued"
                || s.starts_with("running:")
                || s.starts_with("retry_waiting:")
                || s.starts_with("reconciling:") =>
            {
                Some(Self::Live)
            }
            _ => None,
        }
    }
}

/// The operation's declared finish for a terminal run, taken from the runner's own answer
/// (`last_failure`: the class of the last failed attempt this adapter saw of that step).
pub fn run_finish(
    state: &RunState,
    raw: &str,
    last_failure: Option<FailureClass>,
) -> Option<Finish> {
    let said = format!("Durable Runner reports {raw}");
    match state {
        RunState::Done => Some(Finish::succeeded()),
        RunState::Failed { step } => Some(
            Finish::failed(
                last_failure.unwrap_or(FailureClass::Internal),
                &format!("failed_at_step_{step}"),
            )
            .message(&said),
        ),
        RunState::Cancelled => Some(Finish::failed(FailureClass::Internal, "cancelled").message(
            &format!(
                "{said}: scheduling stopped; a cancel never undoes an effect already attempted"
            ),
        )),
        RunState::Live => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEADLINE: Duration = Duration::from_millis(1_000);
    const FAST: Duration = Duration::from_millis(5);
    const LATE: Duration = Duration::from_millis(1_400);

    fn parts(f: &Finish) -> String {
        format!("{f:?}")
    }

    #[test]
    fn a_call_the_runner_gave_up_on_is_uncertain_whatever_the_handler_said() {
        for seen in [
            Seen::Answered {
                status: 200,
                usage: None,
            },
            Seen::Answered {
                status: 500,
                usage: None,
            },
            Seen::NoAnswer,
        ] {
            let f = call_outcome(&seen, RouteKind::Tool, LATE, DEADLINE, None);
            assert_eq!(failure_of(&seen, RouteKind::Tool, LATE, DEADLINE), None);
            let p = parts(&f);
            assert!(p.contains("TimeoutAfterSend"), "{seen:?}: {p}");
            assert!(p.contains("runner_deadline_passed"), "{p}");
        }
    }

    #[test]
    fn definitive_answers_are_told_apart() {
        let ok = call_outcome(
            &Seen::Answered {
                status: 200,
                usage: Some((10, 20)),
            },
            RouteKind::Model,
            FAST,
            DEADLINE,
            Some(&Price {
                input_micros_per_token: 1,
                output_micros_per_token: 4,
                currency: "USD".into(),
                basis: "list".into(),
            }),
        );
        assert!(parts(&ok).contains("Succeeded"));
        assert!(parts(&ok).contains("micros: 90"), "10×1 + 20×4: {ok:?}");
        let invalid = call_outcome(
            &Seen::Answered {
                status: 422,
                usage: None,
            },
            RouteKind::Validation,
            FAST,
            DEADLINE,
            None,
        );
        assert!(parts(&invalid).contains("Validation"), "{invalid:?}");
        let refused = call_outcome(
            &Seen::Answered {
                status: 409,
                usage: None,
            },
            RouteKind::Tool,
            FAST,
            DEADLINE,
            None,
        );
        assert!(parts(&refused).contains("http_409"));
        let down = call_outcome(&Seen::Unreachable, RouteKind::Tool, FAST, DEADLINE, None);
        assert!(parts(&down).contains("Transport"));
        let lost = call_outcome(&Seen::ConnectionLost, RouteKind::Tool, FAST, DEADLINE, None);
        assert!(parts(&lost).contains("ConnectionLostAfterSend"));
        let refusal = Seen::Answered {
            status: 422,
            usage: None,
        };
        assert_eq!(
            failure_of(&refusal, RouteKind::Validation, FAST, DEADLINE),
            Some(FailureClass::Validation)
        );
        assert_eq!(
            failure_of(&refusal, RouteKind::Tool, FAST, DEADLINE),
            Some(FailureClass::Rejected)
        );
        assert_eq!(
            failure_of(&Seen::ConnectionLost, RouteKind::Tool, FAST, DEADLINE),
            None
        );
    }

    #[test]
    fn run_states_parse_and_only_terminal_ones_finish() {
        assert_eq!(RunState::parse("done"), Some(RunState::Done));
        assert_eq!(
            RunState::parse("failed:step:2"),
            Some(RunState::Failed { step: 2 })
        );
        assert_eq!(RunState::parse("reconciling:step:3"), Some(RunState::Live));
        assert_eq!(
            RunState::parse("retry_waiting:step:0:attempt:2"),
            Some(RunState::Live)
        );
        assert_eq!(RunState::parse("failed:step:x"), None);
        assert_eq!(RunState::parse("exploded"), None);
        assert!(run_finish(&RunState::Live, "queued", None).is_none());
        let f = run_finish(
            &RunState::Failed { step: 1 },
            "failed:step:1",
            Some(FailureClass::Validation),
        )
        .unwrap();
        assert!(parts(&f).contains("Validation") && parts(&f).contains("failed_at_step_1"));
    }
}
