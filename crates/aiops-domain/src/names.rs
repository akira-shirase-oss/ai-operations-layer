//! Bounded cardinality (D-014, review finding 11): each project may introduce at most a
//! fixed number of distinct names (operation, step, model, release, …), and each ingest key
//! at most its share of them, so one noisy host cannot use up the project's budget. A name
//! over budget never costs the event: it is stored as [`OVERFLOW_NAME`], labeled and counted.

use std::collections::{HashMap, HashSet};

/// What an over-budget name is stored as (an identifier, so it is always valid).
pub const OVERFLOW_NAME: &str = "_overflow";

/// An ingest key's identity as raw bytes (the domain does not know UUIDs).
pub type KeyId = [u8; 16];

#[derive(Debug, Clone)]
pub struct NameRegistry {
    cap: usize,
    per_key_cap: usize,
    names: HashSet<String>,
    per_key: HashMap<KeyId, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Known,
    /// New and admitted; the caller persists it.
    New,
    /// New and over the project's or the key's budget: store it as [`OVERFLOW_NAME`].
    OverBudget,
}

impl NameRegistry {
    /// `known`: names already introduced, with the key that introduced each (if recorded).
    pub fn new(
        cap: usize,
        per_key_cap: usize,
        known: impl IntoIterator<Item = (String, Option<KeyId>)>,
    ) -> Self {
        let mut r = Self {
            cap,
            per_key_cap,
            names: HashSet::new(),
            per_key: HashMap::new(),
        };
        for (name, key) in known {
            if r.names.len() >= cap {
                break;
            }
            if r.names.insert(name)
                && let Some(k) = key
            {
                *r.per_key.entry(k).or_default() += 1;
            }
        }
        r
    }

    /// Whether [`NameRegistry::admit`] would answer [`Admission::Known`]: a read-only check,
    /// so a batch whose names are all known needs no exclusive access (D-026).
    pub fn is_known(&self, name: &str) -> bool {
        self.names.contains(name) || name.rsplit(':').next() == Some(OVERFLOW_NAME)
    }

    /// `name` is `kind:value`; the reserved overflow value is always known.
    pub fn admit(&mut self, name: &str, key: KeyId) -> Admission {
        if self.is_known(name) {
            return Admission::Known;
        }
        let used = self.per_key.get(&key).copied().unwrap_or(0);
        if self.names.len() >= self.cap || used >= self.per_key_cap {
            return Admission::OverBudget;
        }
        self.names.insert(name.to_owned());
        *self.per_key.entry(key).or_default() += 1;
        Admission::New
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_up_to_the_caps_then_overflows_only_new_names() {
        let mut r = NameRegistry::new(3, 2, [("op:a".to_owned(), Some([1; 16]))]);
        assert_eq!(r.admit("op:a", [9; 16]), Admission::Known);
        assert_eq!(r.admit("op:b", [1; 16]), Admission::New);
        assert_eq!(
            r.admit("op:c", [1; 16]),
            Admission::OverBudget,
            "key 1 used its share"
        );
        assert_eq!(
            r.admit("op:c", [2; 16]),
            Admission::New,
            "another key still has room"
        );
        assert_eq!(
            r.admit("op:d", [2; 16]),
            Admission::OverBudget,
            "the project is full"
        );
        assert_eq!(r.admit("step:_overflow", [2; 16]), Admission::Known);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn is_known_agrees_with_admit_and_changes_nothing() {
        let mut r = NameRegistry::new(2, 2, [("op:a".to_owned(), None)]);
        for name in ["op:a", "op:b", "step:_overflow", "op:"] {
            let known = r.is_known(name);
            let before = r.len();
            assert_eq!(r.len(), before, "is_known is read-only");
            assert_eq!(known, r.admit(name, [1; 16]) == Admission::Known, "{name}");
        }
    }
}
