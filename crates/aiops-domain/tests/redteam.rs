// The privacy red-team (A4): adversarial secret shapes carried in the fields a
// host's errors actually echo — an upstream's reply embeds whatever the request
// held, so `error_message` is the first surface checked. Every case asserts the
// secret substring is GONE from the output, not merely that something matched.
//
// Every credential fixture is assembled from parts at compile time (concat!) or
// run time (format!), so no complete token shape appears in the source for a
// secret scanner — or push protection — to flag. The assembled strings are
// identical to the shapes a real leak would carry.
//
// What is knowingly NOT covered (pattern matching is partial by nature, said so
// in the result's counters): bare all-lowercase 40-hex secrets (a git SHA has
// the same shape — catching it would eat every commit hash in the log), and
// `pass` as a bare key name (ambiguous with counters like `pass=42`).

use aiops_domain::policy::redact;

/// Every secret in `secrets` must be absent from `redact(input)`'s output.
fn assert_gone(input: &str, secrets: &[&str]) {
    let out = redact(input).unwrap_or_else(|| panic!("nothing redacted in {input:?}"));
    for s in secrets {
        assert!(!out.contains(s), "secret {s:?} survived in {out:?}");
    }
}

#[test]
fn credential_prefixes_the_catalog_did_not_know() {
    // Each `secret` is assembled so no complete token shape exists in the source.
    let cases: [(String, &str); 18] = [
        // SendGrid — `SG.` and two dot-separated runs.
        {
            let s = concat!("SG.", "abc123XYZ_", "def-456.", "ghI789jKl_012");
            (format!("mail reply 401: {s} failed"), s)
        },
        // DigitalOcean PAT.
        {
            let s = concat!(
                "dop",
                "_v1_",
                "a1b2c3d4e5f6",
                "a1b2c3d4e5f6",
                "a1b2c3d4e5f6",
                "a1b2c3d4e5f6",
                "a1b2"
            );
            (format!("deploy failed: {s} refused"), s)
        },
        // npm granular + classic tokens.
        {
            let s = concat!("npm_", "aBcDeFgH1234", "567890aBcD", "eFgH12345678");
            (format!("registry 403 on {s}"), s)
        },
        // PyPI.
        {
            let s = concat!("pypi-", "AgEIcHlwaS5v", "cmcCJGFiY2RlZmdoMTIzNA");
            (format!("upload 403: {s}"), s)
        },
        // Square access + app secret.
        {
            let s = concat!("sq0atp-", "AbCdEfGhIjKl", "MnOpQrStUvWxYz12");
            (format!("charge failed {s}"), s)
        },
        {
            let s = concat!(
                "sq0csp-",
                "AbCdEfGhIjKlMnOp",
                "QrStUvWxYz1234",
                "567890aBcDeF"
            );
            (format!("oauth {s}"), s)
        },
        // Shopify admin/custom/private-app tokens.
        {
            let s = concat!("shpat_", "AbCdEfGhIjKl", "MnOpQrStUvWxYz12");
            (format!("storefront 401 {s}"), s)
        },
        {
            let s = concat!("shppa_", "AbCdEfGhIjKl", "MnOpQrStUvWxYz12");
            (format!("storefront 401 {s}"), s)
        },
        // Slack app-level token (xoxb/xoxp were known; xapp/xoxe were not).
        {
            let s = concat!("xapp", "-1-", "A0B1C2D3E4", "-a1b2c3d4", "e5f6a1b2c3d4");
            (format!("slack sock mode: {s}"), s)
        },
        {
            let s = concat!("xoxe", "-1-", "AbCdEfGh", "IjKlMnOp");
            (format!("slack rotation {s}"), s)
        },
        // Google OAuth access token (ya29. — distinct from the AIza API key).
        {
            let s = concat!("ya29.", "a0AfH6SMB_", "AbCdEfGhIjKl", "MnOpQrStU-VwXy");
            (format!("calendar 401 {s}"), s)
        },
        // Doppler service tokens across its prefixes.
        {
            let s = concat!("dp.", "pt.", "AbCdEfGhIjKl", "MnOpQrStUvWxYz1234");
            (format!("secret read failed {s}"), s)
        },
        {
            let s = concat!("dp.", "st.", "AbCdEfGhIjKl", "MnOpQrStUvWxYz1234");
            (format!("secret read failed {s}"), s)
        },
        {
            let s = concat!("dp.", "ct.", "AbCdEfGhIjKl", "MnOpQrStUvWxYz1234");
            (format!("secret read failed {s}"), s)
        },
        // Notion internal integration.
        {
            let s = concat!("ntn_", "AbCdEfGhIjKlMnOp", "QrStUvWxYz123456");
            (format!("notion 401 {s}"), s)
        },
        // Figma PAT.
        {
            let s = concat!("figd_", "AbCdEfGhIjKlMnOp", "QrStUvWxYz1234-56");
            (format!("figma 403 {s}"), s)
        },
        // Anthropic-style keys ride the generic sk- prefix.
        {
            let s = concat!("sk-", "ant-api03-", "AbCdEfGhIjKl", "MnOpQrStUvWxYz12");
            (format!("model error {s}"), s)
        },
        // OpenAI project keys ride it too.
        {
            let s = concat!("sk-", "proj-", "AbCdEfGhIjKl", "MnOpQrStUvWxYz12");
            (format!("model error {s}"), s)
        },
    ];
    for (input, secret) in cases {
        assert_gone(&input, &[secret]);
    }
}

#[test]
fn the_error_field_is_where_they_land() {
    // The shapes an upstream error actually echoes back: the request's own
    // Authorization header, a connection string, a JSON body fragment.
    let basic = concat!("dXNlcj", "pwYXNzd29yZA", "==");
    assert_gone(
        &format!("upstream answered 401 'Authorization: Basic {basic}' for /v2/publish"),
        &[basic],
    );
    let pw = concat!("s3cr3", "t-pw");
    assert_gone(
        &format!("connect failed redis://worker:{pw}@cache.internal:6379/0 after 3 attempts"),
        &[pw],
    );
    let key = concat!("AbCdEfGh", "12345678");
    assert_gone(
        &format!("tool replied {{\"error\":\"bad key\",\"api_key\":\"{key}\"}}"),
        &[key],
    );
    let sess = concat!("AbCdEfGhIjKl", "MnOp123456");
    assert_gone(&format!("set-cookie replay: sessionid={sess}"), &[sess]);
}

#[test]
fn regression_the_covered_shapes_still_hold() {
    let sk = concat!("sk-", "abcdefghijklmnop", "1234");
    assert_gone(&format!("key {sk} used"), &[sk]);
    let akia = concat!("AKIA", "ABCDEFGH", "IJKLMNOP");
    assert_gone(&format!("aws {akia} refused"), &[akia]);
    let jwt = concat!("eyJhbGciOiJIUzI1NiJ9", ".eyJzdWIiOiIxIn0", ".c2lnbmF0dXJl");
    assert_gone(&format!("jwt {jwt} end"), &[jwt]);
    assert_gone("url?password=hunter2&x=1", &["hunter2"]);
    let pem = concat!("-----BEGIN ", "PRIVATE KEY-----");
    assert_gone(&format!("key {pem} abc"), &["BEGIN PRIVATE KEY"]);
    let ik = concat!("aiops_ik_", "AAAAAAAAAA", "AAAAAAAAAA");
    assert_gone(&format!("ingest {ik}"), &[ik]);
}

#[test]
fn the_partial_boundary_is_named_not_hidden() {
    // A bare lowercase-hex run is a commit SHA's shape AND a hex credential's —
    // indistinguishable. The re-review (N7) called the miss a leak, so the
    // boundary moved: ≥40 hex dies, the abbreviated SHA a log actually shows
    // stays readable. That is a documented boundary, not a miss.
    assert_eq!(
        redact("deploy sha 0123456789abcdef0123456789abcdef01234567").as_deref(),
        Some("deploy sha [redacted]")
    );
    assert_eq!(redact("deploy sha 0123456789abcdef"), None);
}
