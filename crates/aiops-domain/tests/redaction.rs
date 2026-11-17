//! Review finding 12: secret-shaped strings are redacted wherever they appear in a free-text
//! field, across the common shapes, and ordinary operational text is left alone.

use aiops_domain::policy::redact;
use proptest::prelude::*;

/// The review's seven error messages (only the first was redacted before the fix).
#[test]
fn the_reviewed_shapes_are_all_redacted() {
    let cases = [
        (
            "oauth failed: access_token=ya29.a0AfH6SMBx3kLq9zZ",
            "ya29.a0AfH6SMBx3kLq9zZ",
        ),
        ("refresh_token=1//0gLz8Qw7RkT2xVb", "1//0gLz8Qw7RkT2xVb"),
        (
            "upstream said X-Api-Key: 9f8e7d6c5b4a3928 is invalid",
            "9f8e7d6c5b4a3928",
        ),
        (
            "dial postgres://admin:Sup3rS3cret@db.internal:5432/app failed",
            "Sup3rS3cret",
        ),
        (
            "private_key=MIIEvQIBADANBgkqhkiG9w0BAQEFAASC",
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC",
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
}

#[test]
fn operational_text_is_left_alone() {
    for text in [
        "publisher answered 503 after 2.1s",
        "tokens=42 secretary: Ana author=Bo",
        "retrying attempt 2 of 3 for step generate",
        "commit 3fb3c23a9d1e4f5b6c7d8e9f0a1b2c3d4e5f6a7b",
        "request 0123e4567-e89b-12d3-a456-426614174000 timed out",
        "GET https://example.com/articles/42 returned 404",
        "cache key=article:42 missed",
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
];

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, .. ProptestConfig::default() })]

    /// Any sensitive key, any common separator and quoting, any secret value, anywhere in
    /// a message: the value never survives.
    #[test]
    fn a_value_after_a_sensitive_key_never_survives(
        key in prop::sample::select(KEYS),
        sep in prop::sample::select(&["=", ": ", ":", " = ", "=\"", ": '"][..]),
        value in "[A-Za-z0-9_\\-./+]{6,40}",
        before in "[a-z ]{0,20}",
        after in "[a-z ]{0,20}",
    ) {
        let text = format!("{before} {key}{sep}{value} {after}");
        let out = redact(&text).unwrap_or_else(|| text.clone());
        prop_assert!(!out.contains(&value), "{text:?} → {out:?}");
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
}
