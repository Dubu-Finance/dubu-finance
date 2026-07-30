//! The RFQ maker endpoint: one `POST /quote` that answers with a signed order.
//!
//! ```text
//!   aggregator ──POST /quote──> this ──> quoting::Book (price + reserve)
//!                                   └──> maker::MakerKey (sign)
//! ```
//!
//! [`ServeConfig::bind`] defaults to loopback, and that default is the intended deployment: this
//! endpoint signs orders against a live key, so what faces the internet should be a tunnel or a
//! TLS-terminating, rate-limited proxy. Binding `0.0.0.0` is a decision an operator types out.
//!
//! There is no authentication. A quote is not a secret; asking repeatedly buys only inventory
//! exhaustion, and the answers to that are the reservation book and a rate limit at the proxy, not
//! a shared secret every aggregator replica would have to carry.
//!
//! [`Shared`] is written by the quote cycle and read here; the cycle owns the truth. A snapshot
//! with no fair value means the venues have not agreed recently, and the answer is a refusal rather
//! than a stale price — carrying the last good reference through an outage is how a maker quotes
//! into a market that has moved.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alloy_primitives::Address;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use dubu_core::curve::Ladder;
use dubu_core::pool::{self, Side};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::maker::{MakerKey, SignedOrder};
use crate::quoting::{Book, MakerParams, MarketState, Refusal};

/// Most grid points one request may price. The aggregator sends eleven.
const AMOUNTS_MAX: usize = 32;

/// The pool state one pair quotes from, as the cycle last observed it on chain. Copied out of the
/// chain snapshot rather than re-read per request: an `eth_call` against GIWA's pending tag reads
/// state and timestamp from different moments, so the pool computes an age that never existed and
/// refuses every quote as stale.
#[derive(Debug, Clone, Copy)]
pub struct PropState {
    /// Which market.
    pub pair_id: u16,
    /// Base token.
    pub base: Address,
    /// Quote token.
    pub quote: Address,
    /// The four prices currently stored on chain.
    pub ladder: Ladder,
    /// The epoch, undecayed. This is what prices the ladder; see [`dubu_core::pool`].
    pub bid_capacity: u128,
    /// The ask side of the same.
    pub ask_capacity: u128,
    /// Base the bid epoch has already traded.
    pub bid_used: u128,
    /// Base the ask epoch has already traded.
    pub ask_used: u128,
    /// Chain seconds the stored ladder was stamped at.
    pub updated_at: u64,
    /// The pair's staleness window.
    pub stale_secs_max: u32,
    /// The pair's capacity ramp, zero when disabled.
    pub decay_secs: u16,
    /// The pair's decimal alignment.
    pub price_scale_exp: u8,
    /// Global halt or the pair's own flag.
    pub paused: bool,
    /// When the cycle observed this. An `Instant`, reported as an elapsed age rather than a
    /// wall-clock stamp, for the reason recorded on [`Inner::chain_clock`].
    pub observed_at: Instant,
}

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

/// The cycle's latest view, shared with the endpoint. One lock over the whole map rather than one
/// per pair: the critical sections are a struct copy and the cycle writes at most once a second, so
/// contention is not the constraint.
#[derive(Debug, Default)]
pub struct Shared {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The chain's clock, as `(sealed head timestamp, when we received it)`.
    ///
    /// Expiry may never be stamped from this host's wall clock: it is a promise read by two other
    /// machines — the aggregator refuses anything with under a second left by *its* clock, the
    /// settler enforces it against `block.timestamp` — and nothing synchronises the three, so a
    /// host two seconds slow signs orders that arrive already expired. A monotonic `Instant`
    /// measures *elapsed* time correctly however wrong the wall clock's offset is, so
    /// `head_secs + elapsed` is chain time to within the head's own delivery latency.
    chain_clock: Option<(u64, Instant)>,
    markets: BTreeMap<u16, MarketState>,
    /// The prop pool's own quotes, keyed the same way. Separate from `markets` because the two
    /// answer different questions — `markets` is what this maker will sign for, `props` is what the
    /// pool contract will pay — and a pair can be quotable on one and not the other.
    props: BTreeMap<u16, PropState>,
    book: Book,
    /// Set once the cycle has published anything. Before that every request is refused: a maker
    /// quoting from an empty snapshot is quoting from zero.
    seeded: bool,
}

impl Shared {
    /// No markets, no reservations, not yet seeded: nothing is quoted until the cycle publishes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

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
    /// Per pair rather than as one batch: a pair whose venues have gone quiet must stop being
    /// quotable without taking the others with it, and [`Self::retire`] is how that happens.
    pub fn publish(&self, state: MarketState) {
        if let Ok(mut g) = self.inner.lock() {
            g.markets.insert(state.pair_id, state);
            g.seeded = true;
        }
    }

    /// Replaces the prop pool's state for one pair. Called by the cycle, once per pair per cycle.
    pub fn publish_prop(&self, state: PropState) {
        if let Ok(mut g) = self.inner.lock() {
            g.props.insert(state.pair_id, state);
            g.seeded = true;
        }
    }

    /// Withdraws a pair from prop quoting, leaving the RFQ market alone.
    pub fn retire_prop(&self, pair_id: u16) {
        if let Ok(mut g) = self.inner.lock() {
            g.props.remove(&pair_id);
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
    /// about to post; without it the curve re-offers inventory RFQ has already promised.
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
///
/// **These key names are not ours to choose.** Each is a member of the EIP-712 struct the signature
/// covers, and the member *names* are hashed into the type hash `PmmSettle.ORDER_TYPEHASH` fixes,
/// so a key that does not match the type string has the taker rebuild a different digest and
/// recover a stranger while the maker signs correctly throughout. The `#[serde(rename)]` on
/// `fill_bps_min` is why the qualifier-last rename did not reach the wire, and
/// `wire_names_match_the_signed_type_string` is the guard.
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
    #[serde(rename = "minFillBps")]
    fill_bps_min: u16,
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

/// A grid of sizes to price against the prop pool. Field names match `aggregator/src/quote.ts`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AmountsRequest {
    token_in: Address,
    token_out: Address,
    /// Decimal strings, in `token_in`'s own units.
    amounts_in: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AmountsResponse {
    pair_id: u16,
    side: &'static str,
    /// Same length and order as the request. A zero is a refusal of that size, not a free leg.
    amounts_out: Vec<String>,
    /// How long ago the cycle observed the chain state this was priced from. Elapsed, not
    /// absolute — see [`PropState::observed_at`].
    observed_age_ms: u64,
    /// Chain age of the stored ladder, which is what the staleness window is measured against.
    quote_age_secs: u64,
}

/// Builds the router. Separated from [`run`] so a test can drive it without a socket.
fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/quote", post(quote))
        .route("/prop/amounts", post(prop_amounts))
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

/// Prices, reserves and signs — or explains why it will not. Every refusal is a 4xx with a reason,
/// never a 200 with a zero amount: a zero-amount order is still a signed order, and a taker routed
/// into one pays gas to discover a revert.
async fn quote(
    State(ctx): State<Ctx>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Checked before anything is priced: signing for a chain or settlement contract we were not
    // asked about produces an order valid somewhere we did not intend, which is the one mistake an
    // EIP-712 domain exists to make impossible.
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

    // Chain time, never this host's wall clock: a signed expiry is read by machines whose clocks
    // we do not control. See `Inner::chain_clock`.
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

        let found = quote_find_market(&g.markets, req.taker_asset, req.maker_asset);
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

    Ok(Json(quote_response(&signed)))
}

/// The market for a token pair and which way round it was asked, or `None`. A retired pair is
/// simply absent, the same answer as one that was never configured.
fn quote_find_market(
    markets: &BTreeMap<u16, MarketState>,
    taker_asset: Address,
    maker_asset: Address,
) -> Option<(MarketState, bool)> {
    markets.values().find_map(|m| {
        if m.quote == taker_asset && m.base == maker_asset {
            Some((*m, true))
        } else if m.base == taker_asset && m.quote == maker_asset {
            Some((*m, false))
        } else {
            None
        }
    })
}

/// Renders a signed order for the wire. See [`OrderJson`] on why the key names are not free.
fn quote_response(signed: &SignedOrder) -> QuoteResponse {
    QuoteResponse {
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
            fill_bps_min: signed.order.fill_bps_min,
        },
        signature: format!("0x{}", alloy_primitives::hex::encode(signed.signature)),
        digest: format!("{:#x}", signed.digest),
    }
}

/// Prices a grid of sizes against the prop pool, the way `getAmountOut` would.
///
/// A zero for one size refuses that size — out of domain, past the decayed or the epoch's remaining
/// room — which is the signal the aggregator already reads from a failed `eth_call`. A pair the
/// pool will not quote is a 404; a side offering no depth is a 503, for the reason at that check.
async fn prop_amounts(
    State(ctx): State<Ctx>,
    Json(req): Json<AmountsRequest>,
) -> Result<Json<AmountsResponse>, (StatusCode, Json<ErrorResponse>)> {
    if req.amounts_in.is_empty() || req.amounts_in.len() > AMOUNTS_MAX {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "bad-request",
            format!(
                "amountsIn carries 1..={AMOUNTS_MAX} entries, not {}",
                req.amounts_in.len()
            ),
        ));
    }
    let amounts_in = req
        .amounts_in
        .iter()
        .map(|s| s.parse::<u128>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            bad(
                StatusCode::BAD_REQUEST,
                "bad-amount",
                "every amountsIn entry must be a decimal integer".into(),
            )
        })?;

    // Chain time, never this host's wall clock: the staleness window below is the pool's, and
    // measuring it against a skewed clock is how the on-chain read failed in the first place.
    let now_secs = ctx.shared.chain_now().unwrap_or_else(crate::now_unix);
    let (state, side) = {
        let g = ctx.shared.inner.lock().map_err(|_| {
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
        prop_amounts_find_pair(&g.props, req.token_in, req.token_out).ok_or_else(|| {
            bad(
                StatusCode::NOT_FOUND,
                "no-market",
                "no prop market for that pair right now".into(),
            )
        })?
    };

    if let Some(why) = pool::refusal(
        state.paused,
        state.updated_at,
        now_secs,
        state.stale_secs_max,
    ) {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "no-market",
            format!("the pool is not quoting this pair: {why:?}"),
        ));
    }

    let age = now_secs.saturating_sub(state.updated_at);

    // A side offering nothing at all is a 503, not a grid of zeros: `pool::amount_out` returns
    // `Ok(0)` for every size once `available` is zero, and the aggregator turns that into the same
    // 404 as a venue refusing every size on price, so "re-pricing for 30 seconds" and "there is no
    // market here" become one answer. `pool::refusal` above reads only `paused` and the staleness
    // clock, so a withdrawal never reaches it. The aggregator matches on the status *and* the code.
    //
    // Tested on `available` rather than `capacity` so it covers a ramp run to zero as well as a
    // zeroed epoch; the log carries `capacity` so the two stay distinguishable.
    let (capacity, _, available) = side_depth(&state, side, age);
    if available == 0 {
        warn!(
            target: "prop", event = "no_capacity", pair_id = state.pair_id,
            side = side_label(side), capacity = %capacity, quote_age_secs = age,
            decay_secs = state.decay_secs,
            "this side is offering no depth; refusing with 503 no-capacity rather than a \
             200 of zeros that reads as no market at all"
        );
        return Err(bad(
            StatusCode::SERVICE_UNAVAILABLE,
            "no-capacity",
            format!(
                "pair {} has no depth on the {} side right now; it is re-pricing, so retry",
                state.pair_id,
                side_label(side)
            ),
        ));
    }

    Ok(Json(price_grid(&state, side, age, &amounts_in)))
}

/// The prop state for a token pair and which side answers it, or `None`. `token_in` being the base
/// is the pool buying, so it is the bid.
fn prop_amounts_find_pair(
    props: &BTreeMap<u16, PropState>,
    token_in: Address,
    token_out: Address,
) -> Option<(PropState, Side)> {
    props.values().find_map(|p| {
        if p.base == token_in && p.quote == token_out {
            Some((*p, Side::Bid))
        } else if p.quote == token_in && p.base == token_out {
            Some((*p, Side::Ask))
        } else {
            None
        }
    })
}

/// Which way round this side is, for logs and for the wire.
const fn side_label(side: Side) -> &'static str {
    match side {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

/// One side's epoch, what it has traded, and what the ramp leaves of it. One place rather than two,
/// because the no-capacity refusal and [`price_grid`] must agree on what "no depth" means: a 503
/// computed from a different `available` than the grid it replaces would refuse sizes the pool
/// would in fact have taken.
fn side_depth(state: &PropState, side: Side, age: u64) -> (u128, u128, u128) {
    let (capacity, used) = match side {
        Side::Bid => (state.bid_capacity, state.bid_used),
        Side::Ask => (state.ask_capacity, state.ask_used),
    };
    let available = crate::chain::decayed(capacity, age, state.decay_secs);
    debug_assert!(available <= capacity, "the ramp only ever removes depth");
    (capacity, used, available)
}

/// Applies the pool's own arithmetic to every grid point.
fn price_grid(state: &PropState, side: Side, age: u64, amounts_in: &[u128]) -> AmountsResponse {
    debug_assert!(!amounts_in.is_empty(), "the handler rejects an empty grid");

    let (capacity, used, available) = side_depth(state, side, age);

    let amounts_out = amounts_in
        .iter()
        .map(|&amount_in| {
            pool::amount_out(
                amount_in,
                &state.ladder,
                side,
                capacity,
                available,
                used,
                state.price_scale_exp,
            )
            .unwrap_or_else(|e| {
                error!(
                    target: "prop", event = "quote_failed", pair_id = state.pair_id,
                    amount_in = %amount_in, error = ?e,
                    "the off-chain port refused a size its own gates admitted"
                );
                0
            })
            .to_string()
        })
        .collect();

    AmountsResponse {
        pair_id: state.pair_id,
        side: side_label(side),
        amounts_out,
        observed_age_ms: Instant::now()
            .saturating_duration_since(state.observed_at)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        quote_age_secs: age,
    }
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
/// The bind failing, which is fatal at startup rather than something to retry into: a maker that
/// cannot be reached is a maker whose quotes silently never appear.
pub async fn run(
    cfg: &ServeConfig,
    shared: Arc<Shared>,
    key: Arc<MakerKey>,
    params: MakerParams,
    chain_id: u64,
    pmm_settle: Address,
) -> std::io::Result<()> {
    // `${VAR}` is expanded so a platform that hands out its port in the environment can be written
    // as `0.0.0.0:${PORT}`. Render does exactly that, and without it the maker starts cleanly on
    // the loopback default, logs nothing wrong, and is reachable by nobody.
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
    /// A host running slow otherwise stamps orders the aggregator rejects as already expired.
    #[test]
    fn chain_now_counts_from_the_head_and_ignores_the_host_clock() {
        let s = Shared::new();
        assert_eq!(s.chain_now(), None, "no head yet, so no chain clock");

        // A head that arrived a moment ago carrying chain second 1_000.
        s.publish_clock(1_000, Instant::now());
        assert_eq!(s.chain_now(), Some(1_000));

        // The same head, two seconds of monotonic time later; the wall clock is never consulted.
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
            half_spread_e2_max: 2_000,
            notional_per_order_max: 100 * ONE_ETH_IN_USDC,
            ttl_secs: 30,
            sigma_horizon_secs: 300,
            fill_bps_min: 1_000,
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

    /// Every key of the signed order must be a member of the type string the signature commits to.
    /// A qualifier-last rename that reaches the wire has the aggregator read the member as absent,
    /// default it to zero and recover a stranger — with the order, the signature and the key all
    /// correct, so nothing inside the maker can catch it.
    #[tokio::test]
    async fn wire_names_match_the_signed_type_string() {
        let (_, body) = post_quote(seeded(), buy_request("2000000000")).await;
        let order = body["order"].as_object().expect("order object");
        assert!(!order.is_empty(), "an order with no fields proves nothing");

        for key in order.keys() {
            assert!(
                dubu_core::rfq::ORDER_TYPE_STRING.contains(&format!(" {key},"))
                    || dubu_core::rfq::ORDER_TYPE_STRING.contains(&format!(" {key})")),
                "`{key}` is not a member of the type hash the signature covers; the taker will \
                 rebuild a different digest and recover a stranger"
            );
        }
    }

    #[tokio::test]
    async fn amounts_cross_the_wire_as_strings() {
        let (_, body) = post_quote(seeded(), buy_request("2000000000")).await;
        for f in ["makerAmount", "takerAmount", "nonce", "expiry"] {
            assert!(body["order"][f].is_string(), "{f} must be a string");
        }
    }

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

    #[tokio::test]
    async fn nothing_is_quoted_before_the_cycle_has_published() {
        let (status, body) = post_quote(Arc::new(Shared::new()), buy_request("2000000000")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "not-ready");
    }

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

    // --- the prop pool's own quote ---

    /// Chain second the prop tests pin their clock to.
    const CHAIN_NOW: u64 = 1_000_000;

    fn prop_state(updated_at: u64) -> PropState {
        PropState {
            pair_id: 1,
            base: BASE,
            quote: QUOTE,
            ladder: Ladder {
                min_bid: 1_990_000_000_000_000,
                max_bid: 2_000_000_000_000_000,
                min_ask: 2_000_100_000_000_000,
                max_ask: 2_010_000_000_000_000,
            },
            bid_capacity: 1_000 * ONE_ETH,
            ask_capacity: 1_000 * ONE_ETH,
            bid_used: 0,
            ask_used: 0,
            updated_at,
            stale_secs_max: 5,
            decay_secs: 30,
            price_scale_exp: 24,
            paused: false,
            observed_at: Instant::now(),
        }
    }

    /// Pins the chain clock, so staleness is a property of the fixture and not of the test's speed.
    fn prop_seeded(state: PropState) -> Arc<Shared> {
        let s = Arc::new(Shared::new());
        s.publish_clock(CHAIN_NOW, Instant::now());
        s.publish_prop(state);
        s
    }

    async fn post_amounts(
        shared: Arc<Shared>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let res = app(shared)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/prop/amounts")
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

    fn grid(token_in: Address, token_out: Address, amounts: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "tokenIn": token_in.to_string(),
            "tokenOut": token_out.to_string(),
            "amountsIn": amounts,
        })
    }

    #[tokio::test]
    async fn a_grid_comes_back_priced_in_the_order_it_was_sent() {
        let shared = prop_seeded(prop_state(CHAIN_NOW - 1));
        let (status, body) = post_amounts(
            shared,
            grid(
                BASE,
                QUOTE,
                &["0", "1000000000000000000", "2000000000000000000"],
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["side"], "bid", "base in, quote out is the pool buying");
        let out = body["amountsOut"].as_array().expect("amountsOut");
        assert_eq!(out.len(), 3, "one answer per grid point, in order");
        assert_eq!(out[0], "0", "zero in is zero out");
        let one: u128 = out[1].as_str().expect("str").parse().expect("u128");
        let two: u128 = out[2].as_str().expect("str").parse().expect("u128");
        assert!(one > 0 && two > one, "a deeper fill pays more in total");
    }

    /// Which way round the pair is asked decides which side of the book answers.
    #[tokio::test]
    async fn the_token_order_selects_the_side() {
        let (_, bid) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(bid["side"], "bid");

        let (_, ask) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(QUOTE, BASE, &["2000000000"]),
        )
        .await;
        assert_eq!(ask["side"], "ask");
    }

    /// Moving the read off chain is not a licence to quote a ladder the pool would not honour.
    #[tokio::test]
    async fn a_ladder_past_the_staleness_window_is_still_refused() {
        let fresh = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 5)),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(fresh.0, StatusCode::OK, "exactly at the window is quotable");

        let (status, body) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 6)),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "no-market");
    }

    #[tokio::test]
    async fn a_paused_pair_is_not_quoted() {
        let mut state = prop_state(CHAIN_NOW - 1);
        state.paused = true;
        let (status, _) = post_amounts(
            prop_seeded(state),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Zeros read to the aggregator as no market at all, when the truth is "come back in a moment".
    #[tokio::test]
    async fn a_withdrawn_epoch_is_a_503_rather_than_a_silent_zero() {
        let mut state = prop_state(CHAIN_NOW - 1);
        state.bid_capacity = 0;
        let (status, body) = post_amounts(
            prop_seeded(state),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // The aggregator matches on the status *and* this code; neither alone is the contract.
        assert_eq!(body["error"], "no-capacity");
        assert!(
            body["detail"].as_str().expect("detail").contains("retry"),
            "the caller has to be told this one is worth asking again: {body}"
        );
    }

    /// A zero bid epoch says nothing about the ask; refusing both takes down live depth.
    #[tokio::test]
    async fn the_other_side_of_a_withdrawn_pair_still_quotes() {
        let mut state = prop_state(CHAIN_NOW - 1);
        state.bid_capacity = 0;
        let shared = prop_seeded(state);
        assert_eq!(
            post_amounts(shared.clone(), grid(BASE, QUOTE, &["1000000000000000000"]))
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE,
            "base in is the bid, which is the side that was zeroed"
        );
        let (status, body) = post_amounts(shared, grid(QUOTE, BASE, &["2000000000"])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["side"], "ask");
        assert_ne!(body["amountsOut"][0], "0");
    }

    /// A ramp run to zero leaves the pool offering nothing just as surely as a withdrawal does.
    #[tokio::test]
    async fn a_fully_decayed_epoch_is_also_a_503() {
        let mut state = prop_state(CHAIN_NOW - 30);
        state.stale_secs_max = 60;
        assert_eq!(state.decay_secs, 30, "the fixture's ramp is the point here");
        let (status, body) = post_amounts(
            prop_seeded(state),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "no-capacity");
    }

    /// A zero refuses a size, which is a different claim from having no depth.
    #[tokio::test]
    async fn a_side_with_depth_left_is_untouched_by_the_no_capacity_check() {
        let mut state = prop_state(CHAIN_NOW - 1);
        // One wei of room left on a thousand-ETH epoch: still depth, so still a 200.
        state.bid_used = state.bid_capacity - 1;
        let (status, body) = post_amounts(
            prop_seeded(state),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["amountsOut"][0], "0",
            "the size is refused, but the venue is not claiming to be out of capacity"
        );
    }

    /// The ramp is the pool's, not this endpoint's opinion: an older ladder offers less depth.
    #[tokio::test]
    async fn the_capacity_ramp_narrows_what_a_stale_ladder_will_take() {
        let whole = (1_000 * ONE_ETH).to_string();
        let (_, fresh) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW)),
            grid(BASE, QUOTE, &[&whole]),
        )
        .await;
        assert_ne!(fresh["amountsOut"][0], "0", "a fresh epoch takes all of it");

        let mut aged = prop_state(CHAIN_NOW - 4);
        aged.stale_secs_max = 60;
        let (_, decayed) = post_amounts(prop_seeded(aged), grid(BASE, QUOTE, &[&whole])).await;
        assert_eq!(
            decayed["amountsOut"][0], "0",
            "the decayed room no longer covers the whole epoch"
        );
    }

    #[tokio::test]
    async fn a_pair_this_pool_does_not_hold_is_refused() {
        let (status, body) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(BASE, PMM_SETTLE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "no-market");
    }

    #[tokio::test]
    async fn prop_amounts_cross_the_wire_as_strings() {
        let (_, body) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert!(body["amountsOut"][0].is_string());
    }

    #[tokio::test]
    async fn a_grid_larger_than_the_endpoint_will_price_is_refused() {
        let many = vec!["1"; AMOUNTS_MAX + 1];
        let (status, _) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(BASE, QUOTE, &many),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (empty, _) = post_amounts(
            prop_seeded(prop_state(CHAIN_NOW - 1)),
            grid(BASE, QUOTE, &[]),
        )
        .await;
        assert_eq!(empty, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn nothing_is_priced_before_the_cycle_has_published() {
        let (status, body) = post_amounts(
            Arc::new(Shared::new()),
            grid(BASE, QUOTE, &["1000000000000000000"]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "not-ready");
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
