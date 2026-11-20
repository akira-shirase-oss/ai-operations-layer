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

/// Whether a key name (as written in text) names a secret.
fn is_secret_key(key: &[u8]) -> bool {
    let mut prev: Vec<u8> = Vec::new();
    for part in key_parts(key) {
        let part = part.as_slice();
        // `jsessionid`, `sessionid`, `PHPSESSID`: a session identifier is a bearer credential.
        if SECRET_PARTS.iter().any(|s| s.as_bytes() == part)
            || part.ends_with(b"sessionid")
            || part.ends_with(b"sessid")
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
fn url_password(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    if !rest.starts_with(b"://") {
        return None;
    }
    let seg = rest[3..]
        .iter()
        .take_while(|b| !b.is_ascii_whitespace() && !b"/?#".contains(b))
        .count();
    let userinfo = &rest[3..3 + seg];
    let at = userinfo.iter().rposition(|b| *b == b'@')?;
    let colon = userinfo[..at].iter().position(|b| *b == b':')?;
    (colon + 1 < at).then_some((3 + colon + 1, i + 3 + at))
}

/// Quote and escape bytes allowed around a key and its value (`"key"`, `\"key\"`).
fn is_quoting(b: u8) -> bool {
    matches!(b, b' ' | b'"' | b'\'' | b'\\')
}

/// After a secret key, the separator and value: `(bytes to keep, end of the value)`, both
/// relative to `after`. Separators: `=`, `:`, `=>` (Ruby), `%3D` (URL-encoded), each with
/// optional spaces, quotes and escapes around it. A quoted value runs to its closing quote;
/// an `Authorization` header's value is the rest of its line (review N5).
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
    let len = if last_part == b"authorization" {
        value.iter().take_while(|b| !b"\r\n".contains(b)).count()
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
    // lowercase hex (commit and content hashes) and UUIDs are not.
    if rest.first().is_some_and(u8::is_ascii_alphanumeric) {
        let run = token_run(rest, b"");
        let token = &rest[..run];
        if run >= 32
            && token.iter().any(u8::is_ascii_uppercase)
            && token.iter().any(u8::is_ascii_lowercase)
            && token.iter().filter(|b| b.is_ascii_digit()).count() >= 2
        {
            return Some((0, i + run));
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
