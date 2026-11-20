//! Never-panics fuzz targets for the store's outside-input sanitizers (standing
//! rule, 2026-11-20). `audit_slug`/`audit_target` are the last line between an
//! attacker's URL and the audit log's columns: whatever they return must be a
//! valid slug/target shape or the literal `<invalid>` — never the raw bytes.

use aiops_store::{audit_slug, audit_target};
use proptest::prelude::*;

const SEEDS: &[&str] = &[
    "",
    "pilot",
    "a",
    "evil%0A%80%AEslug",
    "not-hex%0Ainjected",
    "PILOT",
    "pilot_",
    "a b",
    "..",
    "pilot/../admin",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "550e8400-e29b-41d4-a716-446655440000",
    "\u{202e}",
];

fn seed_corpus() -> Vec<String> {
    let mut v: Vec<String> = SEEDS.iter().map(|s| (*s).to_owned()).collect();
    v.push("a".repeat(100));
    v
}

fn valid_slug_shape(v: &str) -> bool {
    v.len() <= 63
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && v.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

fn valid_target_shape(v: &str) -> bool {
    (v.len() == 32
        && v.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        || uuid::Uuid::parse_str(v).is_ok()
}

proptest! {
    #[test]
    fn audit_slug_output_is_always_a_safe_shape(s in "\\PC{0,256}") {
        let out = audit_slug(&s);
        assert!(
            out == "<invalid>" || valid_slug_shape(out),
            "{s:?} -> {out:?}"
        );
    }

    #[test]
    fn audit_target_output_is_always_a_safe_shape(s in "\\PC{0,256}") {
        let out = audit_target(&s);
        assert!(
            out == "<invalid>" || valid_target_shape(out),
            "{s:?} -> {out:?}"
        );
    }
}

#[test]
fn seeds_never_panic_and_never_leak() {
    for s in seed_corpus() {
        let slug = audit_slug(&s);
        assert!(
            slug == "<invalid>" || valid_slug_shape(slug),
            "{s:?} -> {slug:?}"
        );
        let target = audit_target(&s);
        assert!(
            target == "<invalid>" || valid_target_shape(target),
            "{s:?} -> {target:?}"
        );
    }
}
