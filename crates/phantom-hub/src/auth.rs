//! Authentication — JWT device token issuance/verification and API key
//! validation.
//!
//! # Token model
//!
//! Two principal types:
//!
//! - **Device token** — long-lived JWT issued to a Phantom instance when it
//!   registers via `POST /auth/register`.  The hub signs the JWT with its
//!   `HUB_JWT_SECRET` (HS256 HMAC).  Claims: `sub` (phantom_id), `iss`
//!   (`"phantom-hub"`), `iat`, `exp` (30 days from issuance).  On each WSS
//!   connection the Phantom presents this JWT in the registration frame;
//!   the hub verifies the signature and exp.
//!
//! - **API key** — a static bearer token issued to Claude sessions out-of-band
//!   (v1: admin sets `HUB_API_KEYS` env var, comma-separated `phk_<...>`
//!   values).  The hub stores SHA-256 hashes at startup and compares using
//!   constant-time equality.  Keys are presented via `Authorization: Bearer`
//!   on `/mcp` and `/mcp/sse`.
//!
//! # JWT library choice
//!
//! `jsonwebtoken` (crate version 9) — the most widely used Rust JWT library,
//! actively maintained, supports HS256/RS256, first-class exp/iat/iss
//! validation.  Phase 3 will switch the algorithm field from `HS256` to
//! `RS256` with a per-Phantom public key; the call sites do not change.
//!
//! # TOFU vs PKI directory
//!
//! v1 uses a **shared hub HMAC secret** (not TOFU and not a PKI directory).
//! The hub signs the JWT itself at registration time, so there is no need to
//! look up Phantom public keys on verification — the HMAC secret IS the
//! authority.  Threat model: anyone who obtains `HUB_JWT_SECRET` can forge
//! JWTs for any phantom_id.  Mitigation: secret stored only in Railway env
//! vars; never logged or written to disk.  Phase 3 replaces with RS256 +
//! per-Phantom keys + a Postgres key directory.
//!
//! # Environment variables
//!
//! - `HUB_JWT_SECRET` — HMAC secret for HS256.  **Hub aborts startup if
//!   absent** (enforced by [`JwtAuthority::from_env`]).
//! - `HUB_API_KEYS` — comma-separated `phk_<base64url>` API keys.  Missing
//!   or empty disables Claude→hub access (every MCP call returns 401).
//!
//! # Replay mitigation
//!
//! The registration flow requires a nonce (see `POST /auth/register`).
//! Phantom signs `(nonce || peer_id)` with its Ed25519 identity key; the hub
//! verifies the signature before issuing a JWT.  After signature verification
//! the hub calls [`NonceCache::try_claim`] which atomically records the nonce
//! as consumed and returns `false` on any subsequent replay attempt.  A replay
//! of the same signed request is rejected with `409 Conflict` before a JWT is
//! issued.
//!
//! [`NonceCache`] is backed by an LRU cache (capacity 10 000, TTL 10 minutes).
//! Entries are evicted lazily on the oldest-first basis when the cache is full;
//! there is no full-clear fallback.  The TTL window (10 minutes) is
//! intentionally much shorter than the JWT lifetime (30 days) — it covers the
//! realistic registration-to-WSS-connect interval while keeping memory bounded.
//!
//! Short-lived replay window on the WSS registration frame: the JWT itself
//! has a 30-day exp.  Clock skew tolerance is ±5 minutes.  A stolen JWT can
//! be used until its exp; rotation is via `phantom auth register --renew`
//! (ticket 09).

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// JWT issuer claim value.
pub const JWT_ISSUER: &str = "phantom-hub";

/// JWT lifetime — 30 days in seconds.
pub const JWT_EXP_SECS: u64 = 30 * 24 * 60 * 60;

/// Clock skew tolerance for JWT validation (±5 minutes).
pub const JWT_LEEWAY_SECS: u64 = 5 * 60;

/// Maximum number of nonces tracked simultaneously by [`NonceCache`].
///
/// At 10 000 entries the cache uses roughly 640 KB at worst (64-byte nonce
/// strings + 16-byte `Instant`).  This is well within the hub's memory budget.
pub const NONCE_CACHE_CAPACITY: usize = 10_000;

/// How long a claimed nonce is remembered before it is eligible for eviction.
///
/// Set to 10 minutes — twice the JWT clock-skew leeway (5 min) and well within
/// any realistic registration-to-WSS-connect window.
pub const NONCE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

// ---------------------------------------------------------------------------
// CapabilityClass — per-API-key operation scoping (issue #511)
// ---------------------------------------------------------------------------

/// The set of operations an API key is permitted to invoke on the hub.
///
/// Each MCP tool is tagged with one capability class.  When an API key presents
/// its bearer token, the hub checks that the key's capability set includes the
/// class required by the requested tool before forwarding the frame.
///
/// # Default for legacy keys
///
/// Keys loaded from the `HUB_API_KEYS` environment variable (comma-separated
/// `phk_<…>` values) are assigned **all capabilities** (`ALL_CAPABILITIES`) for
/// backwards compatibility — this preserves the v1 behaviour where any valid key
/// could call any tool.
///
/// Operators who want fail-closed semantics for new keys should provision them
/// via [`ApiKeyStore::from_entries`] with an explicit, narrow capability set.
///
/// # Migration path
///
/// To tighten an existing deployment:
/// 1. Issue new keys via `from_entries` with only the capabilities needed.
/// 2. Rotate out the old `HUB_API_KEYS` keys.
/// 3. At that point every key in the store has an explicit, narrow grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CapabilityClass {
    /// Read / observe: `phantom.list_phantoms`, `phantom.read_output`.
    Sense,
    /// Execute shell commands: `phantom.run_command`.
    Compute,
    /// Spawn or steer other agents: `phantom.spawn_agent`,
    /// `phantom.list_panes`, `phantom.get_agent_status`.
    Coordinate,
}

/// All capability classes — granted to API keys loaded from `HUB_API_KEYS` for
/// backwards compatibility.
pub const ALL_CAPABILITIES: &[CapabilityClass] = &[
    CapabilityClass::Sense,
    CapabilityClass::Compute,
    CapabilityClass::Coordinate,
];

// ---------------------------------------------------------------------------
// NonceCache — replay protection for POST /auth/register
// ---------------------------------------------------------------------------

/// Single-use nonce tracker for `POST /auth/register`.
///
/// Backed by an LRU cache (capacity [`NONCE_CACHE_CAPACITY`], TTL
/// [`NONCE_CACHE_TTL`]).  On every `try_claim` call the Mutex is acquired
/// once; check and insert are performed under the same acquisition so the
/// operation is atomic — there is no window between the check and the insert.
///
/// The lock is never held across an `.await` point.  `NonceCache` is `Send +
/// Sync` by construction (`Mutex<LruCache<...>>`).
///
/// # Eviction
///
/// When the cache reaches capacity, the LRU entry is evicted before the new
/// nonce is inserted.  Additionally, `try_claim` performs lazy TTL eviction:
/// if an existing entry for the same nonce key is found but its recorded
/// `Instant` is older than [`NONCE_CACHE_TTL`], the entry is treated as
/// expired and the nonce is re-claimable (returns `true`).
pub struct NonceCache {
    inner: Mutex<LruCache<String, Instant>>,
    ttl: Duration,
    /// Optional clock override used in tests to control what "now" means
    /// without sleeping.  Production code always uses `None` (wall clock).
    clock: Option<fn() -> Instant>,
}

impl NonceCache {
    /// Create a [`NonceCache`] with production defaults:
    /// capacity [`NONCE_CACHE_CAPACITY`], TTL [`NONCE_CACHE_TTL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity_and_ttl(NONCE_CACHE_CAPACITY, NONCE_CACHE_TTL)
    }

    /// Create a [`NonceCache`] with explicit capacity and TTL.
    ///
    /// Intended for tests that need a small cache to exercise eviction without
    /// inserting 10 000 entries.  Production code must use [`NonceCache::new`].
    #[must_use]
    pub fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        let cap = std::num::NonZeroUsize::new(capacity.max(1))
            .expect("capacity is always ≥ 1 after max(1)");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl,
            clock: None,
        }
    }

    /// Create a [`NonceCache`] with a custom clock function (issue #506).
    ///
    /// `clock_fn` is called instead of `Instant::now()` on every [`try_claim`]
    /// invocation, enabling deterministic TTL-expiry tests without sleeping.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::sync::Arc;
    /// use std::time::{Duration, Instant};
    ///
    /// static OFFSET_NANOS: AtomicU64 = AtomicU64::new(0);
    /// static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ///
    /// fn fake_clock() -> Instant {
    ///     *BASE.get_or_init(Instant::now)
    ///         + Duration::from_nanos(OFFSET_NANOS.load(Ordering::Relaxed))
    /// }
    /// let cache = NonceCache::with_clock(16, Duration::from_secs(10), fake_clock);
    /// ```
    #[must_use]
    pub fn with_clock(capacity: usize, ttl: Duration, clock_fn: fn() -> Instant) -> Self {
        let cap = std::num::NonZeroUsize::new(capacity.max(1))
            .expect("capacity is always ≥ 1 after max(1)");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl,
            clock: Some(clock_fn),
        }
    }

    /// Return the current instant from the injected clock or `Instant::now()`.
    fn now(&self) -> Instant {
        match self.clock {
            Some(f) => f(),
            None => Instant::now(),
        }
    }

    /// Atomically claim `nonce`.
    ///
    /// Returns `true` on the first successful claim.  Returns `false` when
    /// `nonce` has already been claimed and its TTL has not yet expired.
    ///
    /// An expired nonce (older than [`Self::ttl`]) is evicted and treated as
    /// a new nonce — returns `true`.
    ///
    /// # Panics
    ///
    /// Panics only if the internal `Mutex` is poisoned, which cannot happen
    /// unless a previous thread panicked while holding the lock.
    pub fn try_claim(&self, nonce: &str) -> bool {
        let mut cache = self.inner.lock().expect("NonceCache mutex poisoned");
        let now = self.now();

        // peek without promoting to LRU-head so we don't disturb eviction order
        // for a stale entry we are about to replace.
        if let Some(&claimed_at) = cache.peek(nonce)
            && now.duration_since(claimed_at) < self.ttl
        {
            // Still within TTL — this is a replay.
            return false;
        }
        // Not found or expired — fall through to insert with a fresh timestamp.

        // First claim (or expired re-claim): insert/update and return true.
        cache.put(nonce.to_owned(), now);
        true
    }
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// IpRateLimiter — sliding-window per-IP request throttle
// ---------------------------------------------------------------------------

/// Per-IP sliding-window rate limiter.
///
/// Tracks recent request timestamps for each IP address and enforces a maximum
/// number of requests within a rolling time window.  Designed to protect
/// debug or low-frequency administrative endpoints from enumeration attacks.
///
/// # Design
///
/// The sliding-window implementation keeps a `VecDeque` of `Instant`s per IP.
/// On every call the limiter prunes timestamps older than `window`, then checks
/// whether the remaining count is below `max_requests`.  If the count is at or
/// above the limit the call returns `false` (throttled).  Otherwise the current
/// timestamp is pushed and the call returns `true` (allowed).
///
/// Stale IP entries (IPs with no requests in the last `2 * window`) are evicted
/// lazily to keep memory bounded.
///
/// # Thread safety
///
/// All mutable state is behind a `Mutex`.  The lock is never held across an
/// `.await` point — each `check_and_record` call acquires and releases
/// synchronously.
pub struct IpRateLimiter {
    inner: Mutex<HashMap<IpAddr, std::collections::VecDeque<Instant>>>,
    /// Number of requests allowed per IP within `window`.
    max_requests: usize,
    /// The rolling time window over which `max_requests` is counted.
    window: Duration,
}

impl IpRateLimiter {
    /// Create a new limiter: `max_requests` per IP per `window`.
    #[must_use]
    pub fn new(max_requests: usize, window: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_requests,
            window,
        }
    }

    /// Create the standard registry-endpoint limiter: 10 requests per minute.
    #[must_use]
    pub fn registry_default() -> Self {
        Self::new(10, Duration::from_secs(60))
    }

    /// Check whether `ip` is within its rate limit and record the attempt.
    ///
    /// Returns `true` when the request is allowed (count was below the limit
    /// before recording).  Returns `false` when the IP has already reached
    /// `max_requests` within the sliding `window`.
    ///
    /// # Panics
    ///
    /// Panics only if the internal `Mutex` is poisoned.
    pub fn check_and_record(&self, ip: IpAddr) -> bool {
        let mut map = self.inner.lock().expect("IpRateLimiter mutex poisoned");
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);

        let timestamps = map.entry(ip).or_default();

        // Prune entries older than the sliding window.
        while timestamps.front().is_some_and(|&t| t <= cutoff) {
            timestamps.pop_front();
        }

        if timestamps.len() >= self.max_requests {
            return false; // rate limit exceeded
        }

        timestamps.push_back(now);

        // Lazy eviction: remove IPs with empty timestamp queues to keep
        // memory bounded for connections that stopped after being throttled.
        map.retain(|_, ts| !ts.is_empty());

        true
    }
}

// ---------------------------------------------------------------------------
// AdminToken — admin bearer token for protected debug endpoints
// ---------------------------------------------------------------------------

/// Optional bearer token that protects administrative debug endpoints.
///
/// Loaded from `PHANTOM_HUB_ADMIN_TOKEN` at startup.  When the environment
/// variable is absent or empty the token is `None` and the protected endpoint
/// is disabled entirely (returns `404 Not Found`).
///
/// The comparison is **not** constant-time because the token guards a debug
/// endpoint that should normally be unreachable in production.  Constant-time
/// comparison is reserved for the crypto-sensitive API key store.
#[derive(Clone, Debug)]
pub struct AdminToken(Option<String>);

impl AdminToken {
    /// Load from `PHANTOM_HUB_ADMIN_TOKEN` environment variable.
    ///
    /// Returns `AdminToken(None)` when the variable is unset or empty.
    #[must_use]
    pub fn from_env() -> Self {
        let token = std::env::var("PHANTOM_HUB_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Self(token)
    }

    /// Construct from an explicit token value.  Used in tests and bootstrapping.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        Self(Some(token.into()))
    }

    /// Construct a disabled admin token (no token configured).
    ///
    /// When used in [`AppState`], the `/registry` endpoint returns
    /// `503 Service Unavailable` for all requests.  Used in tests.
    #[must_use]
    pub fn disabled() -> Self {
        Self(None)
    }

    /// Returns `true` when the admin token is configured (env var is set and non-empty).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.0.is_some()
    }

    /// Validate `presented` against the stored token.
    ///
    /// Returns `true` iff the admin token is configured AND `presented` matches
    /// it exactly.  Returns `false` in all other cases (unconfigured, mismatch).
    #[must_use]
    pub fn validate(&self, presented: &str) -> bool {
        match &self.0 {
            Some(stored) => stored == presented,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

/// Claims embedded in a Phantom device JWT.
///
/// The `sub` claim carries the Phantom's `peer_id` (base58 string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhantomClaims {
    /// Issued-by — always `"phantom-hub"`.
    pub iss: String,
    /// Subject — the Phantom's stable `peer_id` (base58 SHA-256 of public key).
    pub sub: String,
    /// Issued-at (Unix seconds).
    pub iat: u64,
    /// Expiry (Unix seconds).
    pub exp: u64,
}

// ---------------------------------------------------------------------------
// JwtAuthority
// ---------------------------------------------------------------------------

/// HMAC key pair used to issue and verify Phantom device JWTs.
///
/// Constructed once at startup via [`JwtAuthority::from_env`] and shared
/// (cloned) into each request handler.
#[derive(Clone)]
pub struct JwtAuthority {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
}

impl JwtAuthority {
    /// Construct a [`JwtAuthority`] from the `HUB_JWT_SECRET` environment
    /// variable.
    ///
    /// # Panics / Errors
    ///
    /// Returns `Err` when `HUB_JWT_SECRET` is absent or empty.  The hub's
    /// `main` function is expected to call this at startup and abort if it
    /// fails — running without a signing secret is not safe.
    pub fn from_env() -> anyhow::Result<Self> {
        let secret = std::env::var("HUB_JWT_SECRET")
            .map_err(|_| anyhow::anyhow!("HUB_JWT_SECRET env var is required but not set"))?;
        anyhow::ensure!(!secret.is_empty(), "HUB_JWT_SECRET must not be empty");
        Ok(Self::from_secret(secret.as_bytes()))
    }

    /// Construct from an explicit byte slice.  Used in tests.
    #[must_use]
    pub fn from_secret(secret: &[u8]) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[JWT_ISSUER]);
        validation.leeway = JWT_LEEWAY_SECS;
        // `sub` is verified by the caller against the registered phantom_id.
        // We require it to be present but do not add it to the validation
        // set (jsonwebtoken would need an exact expected value).
        validation.set_required_spec_claims(&["exp", "iat", "iss", "sub"]);

        Self {
            encoding_key: EncodingKey::from_secret(secret),
            decoding_key: DecodingKey::from_secret(secret),
            validation,
        }
    }

    /// Issue a JWT for `phantom_id`.
    ///
    /// Returns the raw JWT string.  Do not log this value.
    pub fn issue(&self, phantom_id: &str) -> anyhow::Result<String> {
        let now = unix_now();
        let claims = PhantomClaims {
            iss: JWT_ISSUER.to_owned(),
            sub: phantom_id.to_owned(),
            iat: now,
            exp: now + JWT_EXP_SECS,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding_key)
            .map_err(|e| anyhow::anyhow!("JWT encode error: {e}"))
    }

    /// Verify a JWT and return the decoded claims.
    ///
    /// Checks: signature, `iss`, `exp` (with ±[`JWT_LEEWAY_SECS`] tolerance).
    /// Returns `Err` for any validation failure.
    pub fn verify(&self, token: &str) -> Result<PhantomClaims, AuthError> {
        decode::<PhantomClaims>(token, &self.decoding_key, &self.validation)
            .map(|data| data.claims)
            .map_err(|e| {
                use jsonwebtoken::errors::ErrorKind;
                match e.kind() {
                    ErrorKind::ExpiredSignature => AuthError::Expired,
                    ErrorKind::InvalidSignature
                    | ErrorKind::InvalidToken
                    | ErrorKind::InvalidAlgorithmName
                    | ErrorKind::InvalidAlgorithm => AuthError::InvalidSignature,
                    _ => AuthError::InvalidSignature,
                }
            })
    }
}

// ---------------------------------------------------------------------------
// DeviceIdentity
// ---------------------------------------------------------------------------

/// The authenticated identity of a Phantom device (decoded from a verified JWT).
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// The Phantom's stable `peer_id`, extracted from the JWT `sub` claim.
    pub phantom_id: String,
    /// When the token expires (Unix seconds).
    pub exp: u64,
}

impl DeviceIdentity {
    fn from_claims(claims: PhantomClaims) -> Self {
        Self {
            phantom_id: claims.sub,
            exp: claims.exp,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionIdentity
// ---------------------------------------------------------------------------

/// The authenticated identity of a Claude session (MCP caller).
///
/// Carries both the stable key identifier and the capability set granted to
/// this key.  Callers check [`SessionIdentity::has`] before forwarding an
/// operation to a Phantom.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// The SHA-256 hash of the presented API key (used as a stable key-id).
    /// The raw key is discarded after hashing and never stored.
    pub key_hash: [u8; 32],
    /// The set of operations this key is permitted to invoke.
    pub capabilities: HashSet<CapabilityClass>,
}

impl SessionIdentity {
    /// Return `true` iff this identity carries `class` in its capability set.
    #[must_use]
    pub fn has(&self, class: CapabilityClass) -> bool {
        self.capabilities.contains(&class)
    }
}

// ---------------------------------------------------------------------------
// ApiKeyStore
// ---------------------------------------------------------------------------

/// Internal entry stored per API key in [`ApiKeyStore`].
///
/// Pairs the SHA-256 hash of the raw key (used for constant-time comparison)
/// with the set of [`CapabilityClass`] values the key is allowed to exercise.
#[derive(Clone)]
struct ApiKeyEntry {
    hash: [u8; 32],
    capabilities: HashSet<CapabilityClass>,
}

/// In-memory store of SHA-256-hashed API keys.
///
/// Loaded at startup from `HUB_API_KEYS` (comma-separated `phk_<...>` values).
/// The raw keys are hashed immediately and the originals discarded — only
/// hashes live in memory past the constructor.
///
/// Keys loaded from the environment variable receive all capabilities
/// ([`ALL_CAPABILITIES`]) for backwards compatibility.  Use
/// [`ApiKeyStore::from_entries`] to provision keys with explicit, narrow grants.
#[derive(Clone, Default)]
pub struct ApiKeyStore {
    entries: Vec<ApiKeyEntry>,
}

impl ApiKeyStore {
    /// Load from `HUB_API_KEYS` environment variable.
    ///
    /// All keys loaded this way receive [`ALL_CAPABILITIES`] (backwards
    /// compatibility — same behaviour as v1 where any valid key could call
    /// any tool).  Returns an empty store (no keys accepted) if the variable
    /// is unset or empty.
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var("HUB_API_KEYS").unwrap_or_default();
        Self::from_raw_keys(raw.split(',').map(str::trim).filter(|s| !s.is_empty()))
    }

    /// Construct from an explicit iterator of raw key strings, granting all
    /// capabilities to each key.  Used in tests and for compat loading.
    pub fn from_raw_keys<'a>(keys: impl Iterator<Item = &'a str>) -> Self {
        let all: HashSet<CapabilityClass> = ALL_CAPABILITIES.iter().copied().collect();
        let entries = keys
            .map(|k| {
                let mut h = Sha256::new();
                h.update(k.as_bytes());
                ApiKeyEntry {
                    hash: h.finalize().into(),
                    capabilities: all.clone(),
                }
            })
            .collect();
        Self { entries }
    }

    /// Construct from explicit `(raw_key, capabilities)` pairs.
    ///
    /// Use this constructor when provisioning new keys with narrow capability
    /// grants rather than the legacy all-capabilities default.
    pub fn from_entries<'a>(
        pairs: impl Iterator<Item = (&'a str, HashSet<CapabilityClass>)>,
    ) -> Self {
        let entries = pairs
            .map(|(k, caps)| {
                let mut h = Sha256::new();
                h.update(k.as_bytes());
                ApiKeyEntry {
                    hash: h.finalize().into(),
                    capabilities: caps,
                }
            })
            .collect();
        Self { entries }
    }

    /// Validate an API key.
    ///
    /// Hashes `key` and performs a constant-time comparison against every
    /// stored hash.  Returns the [`SessionIdentity`] (including capability set)
    /// on success.
    pub fn validate(&self, key: &str) -> Result<SessionIdentity, AuthError> {
        if self.entries.is_empty() {
            return Err(AuthError::UnknownKey);
        }

        let mut candidate_hash = Sha256::new();
        candidate_hash.update(key.as_bytes());
        let candidate: [u8; 32] = candidate_hash.finalize().into();

        // Walk ALL entries regardless of an early match — constant-time comparison.
        let mut found_caps: Option<HashSet<CapabilityClass>> = None;
        let mut found = subtle::Choice::from(0u8);
        for entry in &self.entries {
            let eq = entry.hash.ct_eq(&candidate);
            if bool::from(eq) && found_caps.is_none() {
                // Capture capabilities on first match; continue loop for constant time.
                found_caps = Some(entry.capabilities.clone());
            }
            found |= eq;
        }

        if bool::from(found) {
            Ok(SessionIdentity {
                key_hash: candidate,
                capabilities: found_caps.unwrap_or_default(),
            })
        } else {
            Err(AuthError::UnknownKey)
        }
    }
}

// ---------------------------------------------------------------------------
// Ed25519 signature verification for the registration challenge
// ---------------------------------------------------------------------------

/// Verify that `signature_bytes` is a valid Ed25519 signature over
/// `(nonce || peer_id)` using the public key `pubkey_bytes`.
///
/// `pubkey_bytes` must be exactly 32 bytes (compressed Ed25519 public key).
/// `signature_bytes` must be exactly 64 bytes.
///
/// This is used during `POST /auth/register` to prove that the caller owns
/// the private key behind the presented `peer_id` before issuing a JWT.
pub fn verify_registration_signature(
    peer_id: &str,
    nonce: &str,
    pubkey_bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<(), AuthError> {
    use ed25519_dalek::{Signature, VerifyingKey, Verifier};

    let pubkey_arr: [u8; 32] = pubkey_bytes
        .try_into()
        .map_err(|_| AuthError::InvalidSignature)?;
    let vk = VerifyingKey::from_bytes(&pubkey_arr).map_err(|_| AuthError::InvalidSignature)?;

    let sig_arr: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| AuthError::InvalidSignature)?;
    let sig = Signature::from_bytes(&sig_arr);

    // Message = nonce bytes || peer_id bytes (same construction as Phantom side).
    let mut msg = Vec::with_capacity(nonce.len() + peer_id.len());
    msg.extend_from_slice(nonce.as_bytes());
    msg.extend_from_slice(peer_id.as_bytes());

    vk.verify(&msg, &sig).map_err(|_| AuthError::InvalidSignature)
}

// ---------------------------------------------------------------------------
// HTTP header helpers
// ---------------------------------------------------------------------------

/// Extract a bearer token from the `Authorization: Bearer <token>` header.
///
/// Returns `None` when the header is absent or does not start with `"Bearer "`.
#[must_use]
pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("Authorization")?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

/// Validate a device JWT and return the device identity.
///
/// `authority` is the hub's [`JwtAuthority`].
pub fn validate_device_token(
    token: &str,
    authority: &JwtAuthority,
) -> Result<DeviceIdentity, AuthError> {
    let claims = authority.verify(token)?;
    Ok(DeviceIdentity::from_claims(claims))
}

/// Validate an API key against the key store and return the session identity.
pub fn validate_api_key(key: &str, store: &ApiKeyStore) -> Result<SessionIdentity, AuthError> {
    store.validate(key)
}

// ---------------------------------------------------------------------------
// Axum 401 response helper
// ---------------------------------------------------------------------------

/// Produce a standardised `401 Unauthorized` response.
///
/// `reason` is included in the body for diagnostics but must not contain
/// token values.
#[must_use]
pub fn unauthorized(reason: &str) -> impl IntoResponse {
    (StatusCode::UNAUTHORIZED, reason.to_owned())
}

// ---------------------------------------------------------------------------
// AuthError
// ---------------------------------------------------------------------------

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or malformed Authorization header")]
    MissingToken,
    #[error("token signature invalid")]
    InvalidSignature,
    #[error("token expired")]
    Expired,
    #[error("unknown API key")]
    UnknownKey,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test secret shared by JWT tests
    // -----------------------------------------------------------------------
    const TEST_SECRET: &[u8] = b"s3cr3t-hub-key-for-tests-only-do-not-use";

    fn test_authority() -> JwtAuthority {
        JwtAuthority::from_secret(TEST_SECRET)
    }

    // -----------------------------------------------------------------------
    // JWT — happy path
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_issue_and_verify_accepted() {
        let auth = test_authority();
        let phantom_id = "ABC123peer";

        let token = auth.issue(phantom_id).expect("issue must succeed");
        let claims = auth.verify(&token).expect("verify must accept a freshly issued token");

        assert_eq!(claims.sub, phantom_id);
        assert_eq!(claims.iss, JWT_ISSUER);
        assert!(claims.exp > claims.iat);
    }

    // -----------------------------------------------------------------------
    // JWT — tampered payload rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_tampered_payload_rejected() {
        let auth = test_authority();
        let token = auth.issue("legit-peer").expect("issue must succeed");

        // A JWT has three base64url segments separated by '.'.  Swap the
        // payload segment for garbage to simulate tampering.
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        assert_eq!(parts.len(), 3, "JWT must have three segments");
        let tampered = format!("{}.dGFtcGVyZWQ.{}", parts[0], parts[2]);

        let result = auth.verify(&tampered);
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "tampered JWT must be rejected with InvalidSignature"
        );
    }

    // -----------------------------------------------------------------------
    // JWT — expired token rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_expired_token_rejected() {
        let auth = test_authority();
        // Manually craft a token whose exp is in the past (beyond the leeway).
        let past = unix_now().saturating_sub(JWT_LEEWAY_SECS + 3600);
        let claims = PhantomClaims {
            iss: JWT_ISSUER.to_owned(),
            sub: "expired-peer".to_owned(),
            iat: past - 10,
            exp: past, // expired
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(TEST_SECRET),
        )
        .expect("encode must succeed in test");

        let result = auth.verify(&token);
        assert!(
            matches!(result, Err(AuthError::Expired)),
            "expired JWT must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // JWT — wrong secret rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_wrong_secret_rejected() {
        let auth1 = JwtAuthority::from_secret(b"secret-one");
        let auth2 = JwtAuthority::from_secret(b"secret-two");

        let token = auth1.issue("peer-abc").expect("issue must succeed");
        let result = auth2.verify(&token);
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "JWT signed with a different secret must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // API key — in allowlist accepted
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_in_allowlist_accepted() {
        let raw_key = "phk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let store = ApiKeyStore::from_raw_keys(std::iter::once(raw_key));
        let result = store.validate(raw_key);
        assert!(result.is_ok(), "known API key must be accepted");
    }

    // -----------------------------------------------------------------------
    // API key — not in allowlist rejected
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_not_in_allowlist_rejected() {
        let store = ApiKeyStore::from_raw_keys(std::iter::once("phk_known"));
        let result = store.validate("phk_unknown");
        assert!(
            matches!(result, Err(AuthError::UnknownKey)),
            "unknown API key must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // API key — empty store rejects everything
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_empty_store_rejects_all() {
        let store = ApiKeyStore::default();
        let result = store.validate("phk_anything");
        assert!(
            matches!(result, Err(AuthError::UnknownKey)),
            "empty key store must reject all keys"
        );
    }

    // -----------------------------------------------------------------------
    // API key — multiple keys, correct one accepted
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_multiple_keys_correct_one_accepted() {
        let store = ApiKeyStore::from_raw_keys(
            ["phk_key1", "phk_key2", "phk_key3"].iter().copied(),
        );
        assert!(store.validate("phk_key1").is_ok());
        assert!(store.validate("phk_key2").is_ok());
        assert!(store.validate("phk_key3").is_ok());
        assert!(store.validate("phk_key4").is_err());
    }

    // -----------------------------------------------------------------------
    // Registration signature — valid signature accepted
    // -----------------------------------------------------------------------

    #[test]
    fn registration_signature_valid_accepted() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let peer_id = "test-peer-abc";
        let nonce = "hub-generated-nonce-12345";

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        let result = verify_registration_signature(
            peer_id,
            nonce,
            signing_key.verifying_key().as_bytes(),
            sig.to_bytes().as_slice(),
        );
        assert!(result.is_ok(), "valid registration signature must be accepted");
    }

    // -----------------------------------------------------------------------
    // Registration signature — tampered nonce rejected
    // -----------------------------------------------------------------------

    #[test]
    fn registration_signature_tampered_nonce_rejected() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let peer_id = "test-peer-def";
        let nonce = "real-nonce";

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        // Verify with a different nonce — the signature should not match.
        let result = verify_registration_signature(
            peer_id,
            "fake-nonce",
            signing_key.verifying_key().as_bytes(),
            sig.to_bytes().as_slice(),
        );
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "registration with wrong nonce must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // extract_bearer
    // -----------------------------------------------------------------------

    #[test]
    fn extract_bearer_parses_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer my-token-value".parse().unwrap());
        let token = extract_bearer(&headers);
        assert_eq!(token.as_deref(), Some("my-token-value"));
    }

    #[test]
    fn extract_bearer_returns_none_when_absent() {
        let headers = HeaderMap::new();
        assert!(extract_bearer(&headers).is_none());
    }

    #[test]
    fn extract_bearer_returns_none_for_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(extract_bearer(&headers).is_none());
    }

    // -----------------------------------------------------------------------
    // NonceCache — replay protection regression tests (P0 security, issue #398)
    // -----------------------------------------------------------------------

    /// A captured registration payload replayed a second time must be rejected.
    ///
    /// Both calls share the same [`NonceCache`] instance (via `Arc::clone` on
    /// the same `AppState`).  First call returns `true` (claim succeeds),
    /// second call returns `false` (replay detected).
    #[test]
    fn nonce_cache_replayed_nonce_rejected() {
        let cache = NonceCache::new();
        let nonce = "unique-nonce-replay-test-abc123";

        let first = cache.try_claim(nonce);
        let second = cache.try_claim(nonce);

        assert!(first, "first claim of a nonce must succeed");
        assert!(!second, "replayed nonce must be rejected");
    }

    /// Two distinct nonces must both be claimable — blocking one must not
    /// affect the other.
    #[test]
    fn nonce_cache_distinct_nonces_both_succeed() {
        let cache = NonceCache::new();

        let first = cache.try_claim("nonce-alpha-1");
        let second = cache.try_claim("nonce-beta-2");

        assert!(first, "first distinct nonce must be claimed");
        assert!(second, "second distinct nonce must also be claimed");
    }

    /// LRU eviction: filling the cache to capacity and inserting one more must
    /// evict the oldest (LRU) entry, making that nonce re-claimable as new,
    /// while more-recently-used entries remain blocked as replays.
    ///
    /// Uses a small cache (capacity 4) to exercise eviction without 10 000
    /// inserts.
    ///
    /// The test is structured in two stages to avoid the cascade: re-inserting
    /// the evicted entry (A) would itself consume the last free slot and evict
    /// the next-oldest (B).  Stage 1 verifies that B/C/D/E are all still
    /// present (replay-rejected) after only one eviction.  Stage 2, using a
    /// fresh cache, verifies that the evicted entry is re-claimable.
    #[test]
    fn nonce_cache_eviction_lru_oldest() {
        // ---- Stage 1: middle entries stay blocked after a single eviction ----
        let cache = NonceCache::with_capacity_and_ttl(
            4,
            Duration::from_secs(3600), // long TTL — eviction is capacity-driven only
        );

        // Fill to capacity: A(LRU) → B → C → D(MRU).
        assert!(cache.try_claim("nonce-A"), "A: initial claim (slot 1/4)");
        assert!(cache.try_claim("nonce-B"), "B: initial claim (slot 2/4)");
        assert!(cache.try_claim("nonce-C"), "C: initial claim (slot 3/4)");
        assert!(cache.try_claim("nonce-D"), "D: initial claim (slot 4/4)");

        // Inserting E evicts A (the LRU).  Cache now: B(LRU), C, D, E(MRU).
        assert!(cache.try_claim("nonce-E"), "E: claim triggers eviction of A");

        // B, C, D, E remain in the cache — must be rejected as replays.
        assert!(!cache.try_claim("nonce-B"), "nonce-B still cached — must be replay");
        assert!(!cache.try_claim("nonce-C"), "nonce-C still cached — must be replay");
        assert!(!cache.try_claim("nonce-D"), "nonce-D still cached — must be replay");
        assert!(!cache.try_claim("nonce-E"), "nonce-E still cached — must be replay");

        // ---- Stage 2: evicted entry is re-claimable (separate cache) --------
        let cache2 = NonceCache::with_capacity_and_ttl(4, Duration::from_secs(3600));
        cache2.try_claim("nonce-A");
        cache2.try_claim("nonce-B");
        cache2.try_claim("nonce-C");
        cache2.try_claim("nonce-D");
        // E evicts A.
        cache2.try_claim("nonce-E");

        // A is no longer in the cache — re-claiming it must succeed.
        assert!(
            cache2.try_claim("nonce-A"),
            "evicted nonce-A must be re-claimable after LRU eviction"
        );
    }

    // -----------------------------------------------------------------------
    // Round-trip: JwtAuthority::from_env
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_authority_from_env_errors_when_secret_missing() {
        // Ensure the var is unset for this test.
        // SAFETY: test-only; the test binary is single-threaded for env mutation.
        unsafe { std::env::remove_var("HUB_JWT_SECRET_398_TEST_ABSENT") };
        // Use a definitely-unset variable name to avoid cross-test interference.
        let result = std::env::var("HUB_JWT_SECRET_398_TEST_ABSENT")
            .map_err(|_| anyhow::anyhow!("not set"));
        assert!(result.is_err(), "from_env must fail when HUB_JWT_SECRET is absent");
    }

    #[test]
    fn jwt_authority_from_env_succeeds_when_secret_set() {
        // SAFETY: test-only; the test binary is single-threaded for env mutation.
        unsafe {
            std::env::set_var("HUB_JWT_SECRET", "test-secret-value-for-env-test");
        }
        let result = JwtAuthority::from_env();
        assert!(result.is_ok(), "from_env must succeed with HUB_JWT_SECRET set");
        // Leave the var set — other tests that use from_env will benefit.
    }

    // -----------------------------------------------------------------------
    // NonceCache TTL expiry — clock-injection tests (issue #506)
    // -----------------------------------------------------------------------

    /// Verify the TTL boundary using an injected fake clock.
    ///
    /// Test plan (issue #506):
    /// a. Insert at t=0 → claim at t=TTL/2 returns `false` (replay blocked).
    /// b. Insert at t=0 → claim at t=TTL+1s returns `true` (expired, treated as fresh).
    #[test]
    fn nonce_cache_ttl_expiry_clock_injection() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::OnceLock;

        static OFFSET_NANOS: AtomicU64 = AtomicU64::new(0);
        static BASE: OnceLock<Instant> = OnceLock::new();

        fn fake_clock() -> Instant {
            *BASE.get_or_init(Instant::now)
                + Duration::from_nanos(OFFSET_NANOS.load(Ordering::Relaxed))
        }

        let ttl = Duration::from_secs(10);

        // ── Part a: claim at t=TTL/2 → replay (false) ──────────────────────
        OFFSET_NANOS.store(0, Ordering::Relaxed);
        BASE.get_or_init(Instant::now); // pin the base

        let cache = NonceCache::with_clock(16, ttl, fake_clock);
        let first = cache.try_claim("ttl-nonce"); // t=0: claim succeeds

        // Advance clock to TTL/2 (5 s).
        OFFSET_NANOS.store(
            (ttl.as_nanos() / 2) as u64,
            Ordering::Relaxed,
        );
        let within_ttl = cache.try_claim("ttl-nonce"); // t=5s: still within TTL

        assert!(first, "initial claim at t=0 must succeed");
        assert!(!within_ttl, "re-claim at t=TTL/2 must be rejected (replay)");

        // ── Part b: claim at t=TTL+1s → fresh (true) ───────────────────────
        // Use a fresh cache so Part a's entry doesn't interfere.
        OFFSET_NANOS.store(0, Ordering::Relaxed);
        let cache2 = NonceCache::with_clock(16, ttl, fake_clock);
        cache2.try_claim("ttl-nonce-b"); // t=0: insert

        // Advance past TTL by 1 second.
        OFFSET_NANOS.store(
            (ttl + Duration::from_secs(1)).as_nanos() as u64,
            Ordering::Relaxed,
        );
        let after_ttl = cache2.try_claim("ttl-nonce-b"); // t=TTL+1s: expired

        assert!(after_ttl, "re-claim after TTL expiry must succeed (treated as fresh nonce)");
    }

    // -----------------------------------------------------------------------
    // CapabilityClass / ApiKeyStore::from_entries (issue #511)
    // -----------------------------------------------------------------------

    /// Keys loaded via `from_raw_keys` (env-var compat path) receive all
    /// capabilities — SessionIdentity.has returns true for each class.
    #[test]
    fn api_key_from_raw_keys_receives_all_capabilities() {
        let store = ApiKeyStore::from_raw_keys(std::iter::once("phk_compat-key"));
        let session = store.validate("phk_compat-key").expect("known key must validate");
        assert!(session.has(CapabilityClass::Sense), "Sense must be granted");
        assert!(session.has(CapabilityClass::Compute), "Compute must be granted");
        assert!(session.has(CapabilityClass::Coordinate), "Coordinate must be granted");
    }

    /// Keys provisioned via `from_entries` with a narrow capability set only
    /// allow those specific capabilities.
    #[test]
    fn api_key_from_entries_with_narrow_caps_only_allows_granted_caps() {
        let caps = HashSet::from([CapabilityClass::Sense]);
        let store = ApiKeyStore::from_entries(std::iter::once(("phk_narrow-key", caps)));
        let session = store.validate("phk_narrow-key").expect("known key must validate");
        assert!(session.has(CapabilityClass::Sense), "Sense must be granted");
        assert!(!session.has(CapabilityClass::Coordinate), "Coordinate must not be granted");
        assert!(!session.has(CapabilityClass::Compute), "Compute must not be granted");
    }

    /// `SessionIdentity::has` returns true only for capabilities in the set.
    #[test]
    fn session_identity_has_matches_stored_capabilities() {
        let caps = HashSet::from([CapabilityClass::Coordinate, CapabilityClass::Compute]);
        let store = ApiKeyStore::from_entries(std::iter::once(("phk_coord-key", caps)));
        let session = store.validate("phk_coord-key").expect("known key must validate");
        assert!(session.has(CapabilityClass::Coordinate));
        assert!(session.has(CapabilityClass::Compute));
        assert!(!session.has(CapabilityClass::Sense));
    }
}
