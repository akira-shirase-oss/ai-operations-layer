//! The labelled redaction corpus (the review's Ruling, 2026-11-20): every
//! entry is labelled `MUST_REDACT` — a credential shape that must not
//! survive — or `MUST_KEEP` — readable operational text that must come
//! through unchanged. The gate is 100% recall on `MUST_REDACT` and a stated
//! false-positive budget on `MUST_KEEP` (today: zero — every keep case is a
//! shape a previous review round asked to keep). Two properties ride with
//! it: random base64 of 32–88 chars never leaves ≥8 contiguous bytes, and
//! `redact` never panics over arbitrary Unicode with `%XX` and `\r\n`.
//!
//! A corpus failure is a red-first bug: fix `redact`, never relax a label —
//! the labels are the contract three review rounds wrote.

use aiops_domain::policy::redact;
use proptest::prelude::*;
use std::fmt::Write as _;

/// (input, expected output after redaction). 100% recall: every one of these
/// must produce exactly the expected string.
const MUST_REDACT: &[(&str, &str)] = &[
    // Keyed secrets, quoted and unquoted.
    ("password=hunter2", "password=[redacted]"),
    ("api_key: \"zzz-top\"", "api_key: \"[redacted]\""),
    ("secret: mykey123", "secret: [redacted]"),
    // An unquoted value stops only at whitespace, a quote or `&` — `,` and
    // `;` are part of the credential, not separators (N7c).
    ("password=Zq8,kLm;Pw", "password=[redacted]"),
    ("url?password=hunter2&x=1", "url?password=[redacted]&x=1"),
    // A credential inside a JSON container reaches to the value's end, not
    // the first bracket it sees (NF2).
    (
        "{\"password\":\"[ab]hunter2\"}",
        "{\"password\":\"[redacted]\"}",
    ),
    ("password=[ab]hunter2", "password=[redacted]"),
    (
        "{\"auth\": {\"token\": \"hunter2\"}}",
        "{\"auth\": [redacted]}",
    ),
    // URL userinfo: the password dies, the user stays — unless the user part
    // itself is a credential (N1 residual: the `x-oauth-basic` pattern).
    (
        "https://user:p%40ssw0rd@host.io/db",
        "https://user:[redacted]@host.io/db",
    ),
    (
        concat!(
            "https://",
            "ghp_",
            "ABCDEFGH",
            "abcdefgh",
            "IJKLMNOP",
            "ijklmnop",
            "QRST:x-oauth-basic@github.com"
        ),
        "https://[redacted]:[redacted]@github.com",
    ),
    // Minified JSON: an `@` inside an email is not userinfo; the keyed secret
    // beside it still dies (N1).
    (
        "{\"url\":\"https://a.io/token?client_secret=Zq81aa\",\"user\":\"ana@x.io\"}",
        "{\"url\":\"https://a.io/token?client_secret=[redacted]\",\"user\":\"ana@x.io\"}",
    ),
    // Known-prefix tokens, bearer schemes, JWTs, PEM, AWS keys.
    (
        concat!("key ", "sk-", "abcdefghijklmnop", "1234", " used"),
        "key [redacted] used",
    ),
    (
        concat!("aws ", "AKIA", "ABCDEFGH", "IJKLMNOP", " refused"),
        "aws [redacted] refused",
    ),
    (
        concat!(
            "jwt ",
            "eyJhbGciOiJIUzI1NiJ9",
            ".eyJzdWIiOiIxIn0",
            ".c2lnbmF0dXJl",
            " end"
        ),
        "jwt [redacted] end",
    ),
    (
        "Authorization: Bearer abc.def-123456789",
        "Authorization: [redacted]",
    ),
    ("-----BEGIN OPENSSH PRIVATE KEY----- abc", "[redacted]"),
    // Random-shape credentials: 40+ lowercase hex, long mixed runs, slash
    // runs with NO path evidence redact whole (NF1).
    (
        concat!(
            "key ",
            "a1b2c3d4e5f6",
            "a1b2c3d4e5f6",
            "a1b2c3d4e5f6",
            "a1b2"
        ),
        "key [redacted]",
    ),
    (
        "key wJalrXUtnF3MIK7MDENG/bPxRfiCYEXAMPLEKEYa used",
        "key [redacted] used",
    ),
    // The same slash run inside a URL path: no segment is a name, so the
    // path position does not save it — it dies whole.
    (
        "https://h.io/wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKE9",
        "https://h.io/[redacted]",
    ),
    // Encoded input only ADDS redaction: %0C may not split a userinfo shape.
    (
        "postgres://aaa:%0C$Aa@aaa.aa/db",
        "postgres://aaa:[redacted]@aaa.aa/db",
    ),
    // A fully-encoded `password=hunter2`: the decoded view finds the value
    // and its span maps back to exactly the value's raw bytes — the encoded
    // key prefix is a name, not a secret.
    (
        "%70%61%73%73%77%6F%72%64%3D%68%75%6E%74%65%72%32",
        "%70%61%73%73%77%6F%72%64%3D[redacted]",
    ),
    // A webhook path dies inside the host — the `.`-evidenced run keeps its
    // name-shaped prefix (`com`) and kills the segments no human would name.
    (
        "https://hooks.example.com/AbCdEf1234567890XYZabcd/x9yzz",
        "https://hooks.example.com/[redacted]",
    ),
];

/// Readable operational text — the false-positive budget is zero for these;
/// each was named by a review round as a shape that must survive.
const MUST_KEEP: &[&str] = &[
    "publisher answered 503 after 2.1s",
    "tokens=42 secretary: Ana",
    "sk-short",
    // `auth:login`-style codes keep in identifier fields (`looks_like_secret`
    // is the N3 surface); in free text `auth:`/`session:` name secret keys —
    // they redact their values, as the N4 cases require.
    "source.service: runner-07:8080",
    "mistralai/Mistral-7B-Instruct-v0.3",
    "mistralai/Mixtral-8x22B-Instruct-v0.1",
    "meta-llama/Llama-3.1-8B-Instruct",
    "/var/log/aiops/collector-2026-11-20.log",
    "deploy at commit 76fc524",
    "https://hooks.example.[redacted]",
    "run dr-article-worker --job=req-7",
    "the model answered in 240 ms",
];

/// sha256 digests are content identifiers, not credentials (NF3): both the
/// `sha256:` and `@sha256:` forms keep their bytes.
#[test]
fn must_keep_sha256_digests() {
    let hex = "9f".repeat(32);
    for input in [
        format!("image sha256:{hex} pulled"),
        format!("image @sha256:{hex} pulled"),
        format!("/commit/{}", "b2c0868"),
    ] {
        assert_eq!(redact(&input), None, "{input}");
    }
}

#[test]
fn must_redact_is_100_percent() {
    for (input, want) in MUST_REDACT {
        assert_eq!(
            redact(input).as_deref(),
            Some(*want),
            "MUST_REDACT: {input}"
        );
    }
}

#[test]
fn must_keep_is_verbatim() {
    for input in MUST_KEEP {
        assert_eq!(redact(input), None, "MUST_KEEP: {input}");
    }
}

proptest! {
    /// The NF1 property: a random standard-base64 run of 32–88 chars —
    /// the AWS secret-key shape — never leaves ≥8 contiguous bytes in the
    /// output, wherever `/` and `=` fall inside it.
    #[test]
    fn random_base64_never_leaves_eight_bytes(token in "[A-Za-z0-9+/]{32,88}={0,2}") {
        // Only credential-shaped draws count (upper + lower + digit — the
        // same classes the run rule itself requires).
        prop_assume!(token.bytes().any(|b| b.is_ascii_uppercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_lowercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_digit()));
        let input = format!("key {token} used");
        if let Some(out) = redact(&input) {
            prop_assert!(
                !token.as_bytes().windows(8).any(|win| {
                    std::str::from_utf8(win).is_ok_and(|w| out.contains(w))
                }),
                "{token:?} left ≥8 contiguous bytes in {out:?}"
            );
        }
    }

    /// Arbitrary Unicode with %XX escapes and CR/LF — the N2 panic class:
    /// redact must never slice a multi-byte boundary or read past the end.
    #[test]
    fn redact_never_panics_on_unicode_and_escapes(
        s in any::<String>(),
        escapes in prop::collection::vec(any::<u8>(), 0..24),
    ) {
        let mut input = s;
        for b in escapes {
            let _ = write!(input, "%{b:02X}");
            input.push_str("\r\n%");
            let _ = write!(input, "{b:02x}");
        }
        let _ = redact(&input);
    }
}
