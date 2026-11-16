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
    "AIza",
];

/// Keys whose `key=value` / `key: value` value is redacted (ASCII case-insensitive).
const SECRET_KEYS: &[&str] = &[
    "access_key",
    "api_key",
    "apikey",
    "authorization",
    "client_secret",
    "passwd",
    "password",
    "pwd",
    "secret",
    "token",
];

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
        let boundary = i == 0 || !is_word(bytes[i - 1]);
        if let Some((keep, end)) = boundary.then(|| match_secret(bytes, i)).flatten() {
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

/// At `i`, returns `(bytes to keep before the redaction, end of the secret)`.
fn match_secret(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    if rest.starts_with(b"-----BEGIN") {
        return Some((0, bytes.len()));
    }
    for prefix in SECRET_PREFIXES {
        if rest.starts_with(prefix.as_bytes()) {
            let run = token_run(&rest[prefix.len()..], b"_-");
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
    for key in SECRET_KEYS {
        if starts_with_ignore_case(rest, key) {
            let mut j = key.len();
            if rest.get(j).is_some_and(|b| is_word(*b)) {
                continue; // `tokens=`, `secretary:` are not the key
            }
            while rest
                .get(j)
                .is_some_and(|b| *b == b' ' || *b == b'"' || *b == b'\'')
            {
                j += 1;
            }
            if !rest.get(j).is_some_and(|b| *b == b'=' || *b == b':') {
                continue;
            }
            j += 1;
            while rest
                .get(j)
                .is_some_and(|b| *b == b' ' || *b == b'"' || *b == b'\'')
            {
                j += 1;
            }
            // `Authorization: Bearer <token>`: the value is the scheme and its token.
            let scheme = AUTH_SCHEMES
                .iter()
                .find(|s| starts_with_ignore_case(&rest[j..], s))
                .map_or(0, |s| s.len());
            let value = scheme
                + rest[j + scheme..]
                    .iter()
                    .take_while(|b| !b.is_ascii_whitespace() && !b"&;,\"'".contains(b))
                    .count();
            if value > 0 {
                return Some((j, i + j + value));
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
            ("key sk-abcdefghijklmnop1234 used", "key [redacted] used"),
            (
                "Authorization: Bearer abc.def-123456789",
                "Authorization: [redacted]",
            ),
            (
                "got bearer abcdefgh12345678 back",
                "got bearer [redacted] back",
            ),
            ("aws AKIAABCDEFGHIJKLMNOP", "aws [redacted]"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl end",
                "jwt [redacted] end",
            ),
            ("url?password=hunter2&x=1", "url?password=[redacted]&x=1"),
            ("api_key: \"zzz-top\"", "api_key: \"[redacted]\""),
            ("key -----BEGIN PRIVATE KEY----- abc", "key [redacted]"),
            ("ingest aiops_ik_AAAAAAAAAAAAAAAAAAAA", "ingest [redacted]"),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input).as_deref(), Some(want), "input: {input}");
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
