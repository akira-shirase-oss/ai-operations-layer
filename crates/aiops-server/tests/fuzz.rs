//! Never-panics fuzz targets for the server's outside-input parsers (standing
//! rule, 2026-11-20). `business_key_id` is the boundary between a caller's text
//! and the control adapters' outbound URLs: an accepted key must be one URL
//! segment, nothing else.

use aiops_server::control::business_key_id;
use proptest::prelude::*;

const SEEDS: &[&str] = &[
    "",
    "req-7",
    "a",
    ".",
    "..",
    "req-7/../../admin",
    "req-7%2F..%2Fadmin",
    "req-7/../../../etc/passwd",
    "req-7?debug=true",
    "req-7#frag",
    "req-7@internal",
    "req 7",
    "req-7\nGET /admin",
    "req-\u{202e}7",
];

fn seed_corpus() -> Vec<String> {
    let mut v: Vec<String> = SEEDS.iter().map(|s| (*s).to_owned()).collect();
    v.push("a".repeat(65));
    v.push("a".repeat(64));
    v
}

proptest! {
    #[test]
    fn business_key_id_never_panics_and_accepted_means_one_segment(s in "\\PC{0,128}") {
        let ok = business_key_id(&s);
        if ok {
            // The contract callers rely on: one path segment, no structural
            // bytes, bounded length.
            assert!(s.len() <= 64, "overlong id accepted: {s:?}");
            assert!(
                s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
                "structural byte accepted: {s:?}"
            );
            assert!(s != "." && s != "..", "dot-segment accepted: {s:?}");
        }
    }
}

#[test]
fn seeds_never_panic() {
    for s in seed_corpus() {
        let _ = business_key_id(&s);
    }
}
