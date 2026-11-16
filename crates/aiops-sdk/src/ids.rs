//! Identifier generation without a syscall per event: a `SplitMix64` sequence over an
//! atomic counter, seeded once from the operating system. `SplitMix64`'s output function is
//! a bijection, so distinct counter values never collide within one process. Event IDs
//! are derived from (instance, seq), so a re-sent event keeps its ID (idempotent).

use std::sync::atomic::{AtomicU64, Ordering};

use aiops_domain::{EventId, InstanceId, OperationId, StepId};

const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

pub(crate) struct IdGen {
    state: AtomicU64,
    instance: [u8; 16],
}

impl IdGen {
    pub(crate) fn new() -> Result<Self, ()> {
        let mut seed = [0u8; 24];
        getrandom::fill(&mut seed).map_err(|_| ())?;
        let mut instance = [0u8; 16];
        instance.copy_from_slice(&seed[..16]);
        if instance.iter().all(|b| *b == 0) {
            instance[0] = 1;
        }
        let mut s = [0u8; 8];
        s.copy_from_slice(&seed[16..]);
        Ok(Self {
            state: AtomicU64::new(u64::from_le_bytes(s)),
            instance,
        })
    }

    fn next(&self) -> u64 {
        mix(self
            .state
            .fetch_add(GOLDEN, Ordering::Relaxed)
            .wrapping_add(GOLDEN))
    }

    pub(crate) fn instance_id(&self) -> InstanceId {
        InstanceId::from_bytes(self.instance).unwrap_or_else(|| self.instance_id_fallback())
    }

    fn instance_id_fallback(&self) -> InstanceId {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.next().max(1).to_be_bytes());
        InstanceId::from_bytes(b).unwrap_or_else(|| self.instance_id_fallback())
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        loop {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&self.next().to_be_bytes());
            b[8..].copy_from_slice(&self.next().to_be_bytes());
            if let Some(id) = OperationId::from_bytes(b) {
                return id;
            }
        }
    }

    pub(crate) fn step_id(&self) -> StepId {
        loop {
            if let Some(id) = StepId::from_bytes(self.next().to_be_bytes()) {
                return id;
            }
        }
    }

    /// Full jitter in `0..=max_ms`.
    pub(crate) fn jitter_ms(&self, max_ms: u64) -> u64 {
        if max_ms == 0 {
            0
        } else {
            self.next() % (max_ms + 1)
        }
    }
}

/// The event ID for `seq` of `instance`: the instance's first 8 bytes and the sequence
/// number, so it is unique per instance and stable across re-sends.
pub(crate) fn event_id(instance: InstanceId, seq: u64) -> EventId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&instance.as_bytes()[..8]);
    b[8..].copy_from_slice(&seq.to_be_bytes());
    // seq ≥ 1, so the bytes are never all zero.
    EventId::from_bytes(b).unwrap_or_else(|| event_id(instance, seq.wrapping_add(1)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ids_do_not_collide_and_event_ids_are_stable() {
        let g = IdGen::new().unwrap();
        let ops: HashSet<_> = (0..10_000).map(|_| g.operation_id()).collect();
        assert_eq!(ops.len(), 10_000);
        let steps: HashSet<_> = (0..10_000).map(|_| g.step_id()).collect();
        assert_eq!(steps.len(), 10_000);
        let i = g.instance_id();
        assert_eq!(event_id(i, 7), event_id(i, 7));
        assert_ne!(event_id(i, 7), event_id(i, 8));
    }
}
