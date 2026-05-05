//! `phantom-hub` — Railway-hosted MCP fleet control hub.
//!
//! # Phase 1 scope
//!
//! - [`registry`] — connection registry with live `ConnState` per Phantom
//! - [`router`] — JSON-RPC frame router with id rewriting, timeout, backpressure
//! - [`phantom_endpoint`] — WSS upgrade handler: binary relay-envelope
//!   handshake (HELLO/HELLO_ACK), JWT verification, inbound/outbound loop
//! - [`health`] — `GET /healthz` liveness probe (always-on)
//! - [`auth`] — JWT device token issuance/verification and API key validation
//!   (live, issue #398)
//! - [`mcp_endpoint`] — `POST /mcp` and `GET /mcp/sse` stubs (issue #397)
//!
//! # Authentication (issue #398)
//!
//! [`auth::JwtAuthority`] and [`auth::ApiKeyStore`] are initialised from
//! environment variables at startup and injected into [`AppState`].  Every
//! handler receives [`AppState`] via `axum::extract::State`.
//!
//! See [`auth`] for the full token model, library choice, and threat model.
//!
//! # Protocol
//!
//! `GET /phantom/connect` speaks **binary relay-envelope** — the same
//! protocol implemented by `phantom-net::RelayClient` / `phantom-mcp::hub_listener`.
//! See [`phantom_endpoint`] for the handshake sequence.
//!
//! # Debug endpoint
//!
//! When the `HUB_REGISTRY_DEBUG` environment variable is set to `1`, a
//! `GET /registry` endpoint is available that returns the list of currently
//! connected Phantom peer IDs.  The route is only registered when the env
//! flag is set, AND the handler requires a valid API key in the
//! `Authorization: Bearer` header — env-only access is not sufficient (issue #502).

pub mod auth;
pub mod health;
pub mod mcp_endpoint;
pub mod phantom_endpoint;
pub mod rate_limit;
pub mod registry;
pub mod router;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{Router, extract::State, Json};
use rate_limit::IpRateLimiter;
use registry::SharedRegistry;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

// ---------------------------------------------------------------------------
// AppState — shared across all request handlers
// ---------------------------------------------------------------------------

/// Shared hub state injected into every Axum handler.
///
/// Cloning this is cheap — all expensive fields are behind [`Arc`].
///
/// The `nonce_cache` field enforces single-use semantics on registration
/// nonces (replay protection, issue #398).  It is wrapped in `Arc` so that
/// cloned `AppState` values — including those shared across unit-test calls —
/// all operate on the same underlying cache.
#[derive(Clone)]
pub struct AppState {
    /// JWT authority — issues and verifies Phantom device tokens.
    pub jwt: Arc<auth::JwtAuthority>,
    /// API key store — validates Claude session API keys.
    pub api_keys: Arc<auth::ApiKeyStore>,
    /// Nonce replay-protection cache.  Every nonce presented at registration
    /// is recorded here; a second presentation of the same nonce within the
    /// TTL window is rejected with `409 Conflict`.
    pub nonce_cache: Arc<auth::NonceCache>,
    /// Per-IP rate limiter for `POST /auth/register`.
    ///
    /// Limits each IP to 10 registration attempts per 60-second window.
    /// Returns `429 Too Many Requests` when the limit is exceeded.
    pub register_limiter: Arc<IpRateLimiter>,
    /// Connection registry — tracks live Phantom WebSocket connections.
    pub registry: SharedRegistry,
    /// Per-IP rate limiter for the `/registry` debug endpoint (10 req/min).
    pub registry_rate_limiter: Arc<auth::IpRateLimiter>,
    /// Admin bearer token for the `/registry` debug endpoint.
    /// When `None` the endpoint is disabled regardless of `HUB_REGISTRY_DEBUG`.
    pub admin_token: Arc<auth::AdminToken>,
}

impl AppState {
    /// Construct [`AppState`] from environment variables.
    ///
    /// Reads `HUB_JWT_SECRET` (required), `HUB_API_KEYS` (optional),
    /// and `PHANTOM_HUB_ADMIN_TOKEN` (optional — disables `/registry` if absent).
    /// The [`auth::NonceCache`] is always initialised with production defaults
    /// (capacity [`auth::NONCE_CACHE_CAPACITY`], TTL [`auth::NONCE_CACHE_TTL`]).
    ///
    /// A background Tokio task is spawned to call
    /// [`IpRateLimiter::evict_stale`] every two minutes, preventing unbounded
    /// memory growth from dormant IP entries.
    ///
    /// # Errors
    ///
    /// Returns an error if `HUB_JWT_SECRET` is absent or empty.
    pub fn from_env() -> Result<Self> {
        let jwt = auth::JwtAuthority::from_env()?;
        let api_keys = auth::ApiKeyStore::from_env();
        let admin_token = auth::AdminToken::from_env();
        let register_limiter = Arc::new(IpRateLimiter::new(Duration::from_secs(60), 10));

        // Spawn a background task that prunes stale IP entries every 2 minutes.
        let limiter_for_task = Arc::clone(&register_limiter);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(120));
            loop {
                interval.tick().await;
                limiter_for_task.evict_stale();
            }
        });

        Ok(Self {
            jwt: Arc::new(jwt),
            api_keys: Arc::new(api_keys),
            nonce_cache: Arc::new(auth::NonceCache::new()),
            register_limiter,
            registry: registry::new_shared(),
            registry_rate_limiter: Arc::new(auth::IpRateLimiter::registry_default()),
            admin_token: Arc::new(admin_token),
        })
    }
}

// ---------------------------------------------------------------------------
// Debug registry endpoint
// ---------------------------------------------------------------------------

/// Handler for `GET /registry` — returns connected peer IDs.
///
/// Requires **all three** of:
/// 1. `HUB_REGISTRY_DEBUG=1` — the route is only registered when this is set.
/// 2. `PHANTOM_HUB_ADMIN_TOKEN` is configured — if absent the route is disabled
///    entirely (returns `503 Service Unavailable`).
/// 3. `Authorization: Bearer <admin_token>` — the presented token must match
///    `PHANTOM_HUB_ADMIN_TOKEN` exactly.
///
/// Additionally, each caller IP is rate-limited to 10 requests per minute.
/// Exceeding the limit returns `429 Too Many Requests`.
///
/// `ConnectInfo` is optional so that unit tests using `tower::ServiceExt::oneshot`
/// (which does not populate connection metadata) can still exercise this handler.
/// In production the server is started with `into_make_service_with_connect_info`
/// so a real remote address is always available.  When it is absent the handler
/// falls back to the IPv4 loopback address (`127.0.0.1`) for rate-limiting
/// purposes — harmless in production (loopback requests only come from the
/// machine itself) and necessary for test coverage.
async fn registry_debug(
    State(state): State<AppState>,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use std::net::{IpAddr, Ipv4Addr};

    // Guard 1: admin token must be configured in the environment.
    if !state.admin_token.is_configured() {
        tracing::warn!("registry_debug: PHANTOM_HUB_ADMIN_TOKEN not set — endpoint disabled");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "registry debug endpoint is not enabled",
        )
            .into_response();
    }

    // Determine the client IP for rate limiting.
    // Falls back to loopback when ConnectInfo is unavailable (unit tests).
    let client_ip: IpAddr = connect_info
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    // Guard 2: per-IP rate limit (10 req/min).
    if !state.registry_rate_limiter.check_and_record(client_ip) {
        tracing::warn!(%client_ip, "registry_debug: rate limit exceeded");
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }

    // Guard 3: admin bearer token must be present and correct.
    let presented = match auth::extract_bearer(&headers) {
        Some(k) => k,
        None => {
            tracing::warn!(%client_ip, "registry_debug: missing Authorization header");
            return (
                StatusCode::UNAUTHORIZED,
                "Authorization: Bearer <admin_token> required",
            )
                .into_response();
        }
    };
    if !state.admin_token.validate(&presented) {
        tracing::warn!(%client_ip, "registry_debug: invalid admin token");
        return (StatusCode::UNAUTHORIZED, "invalid admin token").into_response();
    }

    let reg = state.registry.read().await;
    let peers: Vec<serde_json::Value> = reg
        .list_online()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.0,
                "host": p.host,
                "version": p.version,
                "last_seen_secs_ago": p.last_seen_secs_ago,
            })
        })
        .collect();
    Json(serde_json::json!({ "phantoms": peers })).into_response()
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the application router with `state` injected into every handler.
///
/// Route layout:
/// - `GET  /healthz`         — liveness / readiness probe
/// - `POST /auth/register`   — Phantom registration + JWT issuance (issue #398)
/// - `GET  /phantom/connect` — Phantom-side WSS dial-in (binary relay-envelope)
/// - `POST /mcp`             — Claude-side MCP JSON-RPC (stub, issue #397)
/// - `GET  /mcp/sse`         — Claude-side MCP SSE transport (stub, issue #397)
/// - `GET  /registry`        — debug endpoint (only when `HUB_REGISTRY_DEBUG=1`)
pub fn build_router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/healthz", axum::routing::get(health::healthz))
        .route(
            "/auth/register",
            axum::routing::post(phantom_endpoint::register),
        )
        .route(
            "/phantom/connect",
            axum::routing::get(phantom_endpoint::connect),
        )
        .route("/mcp", axum::routing::post(mcp_endpoint::handle_jsonrpc))
        .route("/mcp/sse", axum::routing::get(mcp_endpoint::handle_sse))
        .with_state(state.clone());

    if std::env::var("HUB_REGISTRY_DEBUG").as_deref() == Ok("1") {
        app = app.route(
            "/registry",
            axum::routing::get(registry_debug).with_state(state),
        );
    }

    app
}

/// Bind and serve the hub on `addr` until the process is killed.
///
/// # Errors
///
/// Returns an error if `HUB_JWT_SECRET` is absent, the TCP listener cannot
/// bind to `addr`, or the server fails after startup.
pub async fn serve(addr: SocketAddr) -> Result<()> {
    let state = AppState::from_env()?;
    let app = build_router(state);
    let listener = TcpListener::bind(addr).await?;
    info!("phantom-hub listening on {}", addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"phantom-hub-lib-test-secret";
    const TEST_API_KEY: &str = "phk_lib-test-key";
    const TEST_ADMIN_TOKEN: &str = "admin-super-secret-token-for-tests";

    fn test_state_with_key(key: &str) -> AppState {
        AppState {
            jwt: Arc::new(auth::JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(auth::ApiKeyStore::from_raw_keys(std::iter::once(key))),
            nonce_cache: Arc::new(auth::NonceCache::new()),
            register_limiter: Arc::new(rate_limit::IpRateLimiter::new(
                std::time::Duration::from_secs(60),
                10,
            )),
            registry: registry::new_shared(),
            registry_rate_limiter: Arc::new(auth::IpRateLimiter::registry_default()),
            admin_token: Arc::new(auth::AdminToken::from_token(TEST_ADMIN_TOKEN)),
        }
    }

    /// Build a state with a tight rate limit (N requests per minute) for rate-limit tests.
    fn test_state_with_rate_limit(key: &str, max_requests: usize) -> AppState {
        AppState {
            jwt: Arc::new(auth::JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(auth::ApiKeyStore::from_raw_keys(std::iter::once(key))),
            nonce_cache: Arc::new(auth::NonceCache::new()),
            registry: registry::new_shared(),
            registry_rate_limiter: Arc::new(auth::IpRateLimiter::new(
                max_requests,
                std::time::Duration::from_secs(60),
            )),
            admin_token: Arc::new(auth::AdminToken::from_token(TEST_ADMIN_TOKEN)),
        }
    }

    /// Build a state where PHANTOM_HUB_ADMIN_TOKEN is not configured.
    fn test_state_no_admin_token(key: &str) -> AppState {
        AppState {
            jwt: Arc::new(auth::JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(auth::ApiKeyStore::from_raw_keys(std::iter::once(key))),
            nonce_cache: Arc::new(auth::NonceCache::new()),
            registry: registry::new_shared(),
            registry_rate_limiter: Arc::new(auth::IpRateLimiter::registry_default()),
            admin_token: Arc::new(auth::AdminToken::disabled()),
        }
    }

    /// Build a router with the /registry endpoint always wired (simulates
    /// `HUB_REGISTRY_DEBUG=1`) so tests don't rely on env var mutation.
    fn build_router_with_debug(state: AppState) -> axum::Router {
        let mut app = axum::Router::new()
            .route("/healthz", axum::routing::get(health::healthz))
            .route(
                "/auth/register",
                axum::routing::post(phantom_endpoint::register),
            )
            .route(
                "/phantom/connect",
                axum::routing::get(phantom_endpoint::connect),
            )
            .route("/mcp", axum::routing::post(mcp_endpoint::handle_jsonrpc))
            .route("/mcp/sse", axum::routing::get(mcp_endpoint::handle_sse))
            .with_state(state.clone());
        // Always wire /registry for these tests (simulates HUB_REGISTRY_DEBUG=1).
        app = app.route(
            "/registry",
            axum::routing::get(registry_debug).with_state(state),
        );
        app
    }

    /// Build a router without the /registry debug route — simulates the
    /// production path when `HUB_REGISTRY_DEBUG` is absent, without needing
    /// to mutate the environment.
    fn build_router_without_debug(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/healthz", axum::routing::get(health::healthz))
            .route(
                "/auth/register",
                axum::routing::post(phantom_endpoint::register),
            )
            .route(
                "/phantom/connect",
                axum::routing::get(phantom_endpoint::connect),
            )
            .route("/mcp", axum::routing::post(mcp_endpoint::handle_jsonrpc))
            .route("/mcp/sse", axum::routing::get(mcp_endpoint::handle_sse))
            .with_state(state)
    }

    // -----------------------------------------------------------------------
    // /registry: valid admin token + env set → 200 (issue #502)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn registry_debug_with_valid_key_returns_200() {
        let state = test_state_with_key(TEST_API_KEY);
        let app = build_router_with_debug(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/registry")
                    .header("Authorization", format!("Bearer {TEST_ADMIN_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "valid admin token must receive 200 from /registry"
        );
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("phantoms").is_some(), "expected phantoms key: {val}");
    }

    // -----------------------------------------------------------------------
    // /registry: no token → 401 (issue #502)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn registry_debug_without_api_key_returns_401() {
        let state = test_state_with_key(TEST_API_KEY);
        let app = build_router_with_debug(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/registry")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated request to /registry must return 401"
        );
    }

    // -----------------------------------------------------------------------
    // /registry: env unset → 404 (route not registered)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn registry_debug_when_env_unset_returns_404() {
        // Use the no-debug builder directly — no env mutation required.
        let state = test_state_with_key(TEST_API_KEY);
        let app = build_router_without_debug(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/registry")
                    .header("Authorization", format!("Bearer {TEST_ADMIN_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "when HUB_REGISTRY_DEBUG is unset /registry must not exist"
        );
    }

    // -----------------------------------------------------------------------
    // /registry: env set + wrong admin token → 401 (issue #532)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn registry_debug_with_invalid_api_key_returns_401() {
        let state = test_state_with_key(TEST_API_KEY);
        let app = build_router_with_debug(state);

        // Bearer token that does not match the admin token.
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/registry")
                    .header("Authorization", "Bearer not-a-real-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "request with wrong admin token must return 401"
        );
    }

    // -----------------------------------------------------------------------
    // Bug fix: /registry requires admin token (PHANTOM_HUB_ADMIN_TOKEN)
    // -----------------------------------------------------------------------

    /// When PHANTOM_HUB_ADMIN_TOKEN is not set, /registry must be disabled
    /// even if the route is registered (HUB_REGISTRY_DEBUG=1).
    #[tokio::test]
    async fn registry_endpoint_requires_admin_token() {
        // State with no admin token configured.
        let state = test_state_no_admin_token(TEST_API_KEY);
        let app = build_router_with_debug(state);

        // Even a valid API key must not bypass the admin-token requirement.
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/registry")
                    .header("Authorization", format!("Bearer {TEST_API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // When PHANTOM_HUB_ADMIN_TOKEN is absent the endpoint returns 503.
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "when admin token is not configured /registry must return 503, got: {}",
            resp.status()
        );
    }

    // -----------------------------------------------------------------------
    // Bug fix: /registry is rate-limited (10 req/min per IP)
    // -----------------------------------------------------------------------

    /// After exhausting the rate limit the endpoint must return 429.
    ///
    /// Uses a tight limit of 2 requests to avoid 10+ round-trips.
    #[tokio::test]
    async fn registry_endpoint_rate_limited() {
        // Rate limiter: allow only 2 requests before throttling.
        let state = test_state_with_rate_limit(TEST_API_KEY, 2);

        // Use the shared state so all three requests hit the same limiter.
        let app1 = build_router_with_debug(state.clone());
        let app2 = build_router_with_debug(state.clone());
        let app3 = build_router_with_debug(state);

        let make_req = || {
            Request::builder()
                .method(Method::GET)
                .uri("/registry")
                .header("Authorization", format!("Bearer {TEST_ADMIN_TOKEN}"))
                .body(Body::empty())
                .unwrap()
        };

        let resp1 = app1.oneshot(make_req()).await.unwrap();
        let resp2 = app2.oneshot(make_req()).await.unwrap();
        let resp3 = app3.oneshot(make_req()).await.unwrap();

        assert_eq!(resp1.status(), StatusCode::OK, "first request must succeed");
        assert_eq!(resp2.status(), StatusCode::OK, "second request must succeed");
        assert_eq!(
            resp3.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "third request beyond limit must return 429"
        );
    }
}
