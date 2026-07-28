//! The RFQ maker endpoint: one `POST /quote` that answers with a signed order.
//!
//! ```text
//!   aggregator ──POST /quote──> this ──> quoting::Book (price + reserve)
//!                                   └──> maker::MakerKey (sign)
//! ```
//!
//! # Where it may listen
//!
//! [`ServeConfig::bind`] defaults to loopback, and that default is the intended deployment. This
//! endpoint signs orders against a live key; the thing that should be reachable from the internet
//! is a tunnel or a reverse proxy that terminates TLS and can be rate-limited, not this. Binding it
//! to `0.0.0.0` is a decision an operator has to type out.
//!
//! There is no authentication, and that is not an oversight either. A quote is not a secret — it
//! commits the maker to a price for thirty seconds and reserves inventory, which is exactly what a
//! taker is supposed to be able to ask for. What an attacker gets by asking repeatedly is
//! inventory exhaustion, and the answer to that is the reservation book plus a rate limit at the
//! proxy, not a shared secret that every aggregator replica would have to carry.
//!
//! # The state it reads
//!
//! [`Shared`] is written by the quote cycle and read here. The cycle owns the truth; this holds a
//! snapshot of it, and a snapshot with no fair value means the venues have not agreed recently and
//! the answer is a refusal rather than a stale price. That is the same rule the ladder follows —
//! carrying the last good reference through an outage is how a maker quotes into a market that
//! has moved.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alloy_primitives::Address;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::maker::MakerKey;
use crate::quoting::{Book, MakerParams, MarketState, Refusal};

/// Where and whether to listen.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeConfig {
    /// Address to bind. Loopback by default; see the module note before changing it.
    #[serde(default = "default_bind")]
    pub bind: String,
}

fn default_bind() -> String {
    "127.0.0.1:8790".into()
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
        }
    }
}

/// The cycle's latest view, shared with the endpoint.
///
/// One lock over the whole map rather than one per pair. The critical sections are a struct copy
/// and the cycle writes at most once a second, so contention is not the constraint; having one
/// place where "the state the maker is quoting from" lives is.
#[derive(Debug, Default)]
pub struct Shared {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The chain's clock, as `(sealed head timestamp, when we received it)`.
    ///
    /// Expiry cannot be stamped from this host's wall clock. It is a promise read by two other
    /// machines -- the aggregator refuses anything with under a second left using *its* clock, and
    /// the settler enforces it against `block.timestamp` -- and nothing synchronises the three.
    /// Measured here: this host ran two seconds slow, so every signed order arrived already expired
    /// and the RFQ leg simply never filled.
    ///
    /// The `Instant` is what makes this work. A monotonic clock measures *elapsed* time correctly
    /// however wrong the wall clock's absolute offset is, so `head_secs + elapsed` is chain time to
    /// within the head's own delivery latency -- tens of milliseconds -- and is immune to the skew
    /// entirely.
    chain_clock: Option<(u64, Instant)>,
    markets: BTreeMap<u16, MarketState>,
    book: Book,
    /// Set once the cycle has published anything at all. Before that every request is refused —
    /// a maker that quotes from an empty snapshot is quoting from zero.
    seeded: bool,
}

impl Shared {
    /// No markets, no reservations, and not yet seeded — every request refused until the cycle
    /// publishes something.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the state for one market. Called by the cycle, once per pair per cycle.
    ///
    /// Publishing per pair rather than as one batch: a pair whose venues have gone quiet should
    /// stop being quotable without taking the others with it, and [`Self::retire`] is how that
    /// happens.
    /// Record the chain's clock from a sealed head. See [`Inner::chain_clock`].
    pub fn publish_clock(&self, head_secs: u64, at: Instant) {
        if let Ok(mut g) = self.inner.lock() {
            g.chain_clock = Some((head_secs, at));
        }
    }

    /// Chain time now, or `None` before any head has arrived.
    #[must_use]
    pub fn chain_now(&self) -> Option<u64> {
        let (secs, at) = self.inner.lock().ok()?.chain_clock?;
        Some(secs.saturating_add(Instant::now().saturating_duration_since(at).as_secs()))
    }

    /// Replaces the state for one market. Called by the cycle, once per pair per cycle.
    ///
    /// Publishing per pair rather than as one batch: a pair whose venues have gone quiet should
    /// stop being quotable without taking the others with it, and [`Self::retire`] is how that
    /// happens.
    pub fn publish(&self, state: MarketState) {
        if let Ok(mut g) = self.inner.lock() {
            g.markets.insert(state.pair_id, state);
            g.seeded = true;
        }
    }

    /// Withdraws a market from quoting. Used when the cycle has no fair value for it.
    pub fn retire(&self, pair_id: u16) {
        if let Ok(mut g) = self.inner.lock() {
            g.markets.remove(&pair_id);
            g.seeded = true;
        }
    }

    /// Base reserved against outstanding orders, for the cycle to subtract from the epoch it is
    /// about to post. Without this the curve would re-offer inventory RFQ has already promised,
    /// which is the same double-commitment the book exists to prevent, in the other direction.
    #[must_use]
    pub fn reserved(&self, pair_id: u16, sells_base: bool) -> u128 {
        self.inner
            .lock()
            .map(|g| g.book.reserved(pair_id, sells_base))
            .unwrap_or(0)
    }

    /// Outstanding orders across every market.
    #[must_use]
    pub fn open_orders(&self) -> usize {
        self.inner.lock().map(|g| g.book.open_len()).unwrap_or(0)
    }

    /// Drops reservations whose orders can no longer be filled. Called from the cycle so the book
    /// is swept even when nobody is asking for quotes.
    pub fn expire(&self, now_secs: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.book.expire(now_secs);
        }
    }
}

/// Everything the handler needs.
#[derive(Clone)]
struct Ctx {
    shared: Arc<Shared>,
    key: Arc<MakerKey>,
    params: MakerParams,
    chain_id: u64,
    pmm_settle: Address,
}

/// What the aggregator asks for. Field names match `aggregator/src/rfq.ts`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteRequest {
    chain_id: u64,
    verifying_contract: Address,
    taker_asset: Address,
    maker_asset: Address,
    /// Decimal string, in the taker asset's own units.
    taker_amount: String,
}

/// One signed order. Amounts are decimal strings — a `u128` does not survive JSON's number type,
/// and a silently truncated amount is a signature over a trade nobody meant.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderJson {
    maker: String,
    maker_asset: String,
    taker_asset: String,
    maker_amount: String,
    taker_amount: String,
    nonce: String,
    expiry: String,
    decay_start: String,
    decay_per_sec: u32,
    decay_cap: u32,
    min_fill_bps: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    order: OrderJson,
    signature: String,
    /// The EIP-712 digest, so a caller can join a later `OrderFilled` back to this response.
    digest: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
    detail: String,
}

/// Builds the router. Separated from [`run`] so a test can drive it without a socket.
fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/quote", post(quote))
        .with_state(ctx)
}

async fn health(State(ctx): State<Ctx>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "maker": ctx.key.address().to_string(),
        "chainId": ctx.chain_id,
        "verifyingContract": ctx.pmm_settle.to_string(),
        "domainSeparator": format!("{:#x}", ctx.key.domain_separator()),
        "openOrders": ctx.shared.open_orders(),
    }))
}

/// Prices, reserves and signs — or explains why it will not.
///
/// Every refusal is a 4xx with a reason, never a 200 with a zero amount. A zero-amount order is
/// still a signed order, and a taker that routed into one would pay gas to discover a revert.
async fn quote(
    State(ctx): State<Ctx>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResponse>, (StatusCode, Json<ErrorResponse>)> {
    // The domain is checked before anything is priced. A request naming a different chain or
    // settlement contract is not a request this maker can answer — signing it anyway would produce
    // an order valid somewhere we did not intend, which is the one mistake an EIP-712 domain
    // exists to make impossible.
    if req.chain_id != ctx.chain_id || req.verifying_contract != ctx.pmm_settle {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "wrong-domain",
            format!(
                "this maker signs for chain {} and {}, not chain {} and {}",
                ctx.chain_id, ctx.pmm_settle, req.chain_id, req.verifying_contract
            ),
        ));
    }

    let taker_amount: u128 = req.taker_amount.parse().map_err(|_| {
        bad(
            StatusCode::BAD_REQUEST,
            "bad-amount",
            "takerAmount must be a decimal integer".into(),
        )
    })?;

    // Chain time, never this host's wall clock. See `Inner::chain_clock` -- a signed expiry is
    // read by machines whose clocks we do not control, and a skewed one here is invisible until
    // every quote comes back refused as expired.
    let now_secs = ctx.shared.chain_now().unwrap_or_else(crate::now_unix);

    let (quote, nonce) = {
        let mut g = ctx.shared.inner.lock().map_err(|_| {
            bad(
                StatusCode::INTERNAL_SERVER_ERROR,
                "poisoned",
                "shared state is poisoned".into(),
            )
        })?;

        if !g.seeded {
            return Err(bad(
                StatusCode::SERVICE_UNAVAILABLE,
                "not-ready",
                "the quote cycle has not published a reference yet".into(),
            ));
        }

        // Which market, and which way round. A pair the cycle has retired is simply absent, which
        // is the same answer as one that was never configured — in both cases this maker has no
        // price it is willing to stand behind.
        let found = g.markets.values().find_map(|m| {
            if m.quote == req.taker_asset && m.base == req.maker_asset {
                Some((*m, true))
            } else if m.base == req.taker_asset && m.quote == req.maker_asset {
                Some((*m, false))
            } else {
                None
            }
        });
        let Some((market, taker_buys_base)) = found else {
            return Err(bad(
                StatusCode::NOT_FOUND,
                "no-market",
                "no quotable market for that pair right now".into(),
            ));
        };

        let nonce = g.book.next_nonce();
        let q = g
            .book
            .quote(
                &ctx.params,
                &market,
                taker_buys_base,
                taker_amount,
                now_secs,
            )
            .map_err(refusal)?;
        (q, nonce)
    };

    let signed = ctx.key.sign(&quote, nonce).map_err(|e| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            "sign-failed",
            e.to_string(),
        )
    })?;

    info!(
        target: "rfq", event = "quoted", pair_id = quote.pair_id,
        maker_sells_base = quote.maker_sells_base,
        taker_amount = %quote.taker_amount, maker_amount = %quote.maker_amount,
        half_spread_e2 = quote.half_spread_e2, expiry = quote.expiry, nonce,
        digest = %signed.digest, open_orders = ctx.shared.open_orders(),
        "signed an RFQ order"
    );

    Ok(Json(QuoteResponse {
        order: OrderJson {
            maker: Address::from(signed.order.maker).to_string(),
            maker_asset: Address::from(signed.order.maker_asset).to_string(),
            taker_asset: Address::from(signed.order.taker_asset).to_string(),
            maker_amount: signed.order.maker_amount.to_string(),
            taker_amount: signed.order.taker_amount.to_string(),
            nonce: signed.order.nonce.to_string(),
            expiry: signed.order.expiry.to_string(),
            decay_start: signed.order.decay_start.to_string(),
            decay_per_sec: signed.order.decay_per_sec,
            decay_cap: signed.order.decay_cap,
            min_fill_bps: signed.order.min_fill_bps,
        },
        signature: format!("0x{}", alloy_primitives::hex::encode(signed.signature)),
        digest: format!("{:#x}", signed.digest),
    }))
}

fn refusal(r: Refusal) -> (StatusCode, Json<ErrorResponse>) {
    let (code, name, detail) = match r {
        Refusal::UnknownPair => (
            StatusCode::NOT_FOUND,
            "no-market",
            "not a market this maker quotes",
        ),
        Refusal::SizeOutOfRange => (
            StatusCode::BAD_REQUEST,
            "size-out-of-range",
            "zero, or larger than this maker will commit in one order",
        ),
        Refusal::InsufficientInventory => (
            StatusCode::CONFLICT,
            "insufficient-inventory",
            "the inventory left after the curve's epoch and outstanding orders will not cover it",
        ),
        Refusal::Undefined => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "undefined",
            "the pricing arithmetic overflowed",
        ),
    };
    bad(code, name, detail.into())
}

fn bad(code: StatusCode, error: &'static str, detail: String) -> (StatusCode, Json<ErrorResponse>) {
    (code, Json(ErrorResponse { error, detail }))
}

/// Serves until the process ends.
///
/// # Errors
///
/// The bind failing, which is fatal at startup and not something to retry into: a maker that
/// cannot be reached is a maker whose quotes silently never appear.
pub async fn run(
    cfg: &ServeConfig,
    shared: Arc<Shared>,
    key: Arc<MakerKey>,
    params: MakerParams,
    chain_id: u64,
    pmm_settle: Address,
) -> std::io::Result<()> {
    // `${VAR}` is expanded here so a platform that hands out its port in the environment can be
    // written as `0.0.0.0:${PORT}`. Render does exactly that, and without this the config would
    // have to hard-code a port the platform did not choose -- or, worse, keep the loopback default
    // and deploy a maker that starts cleanly, logs nothing wrong, and is reachable by nobody.
    let bind = crate::config::expand_env("rfq.serve.bind", &cfg.bind)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let addr: SocketAddr = bind.parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("bind `{bind}`: {e}"),
        )
    })?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    if !addr.ip().is_loopback() {
        warn!(
            target: "rfq", event = "public_bind", %addr,
            "THE RFQ ENDPOINT IS BOUND OFF LOOPBACK. It signs orders against a live key and has no \
             authentication; put a TLS-terminating, rate-limited proxy in front of it"
        );
    }
    info!(
        target: "rfq", event = "listening", %addr, maker = %key.address(),
        domain_separator = %format!("{:#x}", key.domain_separator()),
        "RFQ maker endpoint up"
    );

    axum::serve(
        listener,
        router(Ctx {
            shared,
            key,
            params,
            chain_id,
            pmm_settle,
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    /// Expiry must come from the chain, not from this host.
    ///
    /// The regression: this machine ran two seconds slow, the maker stamped expiry from its own
    /// wall clock, and the aggregator -- refusing anything with under a second left, using a clock
    /// that was right -- rejected every order as expired. Nothing looked broken. The maker was
    /// healthy, the tunnel answered in 92ms, the price was correct, and the leg never filled.
    #[test]
    fn chain_now_counts_from_the_head_and_ignores_the_host_clock() {
        let s = Shared::new();
        assert_eq!(s.chain_now(), None, "no head yet, so no chain clock");

        // A head that arrived a moment ago carrying chain second 1_000.
        s.publish_clock(1_000, Instant::now());
        assert_eq!(s.chain_now(), Some(1_000));

        // The same head, two seconds of monotonic time later. The host's wall clock is not
        // consulted at any point, so its offset -- whatever it is -- cannot enter this.
        s.publish_clock(1_000, Instant::now() - Duration::from_secs(2));
        assert_eq!(s.chain_now(), Some(1_002));
    }

    use super::*;
    use crate::tx::Signer;
    use alloy_primitives::address;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const PMM_SETTLE: Address = address!("68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E");
    const CHAIN_ID: u64 = 91342;
    const BASE: Address = address!("81e46C6379498beBEB5DCcD47ab2DdFaf967d445");
    const QUOTE: Address = address!("d28596C6750D87C53EA146134AfAB53de86C5155");
    const ONE_ETH: u128 = 1_000_000_000_000_000_000;
    const ONE_ETH_IN_USDC: u128 = 2_000_000_000;

    fn params() -> MakerParams {
        MakerParams {
            base_half_spread_e2: 300,
            sigma_coefficient_e2: 10,
            max_half_spread_e2: 2_000,
            max_notional_per_order: 100 * ONE_ETH_IN_USDC,
            ttl_secs: 30,
            sigma_horizon_secs: 300,
            min_fill_bps: 1_000,
        }
    }

    fn market() -> MarketState {
        MarketState {
            pair_id: 1,
            base: BASE,
            quote: QUOTE,
            fair: 2_000_000_000_000_000,
            price_scale_exp: 24,
            sigma_millibps: 0,
            base_balance: 1_000 * ONE_ETH,
            quote_balance: 10_000_000_000_000,
            epoch_ask_base: 0,
            epoch_bid_base: 0,
        }
    }

    fn app(shared: Arc<Shared>) -> Router {
        router(Ctx {
            shared,
            key: Arc::new(MakerKey::new(
                Signer::from_hex(KEY).expect("key"),
                CHAIN_ID,
                PMM_SETTLE,
            )),
            params: params(),
            chain_id: CHAIN_ID,
            pmm_settle: PMM_SETTLE,
        })
    }

    fn seeded() -> Arc<Shared> {
        let s = Arc::new(Shared::new());
        s.publish(market());
        s
    }

    async fn post_quote(
        shared: Arc<Shared>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let res = app(shared)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/quote")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn buy_request(taker_amount: &str) -> serde_json::Value {
        serde_json::json!({
            "chainId": CHAIN_ID,
            "verifyingContract": PMM_SETTLE.to_string(),
            "takerAsset": QUOTE.to_string(),
            "makerAsset": BASE.to_string(),
            "takerAmount": taker_amount,
        })
    }

    #[tokio::test]
    async fn a_well_formed_request_gets_a_signed_order() {
        let (status, body) = post_quote(seeded(), buy_request("2000000000")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["order"]["takerAmount"], "2000000000",
            "the taker leg is honoured exactly"
        );
        assert_eq!(body["order"]["takerAsset"], QUOTE.to_string());
        assert_eq!(body["order"]["makerAsset"], BASE.to_string());
        assert_eq!(
            body["signature"].as_str().expect("signature").len(),
            132,
            "65 bytes as 0x-hex"
        );
        assert!(
            body["order"]["makerAmount"]
                .as_str()
                .expect("amount")
                .parse::<u128>()
                .expect("u128")
                > 0
        );
    }

    /// The amounts are decimal strings, not JSON numbers. A `u128` past 2^53 loses precision as a
    /// double, and a truncated amount is a signature over a trade nobody meant.
    #[tokio::test]
    async fn amounts_cross_the_wire_as_strings() {
        let (_, body) = post_quote(seeded(), buy_request("2000000000")).await;
        for f in ["makerAmount", "takerAmount", "nonce", "expiry"] {
            assert!(body["order"][f].is_string(), "{f} must be a string");
        }
    }

    /// Signing an order for a domain we were not asked about would produce something valid
    /// somewhere we did not intend, which is the one thing an EIP-712 domain exists to prevent.
    #[tokio::test]
    async fn a_request_for_another_domain_is_refused() {
        let mut wrong_chain = buy_request("2000000000");
        wrong_chain["chainId"] = serde_json::json!(1);
        assert_eq!(
            post_quote(seeded(), wrong_chain).await.0,
            StatusCode::BAD_REQUEST
        );

        let mut wrong_contract = buy_request("2000000000");
        wrong_contract["verifyingContract"] = serde_json::json!(BASE.to_string());
        assert_eq!(
            post_quote(seeded(), wrong_contract).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    /// Quoting from an empty snapshot is quoting from zero.
    #[tokio::test]
    async fn nothing_is_quoted_before_the_cycle_has_published() {
        let (status, body) = post_quote(Arc::new(Shared::new()), buy_request("2000000000")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "not-ready");
    }

    /// A retired pair answers the same way as one that was never configured: no price this maker
    /// will stand behind.
    #[tokio::test]
    async fn a_retired_market_stops_being_quotable() {
        let shared = seeded();
        assert_eq!(
            post_quote(shared.clone(), buy_request("2000000000"))
                .await
                .0,
            StatusCode::OK
        );
        shared.retire(1);
        let (status, body) = post_quote(shared, buy_request("2000000000")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "no-market");
    }

    #[tokio::test]
    async fn an_unknown_pair_is_refused() {
        let mut req = buy_request("2000000000");
        req["makerAsset"] = serde_json::json!(PMM_SETTLE.to_string());
        assert_eq!(post_quote(seeded(), req).await.0, StatusCode::NOT_FOUND);
    }

    /// A refusal is a 4xx with a reason, never a 200 carrying a zero amount — a zero-amount order
    /// is still a signed order, and routing into one costs gas to discover a revert.
    #[tokio::test]
    async fn an_oversized_request_is_refused_rather_than_answered_with_zero() {
        let (status, body) = post_quote(seeded(), buy_request("999999999999999")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "size-out-of-range");
    }

    #[tokio::test]
    async fn a_non_numeric_amount_is_refused() {
        let (status, body) = post_quote(seeded(), buy_request("2e9")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad-amount");
    }

    /// The reservation is visible to the cycle, which is how the curve avoids re-offering
    /// inventory RFQ has already promised.
    #[tokio::test]
    async fn a_served_quote_reserves_inventory_the_cycle_can_see() {
        let shared = seeded();
        assert_eq!(shared.reserved(1, true), 0);
        post_quote(shared.clone(), buy_request("2000000000")).await;
        assert!(
            shared.reserved(1, true) > 0,
            "the base leg must be visible as reserved"
        );
        assert_eq!(shared.open_orders(), 1);
    }

    #[tokio::test]
    async fn health_reports_the_domain_the_maker_signs_for() {
        let res = app(seeded())
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            body["domainSeparator"],
            "0x785df92cfa961225995c562e9a42c1b5645097a5bd5b868c303785afb34c5ee7"
        );
    }
}
