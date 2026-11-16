//! Bounded cardinality (D-014): each project may introduce at most a fixed number of
//! distinct names (operation, step, model, release, …). A host that emits a fresh name
//! per event cannot grow indexes or the console's filters without bound.

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct NameRegistry {
    cap: usize,
    names: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Known,
    /// New and admitted; the caller persists it.
    New,
    /// New and refused: the project is at its cap.
    Refused,
}

impl NameRegistry {
    pub fn new(cap: usize, known: impl IntoIterator<Item = String>) -> Self {
        Self {
            cap,
            names: known.into_iter().take(cap).collect(),
        }
    }

    pub fn admit(&mut self, name: &str) -> Admission {
        if self.names.contains(name) {
            Admission::Known
        } else if self.names.len() >= self.cap {
            Admission::Refused
        } else {
            self.names.insert(name.to_owned());
            Admission::New
        }
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
    fn admits_up_to_the_cap_then_refuses_only_new_names() {
        let mut r = NameRegistry::new(2, ["a".to_owned()]);
        assert_eq!(r.admit("a"), Admission::Known);
        assert_eq!(r.admit("b"), Admission::New);
        assert_eq!(r.admit("c"), Admission::Refused);
        assert_eq!(r.admit("b"), Admission::Known);
        assert_eq!(r.len(), 2);
    }
}
