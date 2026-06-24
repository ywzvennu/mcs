//! Abuse-protection limits: per-IP rate limiting and resource caps (#100).
//!
//! This module is the **abuse-protection surface** of the API. It bundles three
//! independent guards, all enforced **per node** (see the scope note below):
//!
//! 1. **Per-IP HTTP rate limiting** ([`RateLimiter`]) — a token-bucket keyed by
//!    the client IP, applied to the abuse-prone routes (`/auth/nonce`,
//!    `/auth/verify`, `POST /seeks`, `POST /challenges`) by the
//!    [`rate_limit`] middleware. Exceeding the bucket yields **429 Too Many
//!    Requests** with a `Retry-After` header.
//! 2. **WebSocket connection caps** ([`WsConnectionRegistry`]) — a global cap on
//!    concurrent live-game sockets and a per-user cap, tracked by a shared
//!    counter that a [`WsConnectionGuard`] increments on connect and decrements
//!    on **every** close path (it decrements on `Drop`).
//! 3. **Per-user live-game cap** ([`LiveGameRegistry`]) — a ceiling on how many
//!    simultaneous live games one user may be a player in. Creation is rejected
//!    once a player is at the cap; the count is released when the game finishes
//!    (via the [`LiveGameCountingHook`] completion hook).
//!
//! # Per-node scope (cluster caveat)
//!
//! Every limit here is **per process**: the token buckets, the connection
//! counters, and the live-game counters all live in this node's memory. In a
//! multi-node deployment (behind a load balancer) a client's requests may be
//! spread across nodes, so the *effective* cluster-wide limit is up to
//! `N x limit` for `N` nodes. Cluster-wide limiting would need a shared store
//! (e.g. Redis token buckets / counters); that is deliberately left as future
//! work, mirroring how presence ([`crate::presence`]) and cluster membership are
//! structured. For a single node — the default deployment — these limits are
//! exact.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mcs_core::Outcome;
use mcs_domain::{Game, UserId};
use mcs_game::GameCompletionHook;
use std::net::SocketAddr;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A single token-bucket rate: a sustained rate plus an instantaneous burst.
///
/// A bucket refills at `per_second = replenish_per_minute / 60` tokens per
/// second up to a ceiling of `burst` tokens. A request costs one token; when the
/// bucket is empty the request is rejected with the time until the next token is
/// available. Setting `replenish_per_minute` to `0` disables the limit (every
/// request is allowed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitTier {
    /// How many tokens are replenished per minute (the sustained rate). `0`
    /// disables the limit entirely.
    pub replenish_per_minute: u32,
    /// The maximum number of tokens the bucket can hold — the largest burst a
    /// single IP may make before being throttled to the sustained rate.
    pub burst: u32,
}

impl RateLimitTier {
    /// Builds a tier from a per-minute rate, using that same value as the burst
    /// ceiling (so a fresh IP may spend a full minute's allowance at once, then
    /// is throttled to the steady rate).
    #[must_use]
    pub const fn per_minute(rate: u32) -> Self {
        Self {
            replenish_per_minute: rate,
            burst: rate,
        }
    }

    /// Whether this tier actually limits anything. A `replenish_per_minute` of
    /// `0` means "no limit".
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.replenish_per_minute > 0
    }

    /// Tokens replenished per second.
    fn per_second(&self) -> f64 {
        f64::from(self.replenish_per_minute) / 60.0
    }
}

/// The set of per-route rate-limit tiers plus the connection / game caps.
///
/// Built from the server's `[limits]` config (see `mcs-server`'s config) and
/// threaded into [`AppState`] via [`AppState::with_limits`]. Every field has a
/// sane default (see [`LimitsConfig::default`]); the whole struct can be
/// constructed with `..Default::default()`.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Per-IP rate for `GET /auth/nonce`.
    pub nonce: RateLimitTier,
    /// Per-IP rate for `POST /auth/verify`.
    pub verify: RateLimitTier,
    /// Per-IP rate for the game-creation routes (`POST /seeks`,
    /// `POST /challenges`).
    pub create: RateLimitTier,
    /// The global ceiling on concurrent live-game WebSocket connections on this
    /// node. `0` disables the global cap.
    pub max_ws_connections: u32,
    /// The per-user ceiling on concurrent live-game WebSocket connections on this
    /// node. `0` disables the per-user cap.
    pub max_ws_connections_per_user: u32,
    /// The per-user ceiling on simultaneous live games a user may be a player in
    /// on this node. `0` disables the cap.
    pub max_games_per_user: u32,
    /// An optional trusted proxy header (e.g. `"x-forwarded-for"` or
    /// `"x-real-ip"`) to read the real client IP from. `None` (the default) uses
    /// the socket peer address, which is correct when the server is exposed
    /// directly. **Only** set this when a trusted reverse proxy sets the header,
    /// since a client can otherwise spoof it to evade per-IP limits.
    pub trusted_proxy_header: Option<String>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            // Conservative auth-flow defaults: a wallet needs only a couple of
            // nonce/verify round-trips, so 10/min and 20/min per IP are generous
            // for a human yet throttle a script.
            nonce: RateLimitTier::per_minute(10),
            verify: RateLimitTier::per_minute(20),
            // Game creation is heavier; 30/min/IP still allows brisk play.
            create: RateLimitTier::per_minute(30),
            // 10k concurrent sockets node-wide, 20 per user — enough for a player
            // on several devices/tabs, low enough to bound a single account's
            // resource use.
            max_ws_connections: 10_000,
            max_ws_connections_per_user: 20,
            // A user may be in up to 50 simultaneous live games.
            max_games_per_user: 50,
            // No proxy trusted by default: read the socket peer address.
            trusted_proxy_header: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Token-bucket rate limiter
// ---------------------------------------------------------------------------

/// The mutable state of one IP's token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Current whole-and-fractional token count, capped at the tier `burst`.
    tokens: f64,
    /// When `tokens` was last refilled.
    last_refill: Instant,
}

/// The outcome of a [`RateLimiter::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request is within the limit and may proceed.
    Allow,
    /// The request exceeds the limit; retry after the given whole-second hint.
    Throttle {
        /// Seconds the caller should wait before retrying (the `Retry-After`
        /// value). Always at least `1`.
        retry_after_secs: u64,
    },
}

/// A per-IP token-bucket rate limiter for one route tier.
///
/// Cheap to clone (it shares its bucket map through an [`Arc`]); a single
/// limiter is built per rate-limited route and stored in [`AppState`]. The
/// buckets are a plain in-memory map under a [`Mutex`] — fine for the request
/// rates a single node sees, and entirely per-node (see the module note).
#[derive(Clone)]
pub struct RateLimiter {
    tier: RateLimitTier,
    buckets: Arc<Mutex<HashMap<IpAddr, Bucket>>>,
}

impl RateLimiter {
    /// Builds a limiter enforcing `tier`.
    #[must_use]
    pub fn new(tier: RateLimitTier) -> Self {
        Self {
            tier,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Charges one token to `ip`'s bucket, returning whether the request is
    /// allowed.
    ///
    /// A disabled tier (`replenish_per_minute == 0`) always allows. Otherwise the
    /// bucket is lazily refilled based on elapsed time, then a token is spent if
    /// one is available; if not, the caller is throttled with a `Retry-After`
    /// hint computed from the deficit and the refill rate.
    pub fn check(&self, ip: IpAddr) -> RateDecision {
        if !self.tier.is_enabled() {
            return RateDecision::Allow;
        }

        let burst = f64::from(self.tier.burst);
        let per_second = self.tier.per_second();
        let now = Instant::now();

        let mut buckets = self.buckets.lock().expect("rate-limiter lock poisoned");
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: burst,
            last_refill: now,
        });

        // Refill based on elapsed time, capped at the burst ceiling.
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_second).min(burst);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateDecision::Allow
        } else {
            // Time until one whole token is available again.
            let deficit = 1.0 - bucket.tokens;
            let wait = if per_second > 0.0 {
                deficit / per_second
            } else {
                1.0
            };
            // Round up to at least one second so a client always backs off.
            let retry_after_secs = wait.ceil().max(1.0) as u64;
            RateDecision::Throttle { retry_after_secs }
        }
    }

    /// The number of distinct IPs currently tracked (test/observability helper).
    #[must_use]
    pub fn tracked_ips(&self) -> usize {
        self.buckets
            .lock()
            .expect("rate-limiter lock poisoned")
            .len()
    }
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("tier", &self.tier)
            .field("tracked_ips", &self.tracked_ips())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Client-IP extraction
// ---------------------------------------------------------------------------

/// Resolves the client IP for rate limiting.
///
/// When `trusted_header` is set (the deployment is behind a reverse proxy that
/// populates it), the **first** IP in that header is used — for
/// `X-Forwarded-For` this is the original client, the rest being the proxy
/// chain. When the header is absent or unset, the socket peer address from
/// [`ConnectInfo`] is used. If neither is available the request cannot be keyed
/// and `None` is returned (the middleware then fails open and allows it).
#[must_use]
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    trusted_header: Option<&str>,
) -> Option<IpAddr> {
    if let Some(name) = trusted_header {
        if let Some(value) = headers.get(name) {
            if let Ok(text) = value.to_str() {
                // X-Forwarded-For is a comma-separated list, client-first.
                if let Some(first) = text.split(',').next() {
                    if let Ok(ip) = first.trim().parse::<IpAddr>() {
                        return Some(ip);
                    }
                }
            }
        }
    }
    peer
}

// ---------------------------------------------------------------------------
// Rate-limit middleware
// ---------------------------------------------------------------------------

/// Which rate-limit tier a route is filed under, selecting the [`RateLimiter`]
/// the [`rate_limit`] middleware charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteLimit {
    /// `GET /auth/nonce`.
    Nonce,
    /// `POST /auth/verify`.
    Verify,
    /// The game-creation routes (`POST /seeks`, `POST /challenges`).
    Create,
}

/// Enforces the per-IP rate limit for `GET /auth/nonce`.
///
/// Thin wrapper around [`rate_limit`] pinning the [`RouteLimit::Nonce`] tier, so
/// it can be mounted with [`axum::middleware::from_fn_with_state`].
pub async fn rate_limit_nonce(
    state: State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    rate_limit(state, RouteLimit::Nonce, request, next).await
}

/// Enforces the per-IP rate limit for `POST /auth/verify`.
pub async fn rate_limit_verify(
    state: State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    rate_limit(state, RouteLimit::Verify, request, next).await
}

/// Enforces the per-IP rate limit for the game-creation routes (`POST /seeks`,
/// `POST /challenges`).
pub async fn rate_limit_create(
    state: State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    rate_limit(state, RouteLimit::Create, request, next).await
}

/// Resolves the client IP (honouring the configured trusted proxy header),
/// charges the matching [`RateLimiter`], and short-circuits with **429 Too Many
/// Requests** plus a `Retry-After` header when the bucket is empty.
///
/// Mounted on the abuse-prone routes by [`crate::router`] through the per-tier
/// wrappers above. The socket peer address is read from the request's
/// [`ConnectInfo`] extension (set by the server's
/// `into_make_service_with_connect_info`); a request whose IP cannot be
/// determined — no `ConnectInfo` and no trusted header — fails **open** (is
/// allowed) rather than blocking legitimate traffic.
async fn rate_limit(
    State(state): State<AppState>,
    which: RouteLimit,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let limiter = match which {
        RouteLimit::Nonce => state.nonce_limiter(),
        RouteLimit::Verify => state.verify_limiter(),
        RouteLimit::Create => state.create_limiter(),
    };

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let ip = client_ip(
        request.headers(),
        peer,
        state.limits().trusted_proxy_header.as_deref(),
    );

    if let Some(ip) = ip {
        if let RateDecision::Throttle { retry_after_secs } = limiter.check(ip) {
            return too_many_requests(retry_after_secs);
        }
    }

    next.run(request).await
}

/// Builds a **429 Too Many Requests** response carrying a `Retry-After` header
/// and an RFC 9457 `application/problem+json` body, matching the API's error
/// contract.
fn too_many_requests(retry_after_secs: u64) -> Response {
    let body = format!(
        r#"{{"type":"about:blank","title":"Too Many Requests","status":429,"detail":"rate limit exceeded; retry after {retry_after_secs}s"}}"#
    );
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::RETRY_AFTER, retry_after_secs.to_string()),
            (header::CONTENT_TYPE, "application/problem+json".to_owned()),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// WebSocket connection registry
// ---------------------------------------------------------------------------

/// Shared inner state of a [`WsConnectionRegistry`].
#[derive(Debug, Default)]
struct ConnectionCounts {
    /// Total live-game sockets open on this node.
    global: AtomicU64,
    /// Per-user open socket counts. Entries are removed when they hit zero so the
    /// map does not grow unbounded across many users over time.
    per_user: Mutex<HashMap<UserId, u64>>,
}

/// Tracks active live-game WebSocket connections against a global and a per-user
/// cap (#100).
///
/// Cheap to clone (shares its counters through an [`Arc`]); one instance lives in
/// [`AppState`]. The WebSocket handler calls [`try_open`](Self::try_open) before
/// upgrading: on success it receives a [`WsConnectionGuard`] whose `Drop`
/// releases the slot on **every** close path, so the counts cannot leak.
#[derive(Clone, Default)]
pub struct WsConnectionRegistry {
    counts: Arc<ConnectionCounts>,
    max_global: u32,
    max_per_user: u32,
}

impl WsConnectionRegistry {
    /// Builds a registry enforcing `max_global` total sockets and `max_per_user`
    /// per user. A `0` for either disables that particular cap.
    #[must_use]
    pub fn new(max_global: u32, max_per_user: u32) -> Self {
        Self {
            counts: Arc::new(ConnectionCounts::default()),
            max_global,
            max_per_user,
        }
    }

    /// Attempts to reserve a connection slot for `user`.
    ///
    /// Returns a [`WsConnectionGuard`] that holds the slot (and releases it on
    /// drop) when both caps allow, or `None` when either the global or the
    /// per-user cap is already reached — in which case the caller must reject the
    /// upgrade. The reservation is atomic: the global and per-user counts are
    /// only incremented together once both checks pass.
    #[must_use]
    pub fn try_open(&self, user: UserId) -> Option<WsConnectionGuard> {
        // Take the per-user lock first; it also serializes the global check so two
        // racing connects cannot both slip past a cap of N at count N-1.
        let mut per_user = self
            .counts
            .per_user
            .lock()
            .expect("ws registry lock poisoned");

        if self.max_global > 0 {
            let global = self.counts.global.load(Ordering::Acquire);
            if global >= u64::from(self.max_global) {
                return None;
            }
        }

        let user_count = per_user.entry(user).or_insert(0);
        if self.max_per_user > 0 && *user_count >= u64::from(self.max_per_user) {
            // Drop a freshly created zero entry so an over-cap probe does not leave
            // an empty slot behind.
            if *user_count == 0 {
                per_user.remove(&user);
            }
            return None;
        }

        *user_count += 1;
        self.counts.global.fetch_add(1, Ordering::AcqRel);

        Some(WsConnectionGuard {
            registry: self.clone(),
            user,
        })
    }

    /// The current global open-connection count (test/observability helper).
    #[must_use]
    pub fn global_count(&self) -> u64 {
        self.counts.global.load(Ordering::Acquire)
    }

    /// The current open-connection count for `user` (test/observability helper).
    #[must_use]
    pub fn user_count(&self, user: UserId) -> u64 {
        self.counts
            .per_user
            .lock()
            .expect("ws registry lock poisoned")
            .get(&user)
            .copied()
            .unwrap_or(0)
    }

    /// Releases one slot for `user`, called by the guard's `Drop`.
    fn release(&self, user: UserId) {
        let mut per_user = self
            .counts
            .per_user
            .lock()
            .expect("ws registry lock poisoned");
        if let Some(count) = per_user.get_mut(&user) {
            *count -= 1;
            if *count == 0 {
                per_user.remove(&user);
            }
        }
        self.counts.global.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for WsConnectionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConnectionRegistry")
            .field("global", &self.global_count())
            .field("max_global", &self.max_global)
            .field("max_per_user", &self.max_per_user)
            .finish()
    }
}

/// A scope guard that holds a reserved WebSocket connection slot.
///
/// Returned by [`WsConnectionRegistry::try_open`]; its `Drop` releases the slot
/// (decrementing the global and per-user counts) so a clean close, a client
/// drop, an actor stop, or an early-return all free the connection exactly once.
#[derive(Debug)]
pub struct WsConnectionGuard {
    registry: WsConnectionRegistry,
    user: UserId,
}

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        self.registry.release(self.user);
    }
}

// ---------------------------------------------------------------------------
// Per-user live-game registry
// ---------------------------------------------------------------------------

/// Tracks how many simultaneous live games each user is a player in, enforcing a
/// per-user cap (#100).
///
/// A user is counted once per live game they play. [`try_reserve`](Self::try_reserve)
/// is called for both players when a game is created; if either is already at the
/// cap, creation is refused. [`release`](Self::release) is called once per player
/// when the game finishes (wired through the [`LiveGameCountingHook`]). Cheap to
/// clone; shares its counts through an [`Arc`].
#[derive(Clone, Default)]
pub struct LiveGameRegistry {
    counts: Arc<Mutex<HashMap<UserId, u32>>>,
    max_per_user: u32,
}

impl LiveGameRegistry {
    /// Builds a registry capping each user at `max_per_user` simultaneous live
    /// games. `0` disables the cap.
    #[must_use]
    pub fn new(max_per_user: u32) -> Self {
        Self {
            counts: Arc::new(Mutex::new(HashMap::new())),
            max_per_user,
        }
    }

    /// Reserves a live-game slot for **both** players atomically.
    ///
    /// Returns `true` when both players are below the cap (and increments both
    /// counts), or `false` when either is at the cap (incrementing neither). This
    /// all-or-nothing reservation means a refused creation never leaves one
    /// player's count bumped.
    #[must_use]
    pub fn try_reserve_pair(&self, white: UserId, black: UserId) -> bool {
        if self.max_per_user == 0 {
            return true;
        }
        let mut counts = self.counts.lock().expect("game registry lock poisoned");

        let white_count = counts.get(&white).copied().unwrap_or(0);
        let black_count = counts.get(&black).copied().unwrap_or(0);

        // A self-game (white == black) would count twice toward the same user, so
        // require room for both reservations against the cap.
        let needed_for =
            |user: UserId| -> u32 { u32::from(white == user) + u32::from(black == user) };
        if white_count + needed_for(white) > self.max_per_user
            || black_count + needed_for(black) > self.max_per_user
        {
            return false;
        }

        *counts.entry(white).or_insert(0) += 1;
        *counts.entry(black).or_insert(0) += 1;
        true
    }

    /// Releases one live-game slot for `user` (called once per player on game
    /// finish). Removing a count that reaches zero keeps the map bounded.
    pub fn release(&self, user: UserId) {
        let mut counts = self.counts.lock().expect("game registry lock poisoned");
        if let Some(count) = counts.get_mut(&user) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&user);
            }
        }
    }

    /// The current live-game count for `user` (test/observability helper).
    #[must_use]
    pub fn count(&self, user: UserId) -> u32 {
        self.counts
            .lock()
            .expect("game registry lock poisoned")
            .get(&user)
            .copied()
            .unwrap_or(0)
    }
}

impl std::fmt::Debug for LiveGameRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveGameRegistry")
            .field("max_per_user", &self.max_per_user)
            .field(
                "tracked_users",
                &self
                    .counts
                    .lock()
                    .expect("game registry lock poisoned")
                    .len(),
            )
            .finish()
    }
}

/// A [`GameCompletionHook`] decorator that releases both players' live-game slots
/// when a game finishes, then delegates to the wrapped hook.
///
/// [`AppState`] wraps its rating hook in this so the per-user live-game count is
/// decremented on the same transition that updates ratings — the single,
/// guaranteed "game finished" signal. The inner hook (the Glicko-2 updater)
/// still runs exactly as before.
pub struct LiveGameCountingHook {
    registry: LiveGameRegistry,
    inner: Arc<dyn GameCompletionHook>,
}

impl LiveGameCountingHook {
    /// Wraps `inner`, releasing slots on `registry` before delegating.
    #[must_use]
    pub fn new(registry: LiveGameRegistry, inner: Arc<dyn GameCompletionHook>) -> Self {
        Self { registry, inner }
    }
}

impl std::fmt::Debug for LiveGameCountingHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveGameCountingHook")
            .field("registry", &self.registry)
            .field("inner", &"<dyn GameCompletionHook>")
            .finish()
    }
}

#[async_trait]
impl GameCompletionHook for LiveGameCountingHook {
    async fn on_finished(&self, game: &Game, outcome: &Outcome) {
        // Release first so the slot is freed even if the inner hook is slow.
        self.registry.release(game.white);
        self.registry.release(game.black);
        self.inner.on_finished(game, outcome).await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use mcs_core::EndReason;
    use mcs_domain::TimeControl;
    use time::OffsetDateTime;

    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    // ── Token bucket ───────────────────────────────────────────────────────

    #[test]
    fn limiter_allows_up_to_burst_then_throttles() {
        let limiter = RateLimiter::new(RateLimitTier::per_minute(5));
        let client = ip(1);
        // First 5 (the burst) are allowed.
        for i in 0..5 {
            assert_eq!(limiter.check(client), RateDecision::Allow, "request {i}");
        }
        // The 6th is throttled with a positive Retry-After.
        match limiter.check(client) {
            RateDecision::Throttle { retry_after_secs } => assert!(retry_after_secs >= 1),
            RateDecision::Allow => panic!("6th request past the burst must be throttled"),
        }
    }

    #[test]
    fn limiter_keys_per_ip() {
        let limiter = RateLimiter::new(RateLimitTier::per_minute(1));
        // Exhaust IP 1's single token.
        assert_eq!(limiter.check(ip(1)), RateDecision::Allow);
        assert!(matches!(
            limiter.check(ip(1)),
            RateDecision::Throttle { .. }
        ));
        // A different IP is unaffected.
        assert_eq!(limiter.check(ip(2)), RateDecision::Allow);
    }

    #[test]
    fn disabled_tier_always_allows() {
        let limiter = RateLimiter::new(RateLimitTier::per_minute(0));
        for _ in 0..100 {
            assert_eq!(limiter.check(ip(1)), RateDecision::Allow);
        }
        // Nothing is even tracked when the tier is disabled.
        assert_eq!(limiter.tracked_ips(), 0);
    }

    // ── Client-IP extraction ───────────────────────────────────────────────

    #[test]
    fn client_ip_prefers_trusted_header_first_entry() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        let got = client_ip(&headers, Some(ip(9)), Some("x-forwarded-for"));
        assert_eq!(got, Some("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn client_ip_falls_back_to_peer_without_header() {
        let headers = HeaderMap::new();
        let got = client_ip(&headers, Some(ip(9)), Some("x-forwarded-for"));
        assert_eq!(got, Some(ip(9)));
    }

    #[test]
    fn client_ip_ignores_header_when_not_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        // No trusted header configured: the spoofable header is ignored.
        let got = client_ip(&headers, Some(ip(9)), None);
        assert_eq!(got, Some(ip(9)));
    }

    // ── WS connection registry ─────────────────────────────────────────────

    #[test]
    fn ws_registry_enforces_per_user_cap_and_frees_on_drop() {
        let reg = WsConnectionRegistry::new(0, 2);
        let user = UserId::new();
        let g1 = reg.try_open(user).expect("first under cap");
        let _g2 = reg.try_open(user).expect("second under cap");
        assert_eq!(reg.user_count(user), 2);
        // Third exceeds the per-user cap of 2.
        assert!(reg.try_open(user).is_none(), "third must be rejected");
        // Dropping one frees a slot for a new connection.
        drop(g1);
        assert_eq!(reg.user_count(user), 1);
        let _g3 = reg.try_open(user).expect("slot freed by drop");
        assert_eq!(reg.user_count(user), 2);
    }

    #[test]
    fn ws_registry_enforces_global_cap() {
        let reg = WsConnectionRegistry::new(2, 0);
        let _a = reg.try_open(UserId::new()).expect("1st under global cap");
        let _b = reg.try_open(UserId::new()).expect("2nd under global cap");
        assert_eq!(reg.global_count(), 2);
        assert!(
            reg.try_open(UserId::new()).is_none(),
            "3rd must exceed the global cap of 2"
        );
    }

    #[test]
    fn ws_registry_zero_caps_are_unlimited() {
        let reg = WsConnectionRegistry::new(0, 0);
        let mut guards = Vec::new();
        for _ in 0..50 {
            guards.push(reg.try_open(UserId::new()).expect("no cap"));
        }
        assert_eq!(reg.global_count(), 50);
    }

    // ── Live-game registry ─────────────────────────────────────────────────

    #[test]
    fn live_game_registry_caps_per_user_and_releases() {
        let reg = LiveGameRegistry::new(2);
        let a = UserId::new();
        let b = UserId::new();
        let c = UserId::new();
        // a plays two games (vs b, vs c) — at the cap of 2.
        assert!(reg.try_reserve_pair(a, b));
        assert!(reg.try_reserve_pair(a, c));
        assert_eq!(reg.count(a), 2);
        // A third game for `a` is refused; `c`/`b` counts are untouched.
        let d = UserId::new();
        assert!(!reg.try_reserve_pair(a, d));
        assert_eq!(
            reg.count(d),
            0,
            "refused reservation must not bump the opponent"
        );
        // Finishing one of a's games frees a slot.
        reg.release(a);
        assert_eq!(reg.count(a), 1);
        assert!(reg.try_reserve_pair(a, d), "slot freed");
    }

    #[test]
    fn live_game_registry_zero_cap_is_unlimited() {
        let reg = LiveGameRegistry::new(0);
        let a = UserId::new();
        for _ in 0..100 {
            assert!(reg.try_reserve_pair(a, UserId::new()));
        }
        // Disabled cap tracks nothing.
        assert_eq!(reg.count(a), 0);
    }

    #[tokio::test]
    async fn counting_hook_releases_both_players() {
        let reg = LiveGameRegistry::new(5);
        let white = UserId::new();
        let black = UserId::new();
        assert!(reg.try_reserve_pair(white, black));
        assert_eq!(reg.count(white), 1);
        assert_eq!(reg.count(black), 1);

        let hook = LiveGameCountingHook::new(reg.clone(), Arc::new(mcs_game::NoopHook));
        let game = Game::new(
            "standard".to_owned(),
            mcs_core::VariantOptions::default(),
            white,
            black,
            TimeControl::Unlimited,
            true,
            OffsetDateTime::now_utc(),
        );
        let outcome = Outcome::win(mcs_core::Color::White, EndReason::Checkmate);
        hook.on_finished(&game, &outcome).await;

        assert_eq!(reg.count(white), 0, "white slot released on finish");
        assert_eq!(reg.count(black), 0, "black slot released on finish");
    }
}
