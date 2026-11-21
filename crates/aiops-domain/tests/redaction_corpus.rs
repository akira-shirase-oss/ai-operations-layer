//! The labelled redaction corpus (the review's Ruling, 2026-11-20), split per
//! `docs/REDACTION-POLICY.md` into two tiers. `KEYED_REDACT` is the
//! deterministic tier: a `CREDENTIAL_KEYS` name in `k=v` / `k:v` / `"k":"v"`
//! (encoded or escaped however deep) decides — the value dies whatever its
//! shape, and the tier's 100% is constructional: the rule is a closed set,
//! not a measured heuristic. `BACKSTOP_REDACT` is the measured tier: unkeyed
//! text where `redact`'s heuristics are the only line — token runs, provider
//! prefixes, JWTs, userinfo, PEM. Its guarantee is the corpus itself plus the
//! documented residual below — that residual is the *whole* residual, now
//! that keyed text answers to the closed rule. `MUST_KEEP` keeps its zero
//! false-positive budget — every keep case is a shape a review round asked
//! to keep. Two properties ride with the tiers: random base64/base64url runs
//! of 32–88 chars never leave ≥8 contiguous bytes outside a strict-name
//! piece in five contexts, and `redact` never panics over arbitrary Unicode
//! with `%XX` and `\r\n`.
//!
//! Accepted residual (review R2, measured — the backstop tier only): a
//! `/`-separated piece of a credential run that is itself a strict name —
//! all-lowercase, every `-`/`_` piece under 6 chars, or a dotted version —
//! may keep (a random draw spells `abcdefghij` about as often as `services`
//! does). Everything else about the token dies; no ≥8-byte window survives
//! outside such a piece in any of the five contexts. A whole run may keep
//! the same way — every `/`-segment name-shaped AND some `-`/`_` piece a
//! real word (≥4 all-lowercase: `mistralai`, `services`) — a path or a
//! model id, never a credential; a word-less run of name pieces is a token
//! and dies.
//!
//! A corpus failure is a red-first bug: fix `redact`, never relax a label —
//! the labels are the contract three review rounds wrote.

use aiops_domain::policy::{looks_like_secret, redact};
use proptest::prelude::*;
use std::fmt::Write as _;

/// The deterministic tier: a `CREDENTIAL_KEYS`-named key in `k=v`, `k:v`,
/// `"k":"v"` or a header line decides, and the value dies whatever its
/// shape — quoted, bracketed, container-nested, percent-encoded once or
/// twice, or JSON-escaped. 100% by construction: the deciding rule is a
/// closed name set (`credential_key`), not a value-shape heuristic.
const KEYED_REDACT: &[(&str, &str)] = &[
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
    // Minified JSON: an `@` inside an email is not userinfo; the keyed secret
    // beside it still dies (N1).
    (
        "{\"url\":\"https://a.io/token?client_secret=Zq81aa\",\"user\":\"ana@x.io\"}",
        "{\"url\":\"https://a.io/token?client_secret=[redacted]\",\"user\":\"ana@x.io\"}",
    ),
    // Header-keyed secret text: `Authorization` and `Cookie` name credentials
    // — the whole value dies whatever the scheme (review corpus class).
    (
        "Authorization: Bearer abc.def-123456789",
        "Authorization: [redacted]",
    ),
    (
        concat!("Cookie: session=", "AbCdEf1234567890aBc", "; theme=light"),
        "Cookie: [redacted]",
    ),
    // A fully-encoded `password=hunter2`: the decoded view finds the value
    // and its span maps back to exactly the value's raw bytes — the encoded
    // key prefix is a name, not a secret.
    (
        "%70%61%73%73%77%6F%72%64%3D%68%75%6E%74%65%72%32",
        "%70%61%73%73%77%6F%72%64%3D[redacted]",
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
    // In an escaped-quote context (`\"key\":\"value\"`) a backslash RUN of
    // two or more is the value's own content — `\\` a literal backslash,
    // `\\\"` that plus an escaped quote — and only a single-backslash
    // `\"` closes. Stopping at the first `\` leaked the tail (review N-D):
    // `\\\"`, `\\u00e9`, `\\n` and PHP's `\\/` each get a row.
    (
        concat!("{\"password\":\"Zq8", "\\", "\\", "\\", "\"kLm;Pw9x\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "\\", "u00e9kLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "\\", "nkLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
    (
        concat!("{\"password\":\"Zq8", "\\", "\\", "/kLm\"}"),
        "{\"password\":\"[redacted]\"}",
    ),
];

/// The measured backstop tier: unkeyed text where no schema or key name can
/// decide — raw token runs, provider prefixes, JWT pieces, URL userinfo,
/// PEM blocks. The corpus and the run properties measure this tier, and the
/// documented strict-name/word residual above is its whole residual.
const BACKSTOP_REDACT: &[(&str, &str)] = &[
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
    // A webhook path dies inside the host — the `.`-evidenced run keeps its
    // name-shaped prefix (`com`) and kills the segments no human would name.
    (
        "https://hooks.example.com/AbCdEf1234567890XYZabcd/x9yzz",
        "https://hooks.example.com[redacted]",
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
    // …and it still dies whole as a `/`-run's first segment: the
    // strict-name keep must never carry a hex run at any length
    // (review N-A).
    (
        concat!(
            "fetch ",
            "0123456789abcdef0123456789abcdef",
            "/Report-V2 failed"
        ),
        "fetch [redacted] failed",
    ),
    (
        concat!(
            "{\"error\":\"",
            "0123456789abcdef0123456789abcdef01234567",
            "/Build_1A\"}"
        ),
        "{\"error\":\"[redacted]\"}",
    ),
    // A ≥32 mixed base64url run is a token even beside a name-shaped
    // `/`-segment — the strict-name keep is bounded, never this long
    // (review N-A; proptest seed).
    (
        "run a0_Aaa0Aaa-0-A_aA-a_aaa_AA0_0AaA/Run-1A ok",
        "run [redacted] ok",
    ),
    // `_`-wrapping is how secrets travel in Markdown italics: a leading `_`
    // is a boundary when it opens the token (`_AKIA…_`, `_glpat-…_`,
    // `_<32hex>_`), never when it joins two words (`api_key`). The N-J skip
    // must not swallow the position after a leading `_` (review round 5).
    ("key _AKIAIOSFODNN7EXAMPLE_ used", "key _[redacted] used"),
    ("x _glpat-AbCdEfGhIjKlMnOpQrSt_ y", "x _[redacted] y"),
    ("x _0123456789abcdef0123456789abcdef_ y", "x _[redacted]_ y"),
    // …and inside an escaped-quote context, where the `_` follows `\"`.
    (
        concat!("{\"msg\":\"x _", "AKIA", "IOSFODNN7EXAMPLE_ y\"}"),
        "{\"msg\":\"x _[redacted] y\"}",
    ),
    // A std-base64 token may LEAD with `/` — 1 in 64 unpadded 24-byte draws:
    // `/` must open a run the way `+` does, or the 31-char tail survives
    // (review round 5, the flaky N-B seed was a real leak).
    // The token is exactly 32 — the interior run is 31, so only the
    // `/`-opening fix catches it.
    ("x /AbCdEf1234567890aBcDeF123456789 y", "x [redacted] y"),
    // A `%25`-nested escape deeper than DECODE_PASSES fails closed on the
    // WHOLE field (review round 6, A1): the hidden byte can be the key's
    // own separator, so the text beside a residual span is where the
    // credential sits — `password[redacted]hunter2` was the regression.
    ("x %2525252541 y", "[redacted]"),
    ("key password%252525253Ahunter2 end", "[redacted]"),
    ("key passw%25252525256Frd=hunter2 end", "[redacted]"),
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

/// The keyed tier is 100% *by construction*: every row is a
/// `CREDENTIAL_KEYS` name deciding — a failure here is a broken closed
/// rule, never a measured miss.
#[test]
fn keyed_redact_is_100_percent() {
    for (input, want) in KEYED_REDACT {
        assert_eq!(
            redact(input).as_deref(),
            Some(*want),
            "KEYED_REDACT: {input}"
        );
    }
}

/// The backstop tier is measured: exact equality on every corpus row, with
/// the strict-name/word residual in the header as the whole residual.
#[test]
fn backstop_redact_is_100_percent() {
    for (input, want) in BACKSTOP_REDACT {
        assert_eq!(
            redact(input).as_deref(),
            Some(*want),
            "BACKSTOP_REDACT: {input}"
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

/// REDACTION-POLICY: every wire field has a declared role — the gate a
/// new field must pass before it can store or show anything. A name
/// dropped from `FIELD_ROLES` fails here even though `only()` would
/// still refuse it at runtime; a row naming a field that does not exist
/// is a stale row.
#[expect(
    clippy::too_many_lines,
    reason = "an exhaustive destructure names every wire field"
)]
#[test]
fn field_roles_cover_every_wire_field() {
    use aiops_domain::policy::{FIELD_ROLES, FieldRole};
    use aiops_domain::validate::OPTIONAL_WIRE_FIELDS;
    use aiops_domain::wire::{WireCost, WireEvent, WireSource, WireUsage};

    // The whole wire surface, destructured field-by-field — `..` is
    // banned (review round 6, B3): a new field on any of these structs
    // stops THIS test compiling until it is constructed, bound, named in
    // `live` and given a role.
    let wire = WireEvent {
        event_id: String::new(),
        event_type: String::new(),
        operation_id: String::new(),
        seq: 0,
        op_seq: 0,
        event_time: String::new(),
        name: Some(String::new()),
        trace_id: Some(String::new()),
        request_id: Some(String::new()),
        release: Some(String::new()),
        deployment: Some(String::new()),
        step_id: Some(String::new()),
        parent_step_id: Some(String::new()),
        kind: Some(String::new()),
        attempt: Some(1),
        model: Some(String::new()),
        prompt_version: Some(String::new()),
        config_version: Some(String::new()),
        external_effect: Some(true),
        outcome: Some(String::new()),
        failure_class: Some(String::new()),
        uncertain_reason: Some(String::new()),
        error_code: Some(String::new()),
        error_message: Some(String::new()),
        usage: Some(WireUsage {
            input_tokens: 1,
            output_tokens: 1,
        }),
        cost_estimate: Some(WireCost {
            micros: 1,
            currency: "USD".into(),
            basis: String::new(),
        }),
        events_emitted: Some(1),
        dropped: Some(1),
        reason: Some(String::new()),
        first_dropped_at: Some(String::new()),
        last_dropped_at: Some(String::new()),
        attributes: Some(serde_json::Map::new()),
        truncated: Some(1),
    };
    let WireEvent {
        event_id: _,
        event_type: _,
        operation_id: _,
        seq: _,
        op_seq: _,
        event_time: _,
        name,
        trace_id,
        request_id,
        release,
        deployment,
        step_id,
        parent_step_id,
        kind,
        attempt,
        model,
        prompt_version,
        config_version,
        external_effect,
        outcome,
        failure_class,
        uncertain_reason,
        error_code,
        error_message,
        usage,
        cost_estimate,
        events_emitted,
        dropped,
        reason,
        first_dropped_at,
        last_dropped_at,
        attributes,
        truncated,
    } = wire;
    let WireSource {
        instance_id: _,
        service: _,
        sdk: _,
    } = WireSource {
        instance_id: String::new(),
        service: String::new(),
        sdk: String::new(),
    };

    // The live field set is BUILT from the bindings — `live` can only
    // name fields the struct really carries.
    let mut live: Vec<&str> = vec![
        "event_id",
        "operation_id",
        "seq",
        "op_seq",
        "event_type",
        "event_time",
        "source.instance_id",
        "source.service",
        "source.sdk",
        "cost_estimate.basis",
        "cost_estimate.currency",
        "cost_estimate.micros",
    ];
    for (present, field) in [
        (name.is_some(), "name"),
        (trace_id.is_some(), "trace_id"),
        (request_id.is_some(), "request_id"),
        (release.is_some(), "release"),
        (deployment.is_some(), "deployment"),
        (step_id.is_some(), "step_id"),
        (parent_step_id.is_some(), "parent_step_id"),
        (kind.is_some(), "kind"),
        (attempt.is_some(), "attempt"),
        (model.is_some(), "model"),
        (prompt_version.is_some(), "prompt_version"),
        (config_version.is_some(), "config_version"),
        (external_effect.is_some(), "external_effect"),
        (outcome.is_some(), "outcome"),
        (failure_class.is_some(), "failure_class"),
        (uncertain_reason.is_some(), "uncertain_reason"),
        (error_code.is_some(), "error_code"),
        (error_message.is_some(), "error_message"),
        (usage.is_some(), "usage"),
        (cost_estimate.is_some(), "cost_estimate"),
        (events_emitted.is_some(), "events_emitted"),
        (dropped.is_some(), "dropped"),
        (reason.is_some(), "reason"),
        (first_dropped_at.is_some(), "first_dropped_at"),
        (last_dropped_at.is_some(), "last_dropped_at"),
        (attributes.is_some(), "attributes"),
        (truncated.is_some(), "truncated"),
    ] {
        if present {
            live.push(field);
        }
    }
    for field in OPTIONAL_WIRE_FIELDS {
        assert!(
            live.contains(field),
            "{field} is an optional wire field but never joined the live set"
        );
        assert!(
            FIELD_ROLES.iter().any(|(name, _)| name == field),
            "{field} has no field role — an unclassified field must not store"
        );
    }
    for (name, _) in FIELD_ROLES {
        assert!(
            live.contains(name),
            "role {name} names no live field — a stale row"
        );
    }
    // The two heuristic roles stay exactly where the policy promises:
    // `error_message` is the only free text, `attributes` the only keyed
    // container — everything else is a closed rule.
    assert_eq!(
        FIELD_ROLES
            .iter()
            .filter(|(_, r)| *r == FieldRole::FreeText)
            .map(|(n, _)| *n)
            .collect::<Vec<_>>(),
        ["error_message"]
    );
    assert_eq!(
        FIELD_ROLES
            .iter()
            .filter(|(_, r)| *r == FieldRole::KeyedValue)
            .map(|(n, _)| *n)
            .collect::<Vec<_>>(),
        ["attributes"]
    );
}

/// `looks_like_secret` names the fields an emitter should never send: a
/// key:value pair is not an identifier, and a bare long hex run is a
/// credential shape in every field but `release`/`request_id` (review
/// round 6, A4). Model ids and service names are not credentials.
#[test]
fn identifier_fields() {
    for input in ["password:x", "token=a1b2c3d4"] {
        assert!(looks_like_secret(input, "name"), "secret: {input}");
    }
    // A bare ≥32-hex run refuses everywhere except the two fields
    // declared to carry it (round 6, A4).
    let hex32 = "0123456789abcdef0123456789abcdef";
    for field in [
        "name",
        "deployment",
        "model",
        "error_code",
        "source.service",
        "cost_estimate.basis",
        "trace_id",
    ] {
        assert!(
            looks_like_secret(hex32, field),
            "{field} kept a bare hex credential"
        );
    }
    for field in ["release", "request_id"] {
        assert!(
            !looks_like_secret(hex32, field),
            "{field} refused its declared hex shape"
        );
    }
    for input in [
        "mistralai/Mistral-7B-Instruct-v0.3",
        "auth:login",
        "token:expired",
        "session:timeout",
        "stripe:charge:failed",
        "run-2026-11-20",
        "collector-01",
        "code:ab12cd",
        "status_code:404",
        "business_key:bk-1",
        "cache_key:user:42",
    ] {
        assert!(!looks_like_secret(input, "name"), "keep: {input}");
    }
    // …while the new credential names refuse (round 6, B1, A3).
    for input in [
        "stripe_key:x",
        "openai_key:x",
        "privatekey:x",
        "secretkey:x",
        "accesskey:x",
        "db_pass:x",
        "pgpass:x",
        "creds:x",
        "hmac_key:x",
        "passwordhash:x",
        "auth_code:x",
        "verification.code:x",
        "otp_code:x",
        "mfa.code:x",
        "sms_code:x",
        "reset.code:x",
        "recovery_code:x",
    ] {
        assert!(
            looks_like_secret(input, "name"),
            "credential-named: {input}"
        );
    }
}

/// Review N-E: a credential-keyed value is never the identifier field's
/// business — `password:hunter2` and a `token:`-prefixed hex run must
/// refuse the field, not be stored verbatim. `opt_ident` is private; the
/// test goes through `validate_source`, which runs `service` through it.
#[test]
fn credential_keyed_identifier_fields_refuse() {
    use aiops_domain::limits::Limits;
    use aiops_domain::validate::validate_source;
    use aiops_domain::wire::WireSource;

    let instance_id = concat!("0123456789abcdef", "0123456789abcdef").to_string();
    for (service, why) in [
        ("password:hunter2", "a credential wearing a key's name"),
        ("password:123456", "the same, digits"),
        // A PIN-shaped tail is not a port: digit pieces keep only under a key
        // whose last part is not itself a credential (`session-api:8080`),
        // never `password:4821` or a multi-piece `1234-5678` (review N-E
        // round 5).
        ("password:1234-5678", "a two-piece PIN tail"),
        ("password:4821", "a four-digit PIN tail"),
        ("token:12345", "a five-digit PIN tail"),
        ("pin:1234", "pin is a credential key name"),
        ("otp:123456", "otp is a credential key name"),
        ("passcode:998877", "passcode is a credential key name"),
        (
            "auth_code:ab12cd",
            "qualified code is a credential key name",
        ),
        ("verification.code:ab12cd", "qualified code again"),
        ("mfa:totp-x", "mfa is a credential key name"),
        (
            "auth:hunter2",
            "auth is no longer exempt for non-state tails",
        ),
        (
            concat!("token:", "0123456789abcdef0123456789abcdef"),
            "a 32-hex tail under a credential key",
        ),
        // Round 6, B1 — the namespace gaps the opt-in path admitted.
        ("stripe_key:x", "a *key part names a credential"),
        ("privatekey:x", "a *key part names a credential"),
        ("hmac_key:x", "a *key part names a credential"),
        ("db_pass:x", "pass is a credential part"),
        ("pgpass:x", "a pass suffix names a credential"),
        ("creds:x", "a cred* part names a credential"),
        // Round 6, A4 — a bare ≥32-hex run refuses outside
        // `release`/`request_id`; `source.service` is not exempt.
        (
            concat!("0123456789abcdef", "0123456789abcdef"),
            "a 32-hex credential shape in service",
        ),
        (
            concat!("0123456789abcdef", "0123456789abcdef01234567"),
            "a 40-hex credential shape in service",
        ),
        (
            "76fc524aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "a 40-hex credential shape in service",
        ),
    ] {
        let source = WireSource {
            instance_id: instance_id.clone(),
            service: service.to_string(),
            sdk: "aiops-sdk-test/0".to_string(),
        };
        assert!(
            validate_source(&source, &Limits::default()).is_err(),
            "{why} kept: {service}"
        );
    }
    // Identifiers keep: state tails, a port tail, a bare `code` key, and
    // the safe-list `*key` names (rounds 5-6).
    for service in [
        "token:expired",
        "session:timeout",
        "session-api:8080",
        "auth:login",
        "code:ab12cd",
        "status_code:404",
        "zip_code:90210",
        "business_key:bk-1",
        "cache_key:user:42",
    ] {
        let source = WireSource {
            instance_id: instance_id.clone(),
            service: service.to_string(),
            sdk: "aiops-sdk-test/0".to_string(),
        };
        assert!(
            validate_source(&source, &Limits::default()).is_ok(),
            "namespaced identifier refused: {service}"
        );
    }
}

/// The five contexts the property runs in (review R2, N-A): after a
/// space, after `host/`, after `//`, inside a hyphenated path, and as a
/// `/`-run's first segment followed by name text.
const CONTEXTS: &[&str] = &[
    "key {T} used",
    "GET https://h.io/{T} 200",
    "x //{T} y",
    "GET https://h.io/prefix-{T}-suffix 200",
    "run {T}/Run-1A ok",
];

/// Mirrors `policy.rs`'s keep rule: when a `/`-joined run dies, only the
/// FIRST segment may survive it — and only when that segment is itself a
/// strict name: no uppercase, every `-`/`_` piece under 6 chars, or a
/// dotted version — and bounded: under 16 bytes, never a ≥32 hex run
/// (review N-A). (A run that is name-shaped end to end is kept whole and
/// `redact` returns `None`, so it never reaches the window loop.) The
/// measured residual this file's header records.
fn strict_segment(s: &[u8]) -> bool {
    !(s.is_empty()
        || s.len() >= 16
        || (s.len() >= 32
            && s.iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))))
        && s.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && (!s.iter().any(u8::is_ascii_uppercase)
            || s.split(|b| matches!(b, b'-' | b'_'))
                .all(|p| !p.is_empty() && p.len() < 6)
            || (s.contains(&b'.')
                && s.split(|b| *b == b'.')
                    .all(|p| !p.is_empty() && p.len() <= 6)))
}

/// Mirrors `policy.rs::name_piece` — kept in step so the `None` check
/// below inspects the keep rather than trusting it.
fn mirror_name_piece(p: &[u8]) -> bool {
    if !p.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'.') {
        return false;
    }
    if p.len() < 6 || p.iter().all(u8::is_ascii_lowercase) {
        return true;
    }
    if p.iter().all(u8::is_ascii_digit) {
        return p.len() <= 13;
    }
    (p.iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && p.len() <= 13)
        || (p.contains(&b'.') && p.iter().filter(|b| b.is_ascii_uppercase()).count() <= 2)
        || (p.len() <= 16
            && p.iter().filter(|b| b.is_ascii_uppercase()).count() <= 2
            && p.iter().filter(|b| b.is_ascii_digit()).count() <= 2)
}

fn mirror_name_segment(s: &[u8]) -> bool {
    s.len() < 6 && s.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'.')
        || s.split(|b| matches!(b, b'-' | b'_'))
            .all(|p| !p.is_empty() && mirror_name_piece(p))
}

/// The maximal `[alnum+/=_-]` span holding `pos`, trimmed of leading
/// non-openers (`=` joins but cannot open a run; `/` can — a std-base64
/// token may lead with it, review round 5) — the run `match_secret`
/// would analyze at the token's position.
fn mirror_run(input: &str, pos: usize) -> &[u8] {
    let b = input.as_bytes();
    let is_run = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=' | b'_' | b'-');
    let is_open = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+' | b'/');
    let mut lo = pos;
    while lo > 0 && is_run(b[lo - 1]) {
        lo -= 1;
    }
    while lo < pos && !is_open(b[lo]) {
        lo += 1;
    }
    let mut hi = pos;
    while hi < b.len() && is_run(b[hi]) {
        hi += 1;
    }
    &b[lo..hi]
}

/// Mirrors the whole-run keep: a ≥32 mixed-case run containing `/` is a
/// path, model id or route — never a credential — only when every
/// `/`-segment is name-shaped AND some `-`/`_` piece is a real word
/// (≥4 all-lowercase chars: `mistralai`, `services`; `a0`/`AA0` are not
/// words). Anything else returning `None` is the leak review N-B named.
fn mirror_kept_whole(run: &[u8]) -> bool {
    let segs: Vec<&[u8]> = run.split(|b| *b == b'/').collect();
    run.len() >= 32
        && run.contains(&b'/')
        && run.iter().any(u8::is_ascii_uppercase)
        && run.iter().any(u8::is_ascii_lowercase)
        && run.iter().any(u8::is_ascii_digit)
        && segs.iter().all(|s| mirror_name_segment(s))
        && segs.iter().any(|s| {
            s.split(|b| matches!(b, b'-' | b'_'))
                .any(|p| p.len() >= 4 && p.iter().all(u8::is_ascii_lowercase))
        })
}

fn assert_no_secret_bytes(token: &str) -> Result<(), TestCaseError> {
    for ctx in CONTEXTS {
        let input = ctx.replace("{T}", token);
        // `None` means nothing was redacted — the token survived WHOLE,
        // the worst leak there is (review N-B: `continue` hid exactly
        // this). A whole-keep is allowed ONLY as the documented
        // name-shaped path residual, which the mirror now inspects.
        let Some(out) = redact(&input) else {
            let pos = ctx.find("{T}").unwrap_or(0);
            let run = mirror_run(&input, pos);
            prop_assert!(
                mirror_kept_whole(run),
                "{token:?} survived whole in {ctx:?} (run {run:?} is no path)"
            );
            continue;
        };
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

    /// And the lowercase-hex alphabet — md5/SHA/GitLab-runner shaped —
    /// in the same contexts, `{T}/Run-1A` included: a hex run must never
    /// ride the strict-name keep out of a dying `/`-run (review N-A).
    #[test]
    fn random_hex_never_leaves_eight_bytes(token in "[0-9a-f]{32,88}") {
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
