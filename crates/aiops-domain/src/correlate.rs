//! Correlation with a job runner's own records (D-027): an operation observed on a
//! runner gets an ID derived from the runner's label and its run ID, so anyone holding
//! both can recompute it, and a claimed link between an operation and a run can be
//! checked before the runner is asked about it. Steps get IDs derived from the operation
//! and the runner's step index, so every attempt of a step, from any adapter instance,
//! lands on one step.

use sha2::{Digest, Sha256};

use crate::ids::{OperationId, StepId};

const DOMAIN: &[u8] = b"aiops/job-runner/v1\0";

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    h.finalize().into()
}

/// The operation that stands for run `run` of the runner labeled `runner`.
pub fn operation_for_run(runner: &str, run: u64) -> OperationId {
    let d = digest(&[b"operation", runner.as_bytes(), &run.to_be_bytes()]);
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    nonzero(b, OperationId::from_bytes)
}

/// The step that stands for step `index` of `operation` (`probe`: the runner's reconcile
/// probe of that step, a step of its own).
pub fn step_for(operation: OperationId, index: u32, probe: bool) -> StepId {
    let kind: &[u8] = if probe { b"probe" } else { b"step" };
    let d = digest(&[kind, operation.as_bytes(), &index.to_be_bytes()]);
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    nonzero(b, StepId::from_bytes)
}

/// IDs may not be all zero; a digest that is (never seen, but possible) gets a one.
fn nonzero<const N: usize, T>(mut b: [u8; N], make: impl Fn([u8; N]) -> Option<T>) -> T {
    loop {
        if let Some(id) = make(b) {
            return id;
        }
        b[N - 1] = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = operation_for_run("dr-local", 7);
        assert_eq!(a, operation_for_run("dr-local", 7), "recomputable");
        assert_ne!(a, operation_for_run("dr-local", 8));
        assert_ne!(a, operation_for_run("dr-other", 7));
        // Length-prefixed: ("dr-1", 23) and ("dr-12", 3) cannot collide by concatenation.
        assert_ne!(operation_for_run("dr-1", 23), operation_for_run("dr-12", 3));
        assert_eq!(a.to_string(), "7e78babdff320cdc53520e23a5efb7d5");
        let s = step_for(a, 2, false);
        assert_eq!(s, step_for(a, 2, false));
        assert_ne!(s, step_for(a, 2, true), "a probe is its own step");
        assert_ne!(s, step_for(a, 3, false));
    }
}
