//! Credentials (D-015). Ingest keys (`aiops_ik_…`) and API tokens (`aiops_tk_…`) are 32
//! random bytes, base64url; the server only ever stores and compares their SHA-256.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

pub const INGEST_KEY_PREFIX: &str = "aiops_ik_";
pub const TOKEN_PREFIX: &str = "aiops_tk_";
const SECRET_CHARS: usize = 43; // base64url of 32 bytes, no padding

/// A fresh credential with `prefix`, from the operating system's randomness.
pub fn generate(prefix: &str) -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub fn hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// The bearer credential, if it has the expected shape. Shape is checked before any
/// hashing or lookup, so garbage costs nothing.
pub fn bearer<'a>(headers: &'a HeaderMap, prefix: &str) -> Option<&'a str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let secret = value.strip_prefix("Bearer ")?;
    let body = secret.strip_prefix(prefix)?;
    (body.len() == SECRET_CHARS
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(secret)
}

type Entries<V> = HashMap<[u8; 32], (Option<V>, Instant)>;

/// A bounded, time-limited cache of credential lookups (positive and negative), so the
/// hot path does not query PostgreSQL per request. Revocation takes effect within `ttl`.
pub struct CredentialCache<V> {
    ttl: Duration,
    negative_ttl: Duration,
    capacity: usize,
    entries: Mutex<Entries<V>>,
}

impl<V: Clone> CredentialCache<V> {
    pub fn new(ttl: Duration, negative_ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            negative_ttl,
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// `Some(found)` when cached and fresh.
    pub fn get(&self, key: &[u8; 32]) -> Option<Option<V>> {
        let entries = self.entries.lock().ok()?;
        let (value, at) = entries.get(key)?;
        let ttl = if value.is_some() {
            self.ttl
        } else {
            self.negative_ttl
        };
        (at.elapsed() < ttl).then(|| value.clone())
    }

    pub fn put(&self, key: [u8; 32], value: Option<V>) {
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= self.capacity {
                entries.clear(); // bounded: a flood of unknown keys cannot grow it
            }
            entries.insert(key, (value, Instant::now()));
        }
    }
}

/// Per-credential token buckets, bounded by the number of credentials that exist.
pub struct Buckets<K> {
    rate: u32,
    burst: u32,
    map: Mutex<HashMap<K, aiops_domain::ratelimit::TokenBucket>>,
}

impl<K: Eq + Hash + Copy> Buckets<K> {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            rate,
            burst,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Takes `amount` or returns the whole seconds to wait.
    pub fn take(&self, key: K, amount: u32, now_micros: i64) -> Result<(), u64> {
        let Ok(mut map) = self.map.lock() else {
            return Ok(()); // a poisoned limiter fails open for one request, never panics
        };
        let bucket = map.entry(key).or_insert_with(|| {
            aiops_domain::ratelimit::TokenBucket::new(self.burst, self.rate, now_micros)
        });
        bucket.try_take(amount, now_micros).map_err(|r| r.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_credentials_have_the_shape_bearer_accepts() {
        let key = generate(INGEST_KEY_PREFIX).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        assert_eq!(bearer(&headers, INGEST_KEY_PREFIX), Some(key.as_str()));
        assert_eq!(bearer(&headers, TOKEN_PREFIX), None, "a key is not a token");
    }

    #[test]
    fn malformed_credentials_are_refused_by_shape() {
        for value in [
            "Bearer aiops_ik_short",
            "Basic abc",
            "Bearer aiops_ik_!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", value.parse().unwrap());
            assert_eq!(bearer(&headers, INGEST_KEY_PREFIX), None, "{value}");
        }
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache: CredentialCache<u8> =
            CredentialCache::new(Duration::from_secs(30), Duration::from_secs(5), 2);
        cache.put([1; 32], Some(1));
        cache.put([2; 32], None);
        cache.put([3; 32], Some(3));
        assert_eq!(cache.get(&[3; 32]), Some(Some(3)));
        assert_eq!(cache.get(&[1; 32]), None, "cleared when full");
    }
}
