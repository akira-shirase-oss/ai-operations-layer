//! Review finding 12: secret-shaped strings are redacted wherever they appear in a free-text
//! field, across the common shapes, and ordinary operational text is left alone.

use aiops_domain::policy::redact;
use proptest::prelude::*;

/// The review's seven error messages (only the first was redacted before the fix).
#[test]
fn the_reviewed_shapes_are_all_redacted() {
    let cases = [
        (
            "refresh_token=REDACT_ME_0a1b2c3d4e5f",
            "REDACT_ME_0a1b2c3d4e5f",
        ),
        (
            "upstream said X-Api-Key: 9f8e7d6c5b4a3928 is invalid",
            "9f8e7d6c5b4a3928",
        ),
        (
            "dial postgres://admin:Sup3rS3cret@db.internal:5432/app failed",
            "Sup3rS3cret",
        ),
        (
            "private_key=REDACT_ME_0123456789abcdef",
            "REDACT_ME_0123456789abcdef",
        ),
        ("auth_token=QWERTY123456", "QWERTY123456"),
        ("client_secret: \"s3cr3t-v4lue\"", "s3cr3t-v4lue"),
        ("db_password=hunter2 rejected", "hunter2"),
        ("header Cookie: session=abc123def456ghi", "abc123def456ghi"),
        (
            "key AbCdEfGhIjKlMnOpQrStUvWxYz012345 leaked",
            "AbCdEfGhIjKlMnOpQrStUvWxYz012345",
        ),
    ];
    for (input, secret) in cases {
        let out = redact(input).unwrap_or_else(|| input.to_owned());
        assert!(!out.contains(secret), "{input:?} → {out:?}");
        assert!(out.contains("[redacted]"), "{input:?} → {out:?}");
    }
    // Assembled: the review's Google-access-token shape holds no token-shaped literal
    // in the source for a secret scanner to flag (same pattern as the hf_ case below).
    let g = concat!("ya29.", "a0AfH6SMBx", "3kLq9zZ");
    let input = format!("oauth failed: access_token={g}");
    let out = redact(&input).unwrap_or_else(|| input.clone());
    assert!(!out.contains(g), "{input:?} → {out:?}");
}

/// Review N5: shapes the second review found leaking (Ruby inspect, other Authorization
/// schemes, camelCase keys, escaped JSON, URL-encoded separators, quoted values with spaces,
/// SAS signatures, session IDs, more token prefixes).
#[test]
fn the_second_reviews_shapes_are_all_redacted() {
    let cases = [
        (r#"params: {"password"=>"Hunt3r-two"}"#, "Hunt3r-two"),
        (
            "Authorization: Token 0123456789abcdef0123456789abcdef01234567",
            "0123456789abcdef0123456789abcdef01234567",
        ),
        ("refresh failed accessToken=ya29Xq81LmZp", "ya29Xq81LmZp"),
        (r#"config clientSecret: "Zx9-wq.Lm2""#, "Zx9-wq.Lm2"),
        (r#"body {\"api_key\":\"Kq81Lm2Zp0\"}"#, "Kq81Lm2Zp0"),
        ("form password%3DHunt3rTwo%26user%3Dbo", "Hunt3rTwo"),
        (
            "GET https://acct.blob.core.windows.net/c/b?sv=2022-11-02&sig=Xq81Lm2Zp0aB%2Fcd%3D&se=1",
            "Xq81Lm2Zp0aB",
        ),
        (
            r#"password="correct horse battery staple" rejected"#,
            "battery staple",
        ),
        (
            "GET /app;jsessionid=A1B2C3D4E5F6A7B8 302",
            "A1B2C3D4E5F6A7B8",
        ),
    ];
    for (input, secret) in cases {
        let out = redact(input).unwrap_or_else(|| input.to_owned());
        assert!(!out.contains(secret), "{input:?} → {out:?}");
        assert!(out.contains("[redacted]"), "{input:?} → {out:?}");
    }
    // Assembled, like the hf_ case below: a webhook-secret prefix + run, with no
    // complete token shape in the source.
    let wh = concat!("whsec_", "Xq81Lm2Z", "p0aBcdEf");
    let input = format!("webhook {wh} rejected");
    let out = redact(&input).unwrap_or_else(|| input.clone());
    assert!(!out.contains(wh), "{input:?} → {out:?}");
    // A Hugging Face token, assembled at run time so the source holds no token-shaped
    // literal for secret scanners to flag.
    let body: String = ('a'..='z')
        .zip('A'..='Z')
        .flat_map(|(l, u)| [u, l])
        .take(34)
        .collect();
    let input = format!("hub said {}_{body}", "hf");
    let out = redact(&input).unwrap_or_else(|| input.clone());
    assert!(!out.contains(&body), "{input:?} → {out:?}");
}

#[test]
fn operational_text_is_left_alone() {
    for text in [
        "publisher answered 503 after 2.1s",
        "tokens=42 secretary: Ana author=Bo",
        "retrying attempt 2 of 3 for step generate",
        // An abbreviated commit id stays readable; a full 40-hex is now the
        // lowercase-hex credential shape the review demanded (N7) — a whole
        // SHA is the accepted cost of that coverage.
        "commit 3fb3c23a9d1e",
        "request 0123e4567-e89b-12d3-a456-426614174000 timed out",
        "GET https://example.com/articles/42 returned 404",
        "cache key=article:42 missed",
        "signed=true signal: ok sessions=3 tokenizer=bpe",
        "user Ana said \"hello there\" twice",
    ] {
        assert_eq!(redact(text), None, "{text:?}");
    }
}

const KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "auth_token",
    "id_token",
    "token",
    "api_key",
    "apikey",
    "x-api-key",
    "X-Api-Key",
    "private_key",
    "secret",
    "client_secret",
    "password",
    "db_password",
    "passwd",
    "Authorization",
    "authorization",
    "cookie",
    "session_token",
    "aws_secret_access_key",
    "accessToken",
    "clientSecret",
    "apiKey",
    "refreshToken",
    "sig",
    "signature",
    "jsessionid",
    "PHPSESSID",
];

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, .. ProptestConfig::default() })]

    /// Any sensitive key, any common separator and quoting, any secret value, anywhere in
    /// a message: the value never survives.
    #[test]
    fn a_value_after_a_sensitive_key_never_survives(
        key in prop::sample::select(KEYS),
        sep in prop::sample::select(&["=", ": ", ":", " = ", "=\"", ": '", "\"=>\"", "\\\":\\\"", "%3D", "%3d"][..]),
        value in "[A-Za-z0-9_\\-./+]{6,40}",
        before in "[a-z ]{0,20}",
        after in "[a-z ]{0,20}",
    ) {
        let text = format!("{before} {key}{sep}{value} {after}");
        let out = redact(&text).unwrap_or_else(|| text.clone());
        prop_assert!(!out.contains(&value), "{text:?} → {out:?}");
    }

    /// A quoted value after a sensitive key never survives, spaces and all (review N5).
    #[test]
    fn a_quoted_value_with_spaces_never_survives(
        key in prop::sample::select(KEYS),
        quote in prop::sample::select(&["\"", "'"][..]),
        words in prop::collection::vec("[A-Za-z0-9]{3,9}", 2..5),
    ) {
        let value = words.join(" ");
        let text = format!("failed {key}={quote}{value}{quote} later");
        let out = redact(&text).unwrap_or_else(|| text.clone());
        prop_assert!(!out.contains(&value), "{text:?} → {out:?}");
        let last = words.last().map(String::as_str).unwrap_or_default();
        prop_assert!(!out.contains(&format!(" {last}{quote}")), "{text:?} → {out:?}");
    }

    /// Credentials in a URL's userinfo never survive.
    #[test]
    fn url_passwords_never_survive(
        scheme in prop::sample::select(&["postgres", "mysql", "https", "redis", "amqp"][..]),
        user in "[a-z]{3,10}",
        password in "[A-Za-z0-9!$%*]{6,20}",
        host in "[a-z]{3,10}\\.[a-z]{2,5}",
    ) {
        let text = format!("connect {scheme}://{user}:{password}@{host}/db failed");
        let out = redact(&text).unwrap_or_else(|| text.clone());
        prop_assert!(!out.contains(&format!(":{password}@")), "{text:?} → {out:?}");
    }

    /// Ordinary words and numbers are never touched.
    #[test]
    fn plain_words_and_numbers_are_untouched(words in prop::collection::vec("[a-z]{1,9}|[0-9]{1,6}", 1..12)) {
        let text = words.join(" ");
        prop_assert_eq!(redact(&text), None, "{:?}", text);
    }

    /// N2: escapes, quotes and braces in any order after any key must never
    /// panic — a trailing backslash once made the JSON-object scan return
    /// len+1 and `redact` sliced past the end (CatchPanicLayer logging the
    /// unredacted input on the way to a 500).
    #[test]
    fn never_panics_on_any_key_or_punctuation(
        key in prop::sample::select(KEYS),
        tail in prop::collection::vec(prop::char::range('\u{20}', '\u{7e}'), 0..80),
        prefix in prop::sample::select(&["", "{", "{\"", "{\""][..]),
    ) {
        let tail: String = tail.into_iter().collect();
        let _ = redact(&format!("{key}: {prefix}{tail}"));
    }
}
