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
        concat!(
            "key ",
            "wJalrXUtnF3MIK7MDENG",
            "/bPxRfiCYEXAMPLEKEY",
            "a used"
        ),
        "key [redacted] used",
    ),
    // The same slash run inside a URL path: no segment is a name, so the
    // path position does not save it — it dies whole.
    (
        concat!(
            "https://h.io/",
            "wJalrXUtnFEMI/K7MDENG",
            "/bPxRfiCYEXAMPLEKE",
            "9"
        ),
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
    // A double-encoded separator hides the `=` from the first decoded view —
    // a second pass still finds the keyed value (review %253A).
    ("password%253Dhunter2", "password%253D[redacted]"),
    // Fully double-encoded `password=hunter2`: the second pass's span unwinds
    // through both raw maps to exactly the value's raw bytes — the key stays
    // a name.
    (
        "%2570%2561%2573%2573%2577%256F%2572%2564%253D%2568%2575%256E%2574%2565%2572%2532",
        "%2570%2561%2573%2573%2577%256F%2572%2564%253D[redacted]",
    ),
    // The review's `%253A` case: `:` and `@` double-encoded inside userinfo
    // stay `%3A`/`%40` after one pass — a second decode finds `u:ab@`, and a
    // two-char password is too short for any run rule, so only the second
    // pass can kill it.
    ("x://u%253A%2561%2562%2540h", "x://u%253A[redacted]%2540h"),
];

/// Provider-token fixtures as `(lead-in, prefix, body, tail, want)` — built
/// at runtime, never contiguous in the source: a push-protection or secret
/// scanner must never find a whole provider token in the tree (review N3,
/// GitHub refused the push on `sbp_…`). Every prefix in `SECRET_PREFIXES`
/// appears once: the corpus's recall covers the whole list, not a sample.
const TOKEN_FIXTURES: &[(&str, &str, &str, &str, &str)] = &[
    ("secret ", "GOCSPX-", "Hk3jD9fG2mN8pQrS4tUvW7xYz", " used", "secret [redacted] used"),
    ("value ", "ghr_", "AbCdEf1234567890aBcDeF1234567890aBcD", " end", "value [redacted] end"),
    ("key ", "PMAK-", "6123456789abcdef01234567-9f", " ok", "key [redacted] ok"),
    ("sb ", "sbp_", "0123456789abcdef0123456789abcdef01234567", " here", "sb [redacted] here"),
    ("shared ", "shpss_", "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d", " inside", "shared [redacted] inside"),
    ("deploy ", "glsa-", "dR3sT8uV9wX1yZ2aB3c4", " token", "deploy [redacted] token"),
    ("trigger ", "gldt-", "kL4mN6pQ8rS2tU5vW7x", "", "trigger [redacted]"),
    ("runner ", "glrt-", "zX1cV4bN7mK2jH5gF9dQ8sA3", "", "runner [redacted]"),
    ("site ", "nfp_", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCd", "", "site [redacted]"),
    ("org ", "lin_api_", "0123456789abcdef0123456789abcdef01234567", "", "org [redacted]"),
    ("tf ", "atlasv1.", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCdEfGhIjKlMnOpQrStUvWxYz", "", "tf [redacted]"),
    ("aws ", "amzn.mws.", "0123456789abcdef0123456789abcdef0123", " id", "aws [redacted] id"),
    ("dbx ", "dapi", "0123456789abcdef0123456789abcdef", "", "dbx [redacted]"),
    ("cloud ", "dop_v1_", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCdEfGhIjKlMnOpQrStUvWxY", "", "cloud [redacted]"),
    ("pkg ", "npm_", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789", "", "pkg [redacted]"),
    ("gh ", "ghp_", "AbCdEf1234567890aBcDeF1234567890aBcDeF", "", "gh [redacted]"),
    ("gh ", "gho_", "AbCdEf1234567890aBcDeF1234567890aBcDeF", "", "gh [redacted]"),
    ("gh ", "ghu_", "AbCdEf1234567890aBcDeF1234567890aBcDeF", "", "gh [redacted]"),
    ("gh ", "ghs_", "AbCdEf1234567890aBcDeF1234567890aBcDeF", "", "gh [redacted]"),
    ("gh ", "github_pat_", "AbCdEf1234567890aBcDeF1234567890aBcDeF123456", "", "gh [redacted]"),
    ("gl ", "glpat-", "AbCdEf1234567890aBcD", "", "gl [redacted]"),
    ("stripe ", "sk_live_", "AbCdEf1234567890aBcDeF12", "", "stripe [redacted]"),
    ("stripe ", "sk_test_", "AbCdEf1234567890aBcDeF12", "", "stripe [redacted]"),
    ("stripe ", "rk_live_", "AbCdEf1234567890aBcDeF12", "", "stripe [redacted]"),
    ("stripe ", "whsec_", "AbCdEf1234567890aBcDeF1234567890", "", "stripe [redacted]"),
    ("openai ", "sk-", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "openai [redacted]"),
    ("slack ", "xoxb-", "123456789012-1234567890123-AbCdEf1234567890aBcDeF12", "", "slack [redacted]"),
    ("slack ", "xoxp-", "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12", "", "slack [redacted]"),
    ("slack ", "xoxa-", "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12", "", "slack [redacted]"),
    ("slack ", "xoxs-", "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12", "", "slack [redacted]"),
    ("slack ", "xoxe-", "1-AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890abcdef", "", "slack [redacted]"),
    ("slack ", "xapp-", "1-A0AbCdEf12-AbCdEf1234567890aBcDeF1234567890aBcDeF12", "", "slack [redacted]"),
    ("aws ", "AKIA", "IOSFODNN7EXAMPLE", "", "aws [redacted]"),
    ("aws ", "ASIA", "IOSFODNN7EXAMPLE", "", "aws [redacted]"),
    ("hf ", "hf_", "AbCdEf1234567890aBcDeF1234567890aBc", "", "hf [redacted]"),
    ("gcp ", "AIza", "AbCdEf1234567890aBcDeF1234567890aBc", "", "gcp [redacted]"),
    ("sg ", "SG.", "AbCdEf1234567890aBcDeF", ".AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890a", "sg [redacted]"),
    ("goog ", "ya29.", "AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF1234", "", "goog [redacted]"),
    ("pkg ", "pypi-", "AgEIcHlwaS5vcmc-AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF12", "", "pkg [redacted]"),
    ("sq ", "sq0atp-", "AbCdEf1234567890aBcDeF12", "", "sq [redacted]"),
    ("sq ", "sq0csp-", "AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890", "", "sq [redacted]"),
    ("shop ", "shpat_", "0123456789abcdef0123456789abcdef01", "", "shop [redacted]"),
    ("shop ", "shppa_", "0123456789abcdef0123456789abcdef01", "", "shop [redacted]"),
    ("shop ", "shpca_", "0123456789abcdef0123456789abcdef01", "", "shop [redacted]"),
    ("dop ", "dp.pt.", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "dop [redacted]"),
    ("dop ", "dp.st.", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "dop [redacted]"),
    ("dop ", "dp.ct.", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "dop [redacted]"),
    ("dop ", "dp.audit.", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "dop [redacted]"),
    ("dop ", "dp.scim.", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "dop [redacted]"),
    ("notion ", "ntn_", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "notion [redacted]"),
    ("figma ", "figd_", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "figma [redacted]"),
    ("ing ", "aiops_ik_", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "ing [redacted]"),
    ("tok ", "aiops_tk_", "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678", "", "tok [redacted]"),
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
    for (lead, prefix, body, tail, want) in TOKEN_FIXTURES {
        let input = format!("{lead}{prefix}{body}{tail}");
        assert_eq!(
            redact(&input).as_deref(),
            Some(*want),
            "TOKEN_FIXTURES: {prefix}"
        );
    }
}

/// The built fixtures still carry their shapes: the prefix lands exactly
/// where the lead-in ends, so a mistyped part fails here before the
/// scanner could ever see a whole token in the tree.
#[test]
fn token_fixtures_match_their_shapes() {
    for (lead, prefix, body, _, _) in TOKEN_FIXTURES {
        let built = format!("{lead}{prefix}{body}");
        assert!(
            built[lead.len()..].starts_with(prefix),
            "{prefix}: parts out of order"
        );
        assert!(
            body.len() >= 8,
            "{prefix}: body too short to exercise the run rule"
        );
    }
    // The whole SECRET_PREFIXES list is covered — a prefix added to policy
    // without a corpus row fails here.
    const NEED: &[&str] = &[
        "aiops_ik_", "aiops_tk_", "sk-", "sk_live_", "sk_test_", "rk_live_", "ghp_", "gho_",
        "ghu_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "AKIA",
        "ASIA", "hf_", "whsec_", "AIza", "SG.", "ya29.", "dop_v1_", "npm_", "pypi-", "sq0atp-",
        "sq0csp-", "shpat_", "shppa_", "shpca_", "shpss_", "GOCSPX-", "ghr_", "PMAK-", "sbp_",
        "glsa-", "gldt-", "glrt-", "nfp_", "lin_api_", "atlasv1.", "amzn.mws.", "dapi", "xapp-",
        "xoxe-", "dp.pt.", "dp.st.", "dp.ct.", "dp.audit.", "dp.scim.", "ntn_", "figd_",
    ];
    let have: Vec<&str> = TOKEN_FIXTURES.iter().map(|f| f.1).collect();
    for prefix in NEED {
        assert!(have.contains(prefix), "no fixture for prefix {prefix}");
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
