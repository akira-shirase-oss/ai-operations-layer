//! Privacy by default (invariant 6, D-006, D-014): a default-deny attribute allowlist with
//! per-project opt-in, and secret-shape redaction for every free-text string.

/// Attribute keys schema v1 accepts by default. Each names operational metadata, never
/// content: no prompts, outputs, customer text, tokens or secrets.
pub const DEFAULT_ALLOWED_ATTRIBUTES: &[&str] = &[
    "business_key",
    "cache.hit",
    "content.kind",
    "content.length",
    "error.type",
    "http.method",
    "http.route",
    "http.status_code",
    "job.id",
    "job.runner",
    "provider",
    "provider.region",
    "queue.name",
    "retry.reason",
    "sampled",
    "tool.name",
    "validation.rule",
];

/// The replacement for anything secret-shaped.
pub const REDACTED: &str = "[redacted]";

/// How an attribute key is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTreatment {
    /// In the schema's default allowlist.
    Default,
    /// Allowed because the project opted in to this key (labeled in the console).
    OptIn,
    /// Withheld: neither key nor value is stored; the withholding is counted.
    Withheld,
}

/// A project's attribute policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub opt_in_keys: Vec<String>,
}

impl Policy {
    pub fn treat(&self, key: &str) -> KeyTreatment {
        if DEFAULT_ALLOWED_ATTRIBUTES.contains(&key) {
            KeyTreatment::Default
        } else if self.opt_in_keys.iter().any(|k| k == key) {
            KeyTreatment::OptIn
        } else {
            KeyTreatment::Withheld
        }
    }
}

/// Whether a key is in the default allowlist (the console labels the rest "opt-in").
pub fn is_default_attribute(key: &str) -> bool {
    DEFAULT_ALLOWED_ATTRIBUTES.contains(&key)
}

/// Prefixes of well-known credential formats; a token run after one is redacted.
const SECRET_PREFIXES: &[&str] = &[
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

/// Parts of a key name (split on `_`, `-`, `.`) that make its value a secret
/// (`access_token`, `X-Api-Key`, `db_password`, `Cookie`, …; review finding 12).
const SECRET_PARTS: &[&str] = &[
    "apikey",
    "auth",
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "credentials",
    "jwt",
    "passphrase",
    "passwd",
    "password",
    "pwd",
    "secret",
    "sessid",
    "sig",
    "signature",
    "token",
];

/// A `key` part is a secret only after one of these (`api_key`, `private-key`, …), so a
/// cache key or a sort key stays readable.
const KEY_QUALIFIERS: &[&str] = &[
    "access",
    "api",
    "client",
    "encryption",
    "master",
    "private",
    "secret",
    "signing",
    "ssh",
];

/// A key name's parts, lowercased: split on `_`, `-`, `.` and at camelCase humps
/// (`accessToken` → `access`, `token`; review N5).
fn key_parts(key: &[u8]) -> Vec<Vec<u8>> {
    let mut parts = vec![Vec::new()];
    for (n, &b) in key.iter().enumerate() {
        if matches!(b, b'_' | b'-' | b'.') {
            parts.push(Vec::new());
            continue;
        }
        let hump = b.is_ascii_uppercase() && n > 0 && key[n - 1].is_ascii_lowercase();
        if hump {
            parts.push(Vec::new());
        }
        if let Some(last) = parts.last_mut() {
            last.push(b.to_ascii_lowercase());
        }
    }
    parts
}

/// A key part ending in a credential word names a secret: `PGPASSWORD`,
/// `accessToken`, `ASP.NET_SessionId`, `connect.sid`, `laravel_session` (review S1-2).
/// Suffix, not substring: `tokens`, `secretary` and `sessionless` stay readable.
const SECRET_SUFFIXES: &[&[u8]] = &[
    b"passwd",
    b"password",
    b"pwd",
    b"secret",
    b"session",
    b"sessionid",
    b"sessid",
    b"sid",
    b"token",
];

/// Whether a key name (as written in text) names a secret.
fn is_secret_key(key: &[u8]) -> bool {
    let mut prev: Vec<u8> = Vec::new();
    for part in key_parts(key) {
        let part = part.as_slice();
        if SECRET_PARTS.iter().any(|s| s.as_bytes() == part)
            || SECRET_SUFFIXES.iter().any(|s| part.ends_with(s))
        {
            return true;
        }
        if part == b"key"
            && KEY_QUALIFIERS
                .iter()
                .any(|q| q.as_bytes() == prev.as_slice())
        {
            return true;
        }
        prev = part.to_vec();
    }
    false
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Schemes whose credential follows them (`Bearer abc…`).
const AUTH_SCHEMES: &[&str] = &["bearer ", "basic "];

const MIN_SECRET_RUN: usize = 8;

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn token_run(bytes: &[u8], extra: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || extra.contains(b))
        .count()
}

fn starts_with_ignore_case(hay: &[u8], needle: &str) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// Replaces secret-shaped substrings with [`REDACTED`]. Returns `None` when nothing
/// matched (the common case allocates nothing). Linear in the input for a fixed pattern
/// set; inputs are already capped (≤ 512 bytes).
pub fn redact(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0; // bytes of `text` already copied into `out`
    let mut i = 0;
    while i < bytes.len() {
        if let Some((keep, end)) = match_secret(bytes, i) {
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            // Every match starts and ends on an ASCII byte, so these are char boundaries.
            buf.push_str(&text[copied..i + keep]);
            buf.push_str(REDACTED);
            copied = end;
            i = end;
            continue;
        }
        i += 1;
    }
    out.map(|mut buf| {
        buf.push_str(&text[copied..]);
        buf
    })
}

/// URL userinfo: `scheme://user:password@host` keeps the user and hides the password.
/// The password may itself contain `/ ? #` (review S1-2), so the userinfo runs to
/// whitespace, not the first path delimiter; the LAST `@` still separates it from the
/// host.
fn url_password(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    if !rest.starts_with(b"://") {
        return None;
    }
    let seg = rest[3..]
        .iter()
        .take_while(|b| !b.is_ascii_whitespace())
        .count();
    let userinfo = &rest[3..3 + seg];
    let at = userinfo.iter().rposition(|b| *b == b'@')?;
    let colon = userinfo[..at].iter().position(|b| *b == b':')?;
    // The user part is kept verbatim, so it must LOOK like userinfo: a path,
    // query or quote byte in it means this '@' belongs to something else (an
    // email inside minified JSON — review N1) — decline so the keyed-secret
    // pass scans the text instead.
    if userinfo[..colon]
        .iter()
        .any(|b| matches!(b, b'/' | b'?' | b'#' | b'"' | b'\''))
    {
        return None;
    }
    (colon + 1 < at).then_some((3 + colon + 1, i + 3 + at))
}

/// Quote and escape bytes allowed around a key and its value (`"key"`, `\"key\"`).
fn is_quoting(b: u8) -> bool {
    matches!(b, b' ' | b'"' | b'\'' | b'\\')
}

/// A secret's value that starts a JSON object: to the matching `}` (braces inside
/// quoted strings don't count); unclosed runs to the line's end — hiding too much is
/// safer than leaving the tail (review S1-2).
fn json_object_len(value: &[u8]) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut k = 0;
    while k < value.len() {
        let b = value[k];
        if in_string {
            if b == b'\\' {
                k += 1;
            } else if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return k + 1;
            }
        } else if b == b'\r' || b == b'\n' {
            break;
        }
        k += 1;
    }
    // A string ending in a backslash pushes k one past the end; never report a
    // length the caller can't slice (review N2).
    k.min(value.len())
}

/// After a secret key, the separator and value: `(bytes to keep, end of the value)`, both
/// relative to `after`. Separators: `=`, `:`, `=>` (Ruby), `%3D` (URL-encoded), each with
/// optional spaces, quotes and escapes around it. A quoted value runs to its closing quote;
/// an `Authorization` or `Cookie` header's value is the rest of its line — `;`-joined
/// pairs are all credential (reviews N5, S1-2) — and a `{` starts a JSON object.
fn secret_value(key: &[u8], after: &[u8]) -> Option<(usize, usize)> {
    let mut j = after.iter().take_while(|b| is_quoting(**b)).count();
    let sep = if after[j..].starts_with(b"=>") {
        2
    } else if after.get(j).is_some_and(|b| *b == b'=' || *b == b':') {
        1
    } else if starts_with_ignore_case(&after[j..], "%3d") {
        3
    } else {
        return None;
    };
    j += sep;
    let opening = after[j..].iter().take_while(|b| is_quoting(**b)).count();
    let quote = after[j..j + opening]
        .iter()
        .rev()
        .find(|b| **b == b'"' || **b == b'\'')
        .copied();
    j += opening;
    let value = &after[j..];
    let last_part = key_parts(key).pop().unwrap_or_default();
    let len = if last_part == b"authorization" || last_part == b"cookie" {
        value.iter().take_while(|b| !b"\r\n".contains(b)).count()
    } else if value.first() == Some(&b'{') {
        json_object_len(value)
    } else if let Some(q) = quote {
        // To the closing quote (or an escaped one), spaces included.
        value
            .iter()
            .take_while(|b| **b != q && **b != b'\\' && !b"\r\n".contains(b))
            .count()
    } else {
        // `Bearer <token>`: the value is the scheme and its token.
        let scheme = AUTH_SCHEMES
            .iter()
            .find(|s| starts_with_ignore_case(value, s))
            .map_or(0, |s| s.len());
        scheme
            + value[scheme..]
                .iter()
                .take_while(|b| !b.is_ascii_whitespace() && !b"&;,\"'\\".contains(b))
                .count()
    };
    (len > 0).then_some((j, j + len))
}

/// The credential shapes an identifier is refused on (review N3): PEM
/// material, known token prefixes, bearer/basic headers and JWTs. The
/// free-text heuristics below (`key=`-style names, long mixed runs) stay
/// out — real model ids and namespaced codes are not secrets.
fn high_confidence(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    if rest.starts_with(b"-----BEGIN") {
        return Some((0, bytes.len()));
    }
    for prefix in SECRET_PREFIXES {
        if rest.starts_with(prefix.as_bytes()) {
            // `.` is in the run: SendGrid/Doppler/JWT-shaped keys dot their segments.
            let run = token_run(&rest[prefix.len()..], b"_-.");
            if run >= MIN_SECRET_RUN {
                return Some((0, i + prefix.len() + run));
            }
        }
    }
    for scheme in AUTH_SCHEMES {
        if starts_with_ignore_case(rest, scheme) {
            let run = token_run(&rest[scheme.len()..], b"_-+/=.~");
            if run >= MIN_SECRET_RUN {
                return Some((scheme.len(), i + scheme.len() + run));
            }
        }
    }
    if rest.starts_with(b"eyJ") {
        let run = token_run(rest, b"_-.");
        let dots = rest[..run].split(|b| *b == b'.').count() - 1;
        if run >= 20 && dots >= 2 {
            return Some((0, i + run));
        }
    }
    None
}

/// Whether `text` holds a high-confidence credential shape — the check
/// [`crate::validate`] runs on identifier fields (model ids, release names,
/// request ids). A model id is a long mixed run and `auth:login` a key-looking
/// pair; neither is a credential, so only these shapes refuse (review N3).
pub fn looks_like_secret(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|i| {
        url_password(bytes, i).is_some()
            || ((i == 0 || !is_word(bytes[i - 1])) && high_confidence(bytes, i).is_some())
    })
}

/// At `i`, returns `(bytes to keep before the redaction, end of the secret)`.
fn match_secret(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    // URL userinfo follows the scheme's letters, so it needs no word boundary.
    if let Some(found) = url_password(bytes, i) {
        return Some(found);
    }
    // Every other shape starts a word.
    if i > 0 && is_word(bytes[i - 1]) {
        return None;
    }
    if let Some(found) = high_confidence(bytes, i) {
        return Some(found);
    }
    // `key=value` / `key: value` where the key names a secret. The key must start here
    // (the caller checked the word boundary; a key byte before it would make it a suffix).
    if i == 0 || !is_key_byte(bytes[i - 1]) {
        let key_len = rest
            .iter()
            .take(64)
            .take_while(|b| is_key_byte(**b))
            .count();
        if key_len > 0
            && is_secret_key(&rest[..key_len])
            && let Some((keep, len)) = secret_value(&rest[..key_len], &rest[key_len..])
        {
            return Some((key_len + keep, i + key_len + len));
        }
    }
    // A long mixed-case alphanumeric run is a credential shape (API keys, signing keys);
    // lowercase hex (commit and content hashes) and UUIDs are not. Base64's `/ + =`
    // join the run — otherwise they split it into pieces under the limit (review
    // S1-2), which is also how a multi-segment webhook path stays one run.
    if rest.first().is_some_and(u8::is_ascii_alphanumeric) {
        let run = token_run(rest, b"+/=_-");
        let token = &rest[..run];
        if run >= 32
            && token.iter().any(u8::is_ascii_uppercase)
            && token.iter().any(u8::is_ascii_lowercase)
            && token.iter().filter(|b| b.is_ascii_digit()).count() >= 2
        {
            if !token.contains(&b'/') {
                return Some((0, i + run));
            }
            // A `/`-joined run is a path by default — model ids, file paths,
            // URL routes are long mixed runs too (review N4). It is only a
            // credential when at least two segments each look like a key
            // (≥6 chars with an uppercase and a digit: T0ABCDEF12,
            // B0CDEF1234). The redaction starts at the first of them so the
            // readable prefix — a domain, a directory, a namespace — survives.
            let mut qualified = 0;
            let mut at = None;
            let mut off = 0;
            for seg in token.split(|b| *b == b'/') {
                if seg.len() >= 6
                    && seg.iter().any(u8::is_ascii_uppercase)
                    && seg.iter().any(u8::is_ascii_digit)
                {
                    qualified += 1;
                    at.get_or_insert(off);
                }
                off += seg.len() + 1;
            }
            if qualified >= 2 {
                return Some((at.unwrap_or(0), i + run));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_ordinary_text_alone_without_allocating() {
        assert_eq!(redact("publisher answered 503 after 2.1s"), None);
        assert_eq!(redact("tokens=42 secretary: Ana"), None);
        assert_eq!(redact("sk-short"), None);
    }

    #[test]
    fn redacts_known_credential_shapes() {
        let cases = [
            (
                "Authorization: Bearer abc.def-123456789",
                "Authorization: [redacted]",
            ),
            (
                "got bearer abcdefgh12345678 back",
                "got bearer [redacted] back",
            ),
            ("url?password=hunter2&x=1", "url?password=[redacted]&x=1"),
            ("api_key: \"zzz-top\"", "api_key: \"[redacted]\""),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input).as_deref(), Some(want), "input: {input}");
        }
        // Assembled so the source holds no complete scanner-shaped token literal
        // (the same pattern redteam.rs uses).
        for (input, want) in [
            (
                format!("key {} used", concat!("sk-", "abcdefghijklmnop", "1234")),
                "key [redacted] used",
            ),
            (
                format!("aws {} refused", concat!("AKIA", "ABCDEFGH", "IJKLMNOP")),
                "aws [redacted] refused",
            ),
            (
                format!(
                    "jwt {} end",
                    concat!("eyJhbGciOiJIUzI1NiJ9", ".eyJzdWIiOiIxIn0", ".c2lnbmF0dXJl")
                ),
                "jwt [redacted] end",
            ),
            (
                format!("key {} abc", concat!("-----BEGIN ", "PRIVATE KEY-----")),
                "key [redacted]",
            ),
            (
                format!(
                    "ingest {}",
                    concat!("aiops_ik_", "AAAAAAAAAA", "AAAAAAAAAA")
                ),
                "ingest [redacted]",
            ),
        ] {
            assert_eq!(redact(&input).as_deref(), Some(want), "input: {input}");
        }
    }

    #[test]
    fn redacts_the_shapes_the_first_pass_missed() {
        // A URL password may itself contain / ? #: the userinfo runs to the last
        // @ before whitespace (white-hat review S1-2).
        assert_eq!(
            redact("dial redis://:Zk3/Ab+c=@cache:6379/0").as_deref(),
            Some("dial redis://:[redacted]@cache:6379/0")
        );
        for (input, want) in [
            ("PGPASSWORD=hunter2 ok", "PGPASSWORD=[redacted] ok"),
            ("accesstoken=hunter2", "accesstoken=[redacted]"),
            ("APITOKEN: hunter2", "APITOKEN: [redacted]"),
            (
                "ASP.NET_SessionId=abc123 x",
                "ASP.NET_SessionId=[redacted] x",
            ),
            ("connect.sid=abc123 x", "connect.sid=[redacted] x"),
            ("laravel_session=abc123 x", "laravel_session=[redacted] x"),
            ("_app_session=abc123 x", "_app_session=[redacted] x"),
            // A cookie line is all credential, to its line's end.
            (
                "Cookie: theme=dark; connect.sid=abc123",
                "Cookie: [redacted]",
            ),
            // A secret key's JSON object value is the whole object.
            (
                "got {\"secret\": {\"value\":\"hunter2\"}}",
                "got {\"secret\": [redacted]}",
            ),
        ] {
            assert_eq!(redact(input).as_deref(), Some(want), "input: {input}");
        }
        // Base64 carries / + and = padding; a path joined on / is one run
        // (webhook URLs; assembled so no scanner-shaped literal sits in source).
        assert_eq!(
            redact(&format!(
                "sig {} end",
                concat!(
                    "aB3xY9", "+Qw/ER", "TyuiopAS", "DFGHJKLz", "xcvbnm12", "34567890", "=="
                )
            ))
            .as_deref(),
            Some("sig [redacted] end")
        );
        assert_eq!(
            redact("GET https://hooks.example.com/services/T0ABCDEF12/B0CDEF1234/xyZab").as_deref(),
            Some("GET https://hooks.example.com/services/[redacted]")
        );
    }

    #[test]
    fn an_at_inside_json_is_not_userinfo_and_the_keyed_secret_still_dies() {
        // N1: the last '@' belongs to the email, so the implied "user part"
        // `a.io/token?client_secret=Zq81aa","user` is not userinfo (it holds
        // '/', '?', '"'). The url_password branch must decline so the keyed
        // pass sees client_secret — and the email stays, it is no credential.
        let text = r#"{"url":"https://a.io/token?client_secret=Zq81aa","user":"ana@x.io"}"#;
        let out = redact(text).unwrap_or_else(|| panic!("{text:?} must redact"));
        assert!(!out.contains("Zq81aa"), "{text:?} → {out:?}");
        assert!(
            out.contains("ana@x.io"),
            "the email is not a credential: {out:?}"
        );
        // A real userinfo password still dies, / ? # inside included (S1-2).
        assert_eq!(
            redact("dial redis://:Zk3/Ab+c=@cache:6379/0").as_deref(),
            Some("dial redis://:[redacted]@cache:6379/0")
        );
    }

    #[test]
    fn a_value_ending_in_an_escape_never_slices_past_the_end() {
        // N2: `{"…\` left json_object_len returning len+1 — redact then sliced
        // `text[copied..]` past the end and panicked (CatchPanicLayer logging
        // the unredacted input on the way out).
        let _ = redact("token: {\"\\");
        let _ = redact("secret={\"a\":\"b\\");
        let _ = redact("password: \"\\");
    }

    #[test]
    fn paths_and_model_ids_in_free_text_are_not_secrets() {
        // N4: a `/`-joined run is not a credential by shape alone — paths,
        // model ids and URL paths are long mixed runs too. A run that
        // contains `/` counts only when at least two segments each look
        // like a key (≥6 chars, an uppercase letter and a digit), and the
        // redaction starts at the first of them so the readable prefix —
        // a domain, a directory, a model namespace — survives.
        for text in [
            "model mistralai/Mistral-7B-Instruct-v0.3 refused the prompt",
            "model mistralai/Mixtral-8x22B-Instruct-v0p1 refused the prompt",
            "read /var/log/aiops-collector/RunId42-Error/trace-2026.log tail",
            "GET /api/v2/Operations/RunId42/status 200",
            "release/2024-11-20/publish ok",
        ] {
            assert_eq!(redact(text), None, "{text:?} is operational text");
        }
        // A webhook path still dies — two key-looking segments is the shape —
        // and the domain + path prefix survive to explain where it went.
        assert_eq!(
            redact("GET https://hooks.example.com/services/T0ABCDEF12/B0CDEF1234/xyZab").as_deref(),
            Some("GET https://hooks.example.com/services/[redacted]")
        );
        // Base64 `+`/`/`/`=` still keeps a bare blob one run (S1-2).
        assert_eq!(
            redact(&format!(
                "sig {} end",
                concat!(
                    "aB3xY9", "+Qw/ER", "TyuiopAS", "DFGHJKLz", "xcvbnm12", "34567890", "=="
                )
            ))
            .as_deref(),
            Some("sig [redacted] end")
        );
    }

    #[test]
    fn keeps_multibyte_text_intact_around_a_redaction() {
        assert_eq!(
            redact("café token=s3cr3t ünïcode").as_deref(),
            Some("café token=[redacted] ünïcode")
        );
    }

    #[test]
    fn default_deny_with_opt_in() {
        let policy = Policy {
            opt_in_keys: vec!["customer.tier".into()],
        };
        assert_eq!(policy.treat("http.route"), KeyTreatment::Default);
        assert_eq!(policy.treat("customer.tier"), KeyTreatment::OptIn);
        assert_eq!(policy.treat("prompt"), KeyTreatment::Withheld);
        assert_eq!(policy.treat("llm.output"), KeyTreatment::Withheld);
    }
}
