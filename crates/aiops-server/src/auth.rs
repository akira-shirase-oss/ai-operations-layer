//! Credentials (D-015). Ingest keys (`aiops_ik_…`) and API tokens (`aiops_tk_…`) are 32
//! random bytes, base64url; the server only ever stores and compares their SHA-256.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::locks::LockStats;

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

/// What the cache knows about a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached<V> {
    /// Looked up within the TTL: `Some` is valid, `None` is unknown or revoked.
    Fresh(Option<V>),
    /// Valid when last looked up, past the TTL but inside the stale window: usable when
    /// the lookup budget is exhausted, so a flood of unknown keys cannot lock a known host
    /// out (revocation then takes effect within the stale window).
    Stale(V),
    Miss,
}

/// A bounded map that evicts its oldest entries first, never everything at once.
struct Fifo<V> {
    capacity: usize,
    map: HashMap<[u8; 32], (V, Instant)>,
    order: VecDeque<([u8; 32], Instant)>,
}

impl<V> Fifo<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, key: [u8; 32], value: V) {
        let now = Instant::now();
        self.map.insert(key, (value, now));
        self.order.push_back((key, now));
        while self.map.len() > self.capacity {
            let Some((old, at)) = self.order.pop_front() else {
                break;
            };
            // Skip order entries superseded by a later insert of the same key.
            if self.map.get(&old).is_some_and(|(_, t)| *t == at) {
                self.map.remove(&old);
            }
        }
        // Keep the order queue bounded too (superseded entries accumulate).
        if self.order.len() > self.capacity * 2 {
            let map = &self.map;
            self.order
                .retain(|(k, t)| map.get(k).is_some_and(|(_, at)| at == t));
        }
    }

    fn remove(&mut self, key: &[u8; 32]) {
        self.map.remove(key);
    }
}

/// The cache's mutable state — both maps and the generation under ONE
/// mutex (review N-F): a `remove` that bumped the generation outside the
/// locks let a `put_if_same` check a stale generation and then put, so a
/// revoked row came back for up to a TTL. Check-then-act is only honest
/// when the check and the act share the lock.
struct Inner<V> {
    /// Bumped by every `remove`/`clear`: a lookup whose store read started
    /// before an eviction may not write its (pre-eviction) answer back
    /// (review S3's re-insert race).
    generation: u64,
    positive: Fifo<V>,
    negative: Fifo<()>,
}

/// A bounded, time-limited cache of credential lookups. Valid and invalid credentials are
/// kept apart, so unknown keys (which an attacker can mint at will) can only evict other
/// unknown keys, never a valid one.
pub struct CredentialCache<V> {
    ttl: Duration,
    negative_ttl: Duration,
    stale_for: Duration,
    inner: Mutex<Inner<V>>,
    /// Both maps' wait and hold times (D-026).
    pub lock_stats: LockStats,
}

impl<V: Clone> CredentialCache<V> {
    pub fn new(ttl: Duration, negative_ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            negative_ttl,
            stale_for: Duration::from_secs(300),
            inner: Mutex::new(Inner {
                generation: 0,
                positive: Fifo::new(capacity),
                negative: Fifo::new(capacity),
            }),
            lock_stats: LockStats::default(),
        }
    }

    /// The generation a caller snapshots before its store read — a mirror
    /// read under the lock; a poisoned lock answers MAX so `put_if_same`
    /// can never match it.
    pub fn generation(&self) -> u64 {
        self.lock_stats
            .lock(&self.inner)
            .map_or(u64::MAX, |i| i.generation)
    }

    /// The configured fresh window — `resolve` shortens it while the
    /// revocation listener's health is uncertain (review V1).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The longest an entry can answer for — fresh plus the stale window;
    /// the revocation catch-up reads revocations no older than this
    /// (review N-C: older ones could not still be cached anyway).
    pub fn max_entry_age(&self) -> Duration {
        self.ttl + self.stale_for
    }

    /// Drop any cached answer for a digest — the revocation path: a revoked
    /// credential must stop working now, not when its TTL runs out (D-034).
    pub fn remove(&self, key: &[u8; 32]) {
        if let Some(mut inner) = self.lock_stats.lock(&self.inner) {
            inner.generation += 1;
            inner.positive.remove(key);
            inner.negative.remove(key);
        }
    }

    /// Drop every cached answer — the revocation listener's reconnect
    /// recovery: notifications missed while the channel was down are
    /// indistinguishable from none, so the only safe resume is empty
    /// (D-034, N-G: a gap can hide a DELETE too, and the digest set the
    /// catch-up re-reads holds only revocations).
    pub fn clear(&self) {
        if let Some(mut inner) = self.lock_stats.lock(&self.inner) {
            inner.generation += 1;
            inner.positive.map.clear();
            inner.positive.order.clear();
            inner.negative.map.clear();
            inner.negative.order.clear();
        }
    }

    /// `fresh_ttl` is the caller's fresh window — the revocation listener's
    /// health shrinks it (an unhealthy channel means entries may already
    /// hold a revoked answer; review V1).
    pub fn get(&self, key: &[u8; 32], fresh_ttl: Duration) -> Cached<V> {
        let Some(inner) = self.lock_stats.lock(&self.inner) else {
            return Cached::Miss;
        };
        if let Some((v, at)) = inner.positive.map.get(key) {
            let age = at.elapsed();
            if age < fresh_ttl {
                return Cached::Fresh(Some(v.clone()));
            }
            if age < self.ttl + self.stale_for {
                return Cached::Stale(v.clone());
            }
        }
        if let Some(((), at)) = inner.negative.map.get(key)
            && at.elapsed() < self.negative_ttl.min(fresh_ttl)
        {
            return Cached::Fresh(None);
        }
        Cached::Miss
    }

    /// Write the answer a lookup returned — but only if nothing evicted or
    /// cleared since `seen_gen` was read: a snapshot taken before a
    /// revocation must not put the row back (review S3). The generation
    /// check and the put happen under the one lock, so an eviction can
    /// never slip between them (review N-F).
    pub fn put_if_same(&self, key: [u8; 32], value: Option<V>, seen_gen: u64) {
        if let Some(mut inner) = self.lock_stats.lock(&self.inner)
            && inner.generation == seen_gen
        {
            Self::put_locked(&mut inner, key, value);
        }
    }

    pub fn put(&self, key: [u8; 32], value: Option<V>) {
        if let Some(mut inner) = self.lock_stats.lock(&self.inner) {
            Self::put_locked(&mut inner, key, value);
        }
    }

    fn put_locked(inner: &mut Inner<V>, key: [u8; 32], value: Option<V>) {
        if let Some(v) = value {
            inner.negative.remove(&key);
            inner.positive.insert(key, v);
        } else {
            inner.positive.remove(&key);
            inner.negative.insert(key, ());
        }
    }
}

/// Per-credential token buckets, bounded by the number of credentials that exist.
pub struct Buckets<K> {
    rate: u32,
    burst: u32,
    map: Mutex<HashMap<K, aiops_domain::ratelimit::TokenBucket>>,
    /// The map's wait and hold times (D-026).
    pub lock_stats: LockStats,
}

impl<K: Eq + Hash + Copy> Buckets<K> {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            rate,
            burst,
            map: Mutex::new(HashMap::new()),
            lock_stats: LockStats::default(),
        }
    }

    /// Takes `amount` or returns the whole seconds to wait.
    pub fn take(&self, key: K, amount: u32, now_micros: i64) -> Result<(), u64> {
        let Some(mut map) = self.lock_stats.lock(&self.map) else {
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
    fn unknown_keys_never_evict_valid_ones_and_eviction_is_oldest_first() {
        let cache: CredentialCache<u8> =
            CredentialCache::new(Duration::from_secs(30), Duration::from_secs(5), 2);
        cache.put([1; 32], Some(1));
        for n in 10..200u8 {
            cache.put([n; 32], None); // a flood of unknown keys
        }
        assert_eq!(
            cache.get(&[1; 32], Duration::from_secs(30)),
            Cached::Fresh(Some(1)),
            "a valid key survives the flood"
        );
        assert_eq!(
            cache.get(&[199; 32], Duration::from_secs(30)),
            Cached::Fresh(None)
        );
        assert_eq!(
            cache.get(&[10; 32], Duration::from_secs(30)),
            Cached::Miss,
            "the oldest unknown key was evicted"
        );
        cache.put([2; 32], Some(2));
        cache.put([3; 32], Some(3));
        assert_eq!(
            cache.get(&[1; 32], Duration::from_secs(30)),
            Cached::Miss,
            "oldest valid entry evicted first"
        );
        assert_eq!(
            cache.get(&[3; 32], Duration::from_secs(30)),
            Cached::Fresh(Some(3))
        );
    }
}
