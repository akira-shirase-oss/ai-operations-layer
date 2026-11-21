//! The labelled redaction corpus (the review's Ruling, 2026-11-20): every
//! entry is labelled `MUST_REDACT` — a credential shape that must not
//! survive — or `MUST_KEEP` — readable operational text that must come
//! through unchanged. The gate is 100% recall on `MUST_REDACT` and a stated
//! false-positive budget on `MUST_KEEP` (today: zero — every keep case is a
//! shape a previous review round asked to keep). Two properties ride with
//! it: random base64/base64url runs of 32–88 chars never leave ≥8
//! contiguous bytes outside a strict-name piece in four contexts, and
//! `redact` never panics over arbitrary Unicode with `%XX` and `\r\n`.
//!
//! Accepted residual (review R2, measured): a `/`-separated piece of a
//! credential run that is itself a strict name — all-lowercase, every
//! `-`/`_` piece under 6 chars, or a dotted version — may keep (a random
//! draw spells `abcdefghij` about as often as `services` does). Everything
//! else about the token dies; no ≥8-byte window survives outside such a
//! piece in any of the four contexts.
//!
//! A corpus failure is a red-first bug: fix `redact`, never relax a label —
//! the labels are the contract three review rounds wrote.

use aiops_domain::policy::{looks_like_secret, redact};
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
        "https://h.io[redacted]",
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
        "https://hooks.example.com[redacted]",
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
    // Base64url secrets in URL paths (review R1): `-`/`_` bytes are run
    // members, never path evidence, and a segment is pathy only when every
    // `-`/`_`-split piece is name-shaped. Telegram bot token, a bare
    // /v1/<43>/, a Discord webhook, and the Slack-webhook regression.
    (
        concat!(
            "POST https://api.telegram.org/",
            "bot110201543:AAHdqTcvCH1vGWJx_",
            "SeofSAs0K5PALDsaw",
            "/sendMessage 429"
        ),
        "POST https://api.telegram.org/bot110201543:[redacted] 429",
    ),
    (
        concat!(
            "x https://h.io/v1/",
            "AbCdEf1234567890aBcDeF1234567890aBcDeF123",
            "/status 200"
        ),
        "x https://h.io[redacted] 200",
    ),
    (
        concat!(
            "GET https://d.co/api/webhooks/138574923/",
            "AbCdEf1234567890aBcDeF1234567890aBcDeF1234",
            "567890aBcDeF1234567890aBcD",
            "/github"
        ),
        "GET https://d.co[redacted]",
    ),
    (
        concat!(
            "GET https://hooks.example.com/services/",
            "T0AB_",
            "CDEF12/B0CD-",
            "EF1234",
            "/xyZab"
        ),
        "GET https://hooks.example.com[redacted]",
    ),
    // Cookies are keyed secret text like any other (review corpus class).
    (
        concat!("Cookie: session=", "AbCdEf1234567890aBc", "; theme=light"),
        "Cookie: [redacted]",
    ),
    // Escapes inside a quoted value belong to the value, never end it
    // (review R3): \" \\ \n and \u00e9 (the json.dumps default) each get a
    // row — the value's real closing quote is where the secret stops.
    (
        concat!("{\"password\":\"Zq8", "\\", "\"kLm;Pw9x\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "\\", "kLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "nkLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "u00e9kLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    // A JWT fragment — the `eyJ` prefix with its dots split away — is still
    // a credential piece (review corpus class: split JWTs).
    (
        concat!(
            "part ",
            "eyJ",
            "AbCdEf1234567890aBcDeF1234567890aBcDeF",
            " more"
        ),
        "part [redacted] more",
    ),
    // A bare 32-hex run is a credential shape like its 40-hex sibling —
    // GitLab runner tokens and API keys wear it (review corpus class).
    (
        concat!("tok ", "a1b2c3d4e5f6a1b2", "c3d4e5f6a1b2c3d4", " ok"),
        "tok [redacted] ok",
    ),
];

/// Provider-token fixtures as `(lead-in, prefix, body, tail, want)` — built
/// at runtime, never contiguous in the source: a push-protection or secret
/// scanner must never find a whole provider token in the tree (review N3,
/// GitHub refused the push on `sbp_…`). Every prefix in `SECRET_PREFIXES`
/// appears once: the corpus's recall covers the whole list, not a sample.
const TOKEN_FIXTURES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "secret ",
        "GOCSPX-",
        "Hk3jD9fG2mN8pQrS4tUvW7xYz",
        " used",
        "secret [redacted] used",
    ),
    (
        "value ",
        "ghr_",
        "AbCdEf1234567890aBcDeF1234567890aBcD",
        " end",
        "value [redacted] end",
    ),
    (
        "key ",
        "PMAK-",
        "6123456789abcdef01234567-9f",
        " ok",
        "key [redacted] ok",
    ),
    (
        "sb ",
        "sbp_",
        "0123456789abcdef0123456789abcdef01234567",
        " here",
        "sb [redacted] here",
    ),
    (
        "shared ",
        "shpss_",
        "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d",
        " inside",
        "shared [redacted] inside",
    ),
    (
        "deploy ",
        "glsa-",
        "dR3sT8uV9wX1yZ2aB3c4",
        " token",
        "deploy [redacted] token",
    ),
    (
        "trigger ",
        "gldt-",
        "kL4mN6pQ8rS2tU5vW7x",
        "",
        "trigger [redacted]",
    ),
    (
        "runner ",
        "glrt-",
        "zX1cV4bN7mK2jH5gF9dQ8sA3",
        "",
        "runner [redacted]",
    ),
    (
        "site ",
        "nfp_",
        "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCd",
        "",
        "site [redacted]",
    ),
    (
        "org ",
        "lin_api_",
        "0123456789abcdef0123456789abcdef01234567",
        "",
        "org [redacted]",
    ),
    (
        "tf ",
        "atlasv1.",
        "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCdEfGhIjKlMnOpQrStUvWxYz",
        "",
        "tf [redacted]",
    ),
    (
        "aws ",
        "amzn.mws.",
        "0123456789abcdef0123456789abcdef0123",
        " id",
        "aws [redacted] id",
    ),
    (
        "dbx ",
        "dapi",
        "0123456789abcdef0123456789abcdef",
        "",
        "dbx [redacted]",
    ),
    (
        "cloud ",
        "dop_v1_",
        "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789AbCdEfGhIjKlMnOpQrStUvWxY",
        "",
        "cloud [redacted]",
    ),
    (
        "pkg ",
        "npm_",
        "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        "",
        "pkg [redacted]",
    ),
    (
        "gh ",
        "ghp_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF",
        "",
        "gh [redacted]",
    ),
    (
        "gh ",
        "gho_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF",
        "",
        "gh [redacted]",
    ),
    (
        "gh ",
        "ghu_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF",
        "",
        "gh [redacted]",
    ),
    (
        "gh ",
        "ghs_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF",
        "",
        "gh [redacted]",
    ),
    (
        "gh ",
        "github_pat_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF123456",
        "",
        "gh [redacted]",
    ),
    ("gl ", "glpat-", "AbCdEf1234567890aBcD", "", "gl [redacted]"),
    (
        "stripe ",
        "sk_live_",
        "AbCdEf1234567890aBcDeF12",
        "",
        "stripe [redacted]",
    ),
    (
        "stripe ",
        "sk_test_",
        "AbCdEf1234567890aBcDeF12",
        "",
        "stripe [redacted]",
    ),
    (
        "stripe ",
        "rk_live_",
        "AbCdEf1234567890aBcDeF12",
        "",
        "stripe [redacted]",
    ),
    (
        "stripe ",
        "whsec_",
        "AbCdEf1234567890aBcDeF1234567890",
        "",
        "stripe [redacted]",
    ),
    (
        "openai ",
        "sk-",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "openai [redacted]",
    ),
    (
        "slack ",
        "xoxb-",
        "123456789012-1234567890123-AbCdEf1234567890aBcDeF12",
        "",
        "slack [redacted]",
    ),
    (
        "slack ",
        "xoxp-",
        "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12",
        "",
        "slack [redacted]",
    ),
    (
        "slack ",
        "xoxa-",
        "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12",
        "",
        "slack [redacted]",
    ),
    (
        "slack ",
        "xoxs-",
        "123456789012-123456789012-1234567890123-abcdef1234567890abcdef1234567890abcdef12",
        "",
        "slack [redacted]",
    ),
    (
        "slack ",
        "xoxe-",
        "1-AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890abcdef",
        "",
        "slack [redacted]",
    ),
    (
        "slack ",
        "xapp-",
        "1-A0AbCdEf12-AbCdEf1234567890aBcDeF1234567890aBcDeF12",
        "",
        "slack [redacted]",
    ),
    ("aws ", "AKIA", "IOSFODNN7EXAMPLE", "", "aws [redacted]"),
    ("aws ", "ASIA", "IOSFODNN7EXAMPLE", "", "aws [redacted]"),
    (
        "hf ",
        "hf_",
        "AbCdEf1234567890aBcDeF1234567890aBc",
        "",
        "hf [redacted]",
    ),
    (
        "gcp ",
        "AIza",
        "AbCdEf1234567890aBcDeF1234567890aBc",
        "",
        "gcp [redacted]",
    ),
    (
        "sg ",
        "SG.",
        "AbCdEf1234567890aBcDeF",
        ".AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890a",
        "sg [redacted]",
    ),
    (
        "goog ",
        "ya29.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF1234",
        "",
        "goog [redacted]",
    ),
    (
        "pkg ",
        "pypi-",
        "AgEIcHlwaS5vcmc-AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890aBcDeF12",
        "",
        "pkg [redacted]",
    ),
    (
        "sq ",
        "sq0atp-",
        "AbCdEf1234567890aBcDeF12",
        "",
        "sq [redacted]",
    ),
    (
        "sq ",
        "sq0csp-",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF1234567890",
        "",
        "sq [redacted]",
    ),
    (
        "shop ",
        "shpat_",
        "0123456789abcdef0123456789abcdef01",
        "",
        "shop [redacted]",
    ),
    (
        "shop ",
        "shppa_",
        "0123456789abcdef0123456789abcdef01",
        "",
        "shop [redacted]",
    ),
    (
        "shop ",
        "shpca_",
        "0123456789abcdef0123456789abcdef01",
        "",
        "shop [redacted]",
    ),
    (
        "dop ",
        "dp.pt.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "dop [redacted]",
    ),
    (
        "dop ",
        "dp.st.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "dop [redacted]",
    ),
    (
        "dop ",
        "dp.ct.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "dop [redacted]",
    ),
    (
        "dop ",
        "dp.audit.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "dop [redacted]",
    ),
    (
        "dop ",
        "dp.scim.",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "dop [redacted]",
    ),
    (
        "notion ",
        "ntn_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "notion [redacted]",
    ),
    (
        "figma ",
        "figd_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "figma [redacted]",
    ),
    (
        "ing ",
        "aiops_ik_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "ing [redacted]",
    ),
    (
        "tok ",
        "aiops_tk_",
        "AbCdEf1234567890aBcDeF1234567890aBcDeF12345678",
        "",
        "tok [redacted]",
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
    // Hyphenated path segments are names only when EVERY `-`/`_`-piece is
    // (R1): these model/run identifiers must survive the tightened rule.
    "mistralai/Mistral-7B-Instruct-v0",
    "run RunId42-Error retried",
    "log collector-2026-11-20 rotated",
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
    // The whole SECRET_PREFIXES list is covered — a prefix added to policy
    // without a corpus row fails here.
    const NEED: &[&str] = &[
        "aiops_ik_",
        "aiops_tk_",
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "AKIA",
        "ASIA",
        "hf_",
        "whsec_",
        "AIza",
        "SG.",
        "ya29.",
        "dop_v1_",
        "npm_",
        "pypi-",
        "sq0atp-",
        "sq0csp-",
        "shpat_",
        "shppa_",
        "shpca_",
        "shpss_",
        "GOCSPX-",
        "ghr_",
        "PMAK-",
        "sbp_",
        "glsa-",
        "gldt-",
        "glrt-",
        "nfp_",
        "lin_api_",
        "atlasv1.",
        "amzn.mws.",
        "dapi",
        "xapp-",
        "xoxe-",
        "dp.pt.",
        "dp.st.",
        "dp.ct.",
        "dp.audit.",
        "dp.scim.",
        "ntn_",
        "figd_",
    ];
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

/// `looks_like_secret` names the fields an emitter should never send: a
/// key:value pair is not an identifier, and a bare long hex run is a
/// credential shape (review N3 residual). Model ids and service names are
/// not credentials.
#[test]
fn identifier_fields() {
    for input in ["password:x", "token=a1b2c3d4"] {
        assert!(looks_like_secret(input), "secret: {input}");
    }
    for input in [
        "mistralai/Mistral-7B-Instruct-v0.3",
        "auth:login",
        "token:expired",
        "session:timeout",
        "stripe:charge:failed",
        "run-2026-11-20",
        "collector-01",
    ] {
        assert!(!looks_like_secret(input), "keep: {input}");
    }
}

/// The four contexts the property runs in (review R2): after a space,
/// after `host/`, after `//`, and inside a hyphenated path.
const CONTEXTS: &[&str] = &[
    "key {T} used",
    "GET https://h.io/{T} 200",
    "x //{T} y",
    "GET https://h.io/prefix-{T}-suffix 200",
];

/// Mirrors `policy.rs`'s keep rule: when a `/`-joined run dies, only the
/// FIRST segment may survive it — and only when that segment is itself a
/// strict name: no uppercase, every `-`/`_` piece under 6 chars, or a
/// dotted version. (A run that is name-shaped end to end is kept whole and
/// `redact` returns `None`, so it never reaches the window loop.) The
/// measured residual this file's header records.
fn strict_segment(s: &[u8]) -> bool {
    !s.is_empty()
        && s.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && (!s.iter().any(u8::is_ascii_uppercase)
            || s.split(|b| matches!(b, b'-' | b'_'))
                .all(|p| !p.is_empty() && p.len() < 6)
            || (s.contains(&b'.')
                && s.split(|b| *b == b'.')
                    .all(|p| !p.is_empty() && p.len() <= 6)))
}

fn assert_no_secret_bytes(token: &str) -> Result<(), TestCaseError> {
    for ctx in CONTEXTS {
        let input = ctx.replace("{T}", token);
        let Some(out) = redact(&input) else { continue };
        let pieces: Vec<&[u8]> = token.as_bytes().split(|b| *b == b'/').collect();
        for win in token.as_bytes().windows(8) {
            let Ok(w) = std::str::from_utf8(win) else {
                continue;
            };
            if out.contains(w) {
                // Survival is allowed only fully inside the first
                // `/`-segment, and only when it is a strict name.
                let inside = pieces
                    .first()
                    .is_some_and(|p| strict_segment(p) && p.windows(8).any(|pw| pw == win));
                prop_assert!(inside, "{token:?} left {w:?} in {ctx:?} → {out:?}");
            }
        }
    }
    Ok(())
}

proptest! {
    /// A random standard-base64 run of 32–88 chars — the AWS secret-key
    /// shape — never leaves ≥8 contiguous bytes outside a strict-name
    /// piece, in all four contexts (review R2).
    #[test]
    fn random_base64_never_leaves_eight_bytes(token in "[A-Za-z0-9+/]{32,88}={0,2}") {
        prop_assume!(token.bytes().any(|b| b.is_ascii_uppercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_lowercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_digit()));
        assert_no_secret_bytes(&token)?;
    }

    /// The same for the base64url alphabet — `-`/`_` runs included
    /// (Telegram/JWT-shaped tokens).
    #[test]
    fn random_base64url_never_leaves_eight_bytes(token in "[A-Za-z0-9_\\-]{32,88}") {
        prop_assume!(token.bytes().any(|b| b.is_ascii_uppercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_lowercase()));
        prop_assume!(token.bytes().any(|b| b.is_ascii_digit()));
        assert_no_secret_bytes(&token)?;
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
