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
/// Percent-decoding passes at most — `%25` nests an escape inside an escape,
/// and each pass only shrinks the text.
const DECODE_PASSES: usize = 4;

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

/// Each decoded byte's `(offset, len)` in the raw text.
type RawMap = Vec<(usize, usize)>;

/// Percent-decoded view of `bytes`, plus each decoded byte's `(offset, len)`
/// in the raw text (`%XX` decodes to one byte spanning three). `None` when
/// there is nothing to decode — the common case allocates nothing.
fn percent_decode(bytes: &[u8]) -> Option<(Vec<u8>, RawMap)> {
    if !bytes.contains(&b'%') {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut raw = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(a), Some(b)) = (bytes.get(i + 1), bytes.get(i + 2))
            && a.is_ascii_hexdigit()
            && b.is_ascii_hexdigit()
        {
            decoded.push(hex_val(*a) << 4 | hex_val(*b));
            raw.push((i, 3));
            i += 3;
            changed = true;
        } else {
            decoded.push(bytes[i]);
            raw.push((i, 1));
            i += 1;
        }
    }
    changed.then_some((decoded, raw))
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// The `(start, end)` spans of every secret shape in `view` — one scan.
fn secret_spans(view: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < view.len() {
        if let Some((keep, end)) = match_secret(view, i) {
            spans.push((i + keep, end));
            // A userinfo match ends at `@` with `://` inside its kept
            // prefix — the user part rode the "keep the user" rule unscanned.
            // Rescan from just after the `://`: a credential in the user
            // slot (`ghp_…:x-oauth-basic@` is how real tokens travel) must
            // die. Every other shape skips past its end; the merge absorbs
            // the overlap with the password span (review N1 residual).
            let user = (view.get(end) == Some(&b'@'))
                .then(|| view[i..i + keep].windows(3).position(|w| w == b"://"))
                .flatten();
            i = match user {
                Some(p) => i + p + 3,
                None => end,
            };
            continue;
        }
        i += 1;
    }
    // The user-part rescan can push a span that starts BEFORE one already
    // pushed — the merge below walks forward only, so an unordered span is
    // absorbed and silently lost. Sort so every span is emitted.
    spans.sort_unstable();
    spans
}

/// Replaces secret-shaped substrings with [`REDACTED`]. Returns `None` when nothing
/// matched (the common case allocates nothing). Linear in the input for a fixed pattern
/// set; inputs are already capped (≤ 512 bytes).
pub fn redact(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut spans = secret_spans(bytes);
    // Percent-encoded text hides keys and separators from the raw scanner
    // (review N7): `%63%6C…` is `client…`, `%3A` the `:`. An escape can also
    // FAKE a byte — `%0C` reads as a form feed but is three plain bytes — so
    // the decoded view is scanned in ADDITION and its matches mapped back
    // onto raw spans; it never replaces the raw scan. `%25` decodes to `%`,
    // so one pass leaves `%3A` literal — the scan repeats until the view is
    // stable, bounded by DECODE_PASSES (review residual; each pass only
    // shrinks the text).
    if bytes.contains(&b'%') {
        // `view` is the current decoding; `map` tracks each view byte's
        // (offset, len) in the RAW text — the identity to start.
        let mut view = bytes.to_vec();
        let mut map: Vec<(usize, usize)> = (0..view.len()).map(|i| (i, 1)).collect();
        for _ in 0..DECODE_PASSES {
            let Some((decoded, m)) = percent_decode(&view) else {
                break;
            };
            // Compose the maps: decoded byte j spans view bytes o..o+len —
            // unwind through `map` to the raw extent covering them all.
            let composed: Vec<(usize, usize)> = m
                .iter()
                .map(|&(o, len)| {
                    let start = map[o].0;
                    (start, map[o + len - 1].0 + map[o + len - 1].1 - start)
                })
                .collect();
            for (start, end) in secret_spans(&decoded) {
                spans.push((composed[start].0, composed[end - 1].0 + composed[end - 1].1));
            }
            view = decoded;
            map = composed;
        }
        spans.sort_unstable();
        spans.dedup();
    }
    // Merge overlapping spans — a secret found on both views maps to the same
    // raw bytes — and emit one redaction each. Every span boundary is ASCII.
    let mut spans = spans.into_iter();
    let mut cur = spans.next()?;
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        if span.0 <= cur.1 {
            cur.1 = cur.1.max(span.1);
        } else {
            out.push_str(&text[copied..cur.0]);
            out.push_str(REDACTED);
            copied = cur.1;
            cur = span;
        }
    }
    out.push_str(&text[copied..cur.0]);
    out.push_str(REDACTED);
    out.push_str(&text[cur.1..]);
    Some(out)
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

/// A secret's value that starts a JSON container: to the matching close
/// (brackets inside quoted strings don't count, and `{`/`[` nest together);
/// unclosed runs to the line's end — hiding too much is safer than leaving
/// the tail (reviews S1-2, N7). The caller guarantees `{` or `[` at byte 0.
fn json_container_len(value: &[u8]) -> usize {
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
        } else if b == b'{' || b == b'[' {
            depth += 1;
        } else if b == b'}' || b == b']' {
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
    let run = &after[j..j + opening];
    let qpos = run.iter().rposition(|b| *b == b'"' || *b == b'\'');
    let quote = qpos.map(|p| run[p]);
    // `\"key\":\"value\"` — an escaped opening quote means the value's own
    // delimiter is `\"` too, so a backslash inside can only open the close.
    let quote_escaped = qpos.is_some_and(|p| p > 0 && run[p - 1] == b'\\');
    j += opening;
    let value = &after[j..];
    let last_part = key_parts(key).pop().unwrap_or_default();
    // The normal extent: a quoted value runs to its closing quote; an unquoted
    // one stops only at whitespace, a quote or `&` — `,` and `;` are part of a
    // credential, not separators (review N7c). Inside a real quote a `\x`
    // pair belongs to the value — `Zq8\"kLm` keeps going past the escape;
    // only an unescaped `q` ends it (review R3).
    let normal = if let Some(q) = quote {
        if quote_escaped {
            value
                .iter()
                .take_while(|b| **b != q && **b != b'\\' && !b"\r\n".contains(b))
                .count()
        } else {
            let mut n = 0;
            while n < value.len() {
                match value[n] {
                    b'\\' if n + 1 < value.len() => n += 2,
                    b'\r' | b'\n' => break,
                    b if b == q => break,
                    _ => n += 1,
                }
            }
            n
        }
    } else {
        // `Bearer <token>`: the value is the scheme and its token.
        let scheme = AUTH_SCHEMES
            .iter()
            .find(|s| starts_with_ignore_case(value, s))
            .map_or(0, |s| s.len());
        scheme
            + value[scheme..]
                .iter()
                .take_while(|b| !b.is_ascii_whitespace() && !matches!(*b, b'"' | b'\'' | b'&'))
                .count()
    };
    let len = if last_part == b"authorization" || last_part == b"cookie" {
        value.iter().take_while(|b| !b"\r\n".contains(b)).count()
    } else if value.first() == Some(&b'{') || value.first() == Some(&b'[') {
        // A container can close early while the credential runs on —
        // `"[ab]hunter2"`: the value reaches the farther of the two (NF2).
        json_container_len(value).max(normal)
    } else {
        normal
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
        // A whole JWT (header.payload.sig) or a piece still ≥30 — a split
        // line can cut the dots off a long `eyJ` run (review N7).
        if (run >= 20 && dots >= 2) || run >= 30 {
            return Some((0, i + run));
        }
    }
    None
}

/// Whether `text` holds a high-confidence credential shape — the check
/// [`crate::validate`] runs on identifier fields (model ids, release names,
/// request ids). A model id is a long mixed run and `auth:login` a key-looking
/// pair; neither is a credential, so only these shapes refuse (review N3).
/// A `key:value` pair whose key names a secret refuses too — `password:x` is
/// not an identifier (review N3 residual) — except the category keys `auth`
/// and `authorization`, and except a `:` pair whose tail is a plain
/// lowercase name: `token:expired` is an event, not a field.
pub fn looks_like_secret(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|i| {
        if url_password(bytes, i).is_some()
            || ((i == 0 || !is_word(bytes[i - 1])) && high_confidence(bytes, i).is_some())
        {
            return true;
        }
        // `name:value` / `name=value` where the name names a credential.
        if i > 0 && is_key_byte(bytes[i - 1]) {
            return false;
        }
        let rest = &bytes[i..];
        let key_len = rest
            .iter()
            .take(64)
            .take_while(|b| is_key_byte(**b))
            .count();
        if key_len == 0
            || !rest[key_len..]
                .first()
                .is_some_and(|b| matches!(b, b'=' | b':'))
        {
            return false;
        }
        let key = &rest[..key_len];
        if !is_secret_key(key)
            || key.eq_ignore_ascii_case(b"auth")
            || key.eq_ignore_ascii_case(b"authorization")
        {
            return false;
        }
        if rest[key_len] == b'=' {
            return true;
        }
        // `:`-namespaced names are identifiers too — `token:expired`,
        // `session:timeout`, `stripe:charge:failed`, `session-api:8080` —
        // so a `:` pair refuses only when its tail is not a plain name:
        // every piece a word or a number, nothing that spells like a
        // credential (`password:x` is a field, `token:expired` an event).
        !rest[key_len + 1..]
            .split(|b| matches!(b, b':' | b'.' | b'-' | b'_' | b'/'))
            .all(|p| {
                p.len() >= 2
                    && p.iter().all(u8::is_ascii_alphanumeric)
                    && !(p.iter().any(u8::is_ascii_uppercase) && p.iter().any(u8::is_ascii_digit))
            })
    })
}

/// At `i`, returns `(bytes to keep before the redaction, end of the secret)`.
fn match_secret(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &bytes[i..];
    // URL userinfo follows the scheme's letters, so it needs no word boundary.
    if let Some(found) = url_password(bytes, i) {
        return Some(found);
    }
    // Every other shape starts a word — except after a leading `_`: a
    // base64url token may open with one (`_AaA…`), and `_` joining two
    // words is still no boundary (`api_key`).
    if i > 0 && is_word(bytes[i - 1]) {
        let leading_underscore = bytes[i - 1] == b'_' && (i < 2 || !is_word(bytes[i - 2]));
        if !leading_underscore {
            return None;
        }
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
    // A long mixed-case alphanumeric run is a credential shape (API keys, signing
    // keys; one digit suffices — a JWT piece may carry few, review N7); UUIDs are
    // not. Base64's `/ + =` join the run — otherwise they split it into pieces
    // under the limit (review S1-2) — which is also how a webhook path stays one.
    // A run may open on `_` or `-` too: `_` is a word byte, so a base64url token
    // that leads with underscores (`____Aaa-…`) is invisible to any interior
    // scan position unless the run itself starts there (corpus property).
    if rest
        .first()
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'-'))
    {
        let run = token_run(rest, b"+/=_-");
        let token = &rest[..run];
        if run >= 32
            && token.iter().any(u8::is_ascii_uppercase)
            && token.iter().any(u8::is_ascii_lowercase)
            && token.iter().any(u8::is_ascii_digit)
            && !looks_like_uuid(token)
        {
            if !token.contains(&b'/') {
                return Some((0, i + run));
            }
            if let Some(dead) = slash_run_dead_offset(bytes, i, token) {
                return Some((dead, i + run));
            }
            // Every segment is name-shaped: a path, a model id, a route —
            // not a credential.
        }
        // A bare lowercase-hex run is a hash or a hex credential — at 32
        // (md5 length) and beyond the review calls the miss a leak (N7,
        // corpus rebalance). A run that continues as a word is a longer
        // identifier, not a hex string; a commit SHA is the accepted cost
        // of the coverage. A digest that SAYS it is one — `sha256:<64hex>`
        // — is a content identifier, not a credential (NF3).
        let hex_run = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(*b))
            .count();
        let said_sha256 = i >= 7 && bytes[i - 7..i].eq_ignore_ascii_case(b"sha256:");
        if hex_run >= 32 && !said_sha256 && !rest.get(hex_run).is_some_and(|b| is_word(*b)) {
            return Some((0, i + hex_run));
        }
    }
    None
}

/// A `/`-joined credential run at `i` with token text `token`: `Some(off)`
/// when the run dies — `off` bytes of it may keep — `None` when the run is
/// name-shaped end to end (a path, a model id, a route). A run stays
/// readable only with path evidence — a name-byte segment (`-`/`_`/`.`), or
/// the run sitting inside host or path text: behind a supported `/`
/// (`h.io/…`, one with bytes before it) or a `.` (`example.com/…`). A bare
/// `/` after whitespace is the credential's own leading slash, not a path
/// (review NF1).
fn slash_run_dead_offset(bytes: &[u8], i: usize, token: &[u8]) -> Option<usize> {
    let segments: Vec<&[u8]> = token.split(|b| *b == b'/').collect();
    let name_byte = |b: &u8| matches!(b, b'-' | b'_' | b'.');
    let supported_slash = i >= 2
        && bytes[i - 1] == b'/'
        && (is_word(bytes[i - 2]) || matches!(bytes[i - 2], b'.' | b':' | b'/'));
    let evidence = segments.iter().any(|s| s.iter().any(name_byte))
        || supported_slash
        || (i > 0 && bytes[i - 1] == b'.');
    if !evidence {
        return Some(0);
    }
    // Named like a path part: every `-`/`_`-split piece is name text —
    // alnum/dot bytes only (`+a` and `=x` are credential soup, never a
    // name), and then short, a word, a date/id, or a dotted version (a
    // leading capital is fine; interleaved caps and digit runs are how a
    // generated token spells, not a person). `-`/`_` are run members,
    // never name-proof — `Mistral-7B-Instruct-v0` and
    // `collector-2026-11-20` pass; `T0AB_CDEF12` and `B0CD-EF1234` do not
    // (review R1).
    if segments.iter().all(|s| name_segment(s)) {
        return None;
    }
    // The run dies: at most the FIRST segment may keep — and only when it
    // is itself a strict name, and only its own bytes: the `/` after it is
    // a content byte, so `aa0a0a0/` kept whole is already an 8-byte window
    // (the corpus property caught it). Two adjacent keeps would compose a
    // window across the `/` the same way (`aA/0AA0/`), and a mixed-case
    // keep beside a credential is never a name (review R2's measured
    // residual). The byte before the run matters the same way: a run byte
    // there is the token's own text, and `/aa00a0a` read whole is 8
    // bytes — so a keep is only readable behind a non-run boundary
    // (`h.io`'s `.`), never behind `/`, `+`, `=`, `_` or `-`.
    if segments.len() > 1
        && strict_name(segments[0])
        && (i == 0 || !matches!(bytes[i - 1], b'+' | b'/' | b'=' | b'_' | b'-'))
    {
        return Some(segments[0].len());
    }
    Some(0)
}

/// One `-`/`_`-split piece of a name-shaped segment: alnum/dot text that
/// is short, a word, a date/id, or a dotted version.
fn name_piece(p: &[u8]) -> bool {
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

/// A `/`-segment is name-shaped when every `-`/`_`-split piece is a
/// [`name_piece`].
fn name_segment(s: &[u8]) -> bool {
    s.len() < 6 && s.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'.')
        || s.split(|b| matches!(b, b'-' | b'_'))
            .all(|p| !p.is_empty() && name_piece(p))
}

/// The keep beside a dying run: no uppercase, every `-`/`_` piece under 6
/// chars, or a dotted version.
fn strict_name(s: &[u8]) -> bool {
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

/// `8-4-4-4-12` hex with dashes is a UUID — an identifier, never a credential —
/// whichever digit count the run carries.
fn looks_like_uuid(token: &[u8]) -> bool {
    const PARTS: [usize; 5] = [8, 4, 4, 4, 12];
    token.iter().all(|b| b.is_ascii_hexdigit() || *b == b'-')
        && token.split(|b| *b == b'-').map(<[u8]>::len).eq(PARTS)
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
            Some("GET https://hooks.example.com[redacted]")
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
        // model ids and URL paths are long mixed runs too. A run with path
        // evidence stays readable only when every segment is name-shaped;
        // when it dies, at most a strict-name first segment keeps.
        for text in [
            "model mistralai/Mistral-7B-Instruct-v0.3 refused the prompt",
            "model mistralai/Mixtral-8x22B-Instruct-v0p1 refused the prompt",
            "read /var/log/aiops-collector/RunId42-Error/trace-2026.log tail",
            "GET /api/v2/ops-run/RunId42/status 200",
            "release/2024-11-20/publish ok",
        ] {
            assert_eq!(redact(text), None, "{text:?} is operational text");
        }
        // A webhook path still dies — the `.`-evidenced run keeps its
        // name-shaped first segment (`com`) and kills the segments no
        // human would name.
        assert_eq!(
            redact("GET https://hooks.example.com/services/T0ABCDEF12/B0CDEF1234/xyZab").as_deref(),
            Some("GET https://hooks.example.com[redacted]")
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
    fn the_encoded_and_split_shapes_the_review_found() {
        // N7a: a fully percent-encoded JSON secret — the key and separator
        // are escape bytes, invisible to the raw scanner.
        let encoded = "%7B%22client_secret%22%3A%22Zq81aa%22%7D";
        let out = redact(&format!("got {encoded}"))
            .unwrap_or_else(|| panic!("encoded secret must die: {encoded}"));
        assert!(!out.contains("Zq81aa"), "{encoded} → {out:?}");
        // N7b: a secret key whose value is a JSON array — like `{`, the
        // whole container is credential.
        assert_eq!(
            redact(r#"{"auth": ["hunter2", "hunter3"]}"#).as_deref(),
            Some(r#"{"auth": [redacted]}"#)
        );
        // N7c (reviewed): `,` and `;` are credential bytes, not separators —
        // an unquoted value ends at whitespace, a quote or `&` only.
        assert_eq!(
            redact("password=hunter2,theme=dark;sid=abc123&x=1").as_deref(),
            Some("password=[redacted]&x=1")
        );
        // N7d: a bare lowercase-hex run is a credential shape at 40+ chars —
        // a commit-length SHA is the accepted cost (hashes name nothing a
        // caller can spend).
        let hex = "a".repeat(40);
        assert_eq!(
            redact(&format!("signing {hex} done")).as_deref(),
            Some("signing [redacted] done")
        );
        // N7e: a base64url piece with a single digit — the JWT-signature
        // tail the two-digit rule missed — and a split `eyJ` run.
        let piece = "aB".repeat(16) + "3";
        assert_eq!(
            redact(&format!("sig {piece} end")).as_deref(),
            Some("sig [redacted] end"),
            "base64url run ≥32 with _ or - needs only one digit"
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
