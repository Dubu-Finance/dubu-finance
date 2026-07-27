//! Quoting the RFQ side, from the same state the curve is quoted from.
//!
//! The prop pool and the RFQ leg are two surfaces of one market maker, and this module exists so
//! that stays true. It reads the fair value, the volatility estimate and the inventory that
//! `main`'s cycle already computed, and turns a size into a signed order.
//!
//! # Why RFQ may quote tighter than the curve, and why that is not an arbitrage
//!
//! It is worth being precise, because "our own two venues disagree" sounds like a bug.
//!
//! The curve is a *standing* quote: it is posted before anyone asks, and its exposure is to being
//! picked off by whoever notices a stale ladder first. RFQ is quoted *on request*, after the size
//! is known and against a reference read at that moment. The second is a strictly better-informed
//! quote, so it can be tighter, and being tighter is the point of having it.
//!
//! Nothing can be round-tripped between them. Buying from RFQ costs `fair + s_rfq` and selling to
//! the curve pays `fair - s_curve`; the round trip loses `s_rfq + s_curve`, which is positive for
//! any positive pair of spreads. The two venues cannot be arbitraged against each other by
//! construction, whatever the ratio of their spreads.
//!
//! # What genuinely can go wrong: committing the same inventory twice
//!
//! The risk is not the price, it is the size. The curve's epoch says "I will sell this much base
//! before the next refresh". If RFQ signs orders for the same base without telling anyone, the
//! pool has committed twice against one risk budget, and the killswitch that watches NAV finds out
//! afterwards.
//!
//! [`Book`] is the answer. Every quote reserves its base leg for as long as the order can be
//! filled, and the reservation is released when the order expires. Reservations are never released
//! early on the grounds that an order "probably was not filled" — an order that is filled at the
//! last second and one that expires unfilled are indistinguishable until they are not, and
//! over-reserving for a few tens of seconds costs a little capacity while under-reserving costs
//! the difference between the risk budget and reality.
//!
//! Deliberately not tracked here: whether a specific order was filled. `PmmSettle` emits
//! `OrderFilled`, but treating that as the release signal means a missed log releases nothing and
//! the book leaks capacity until restart. Expiry is a clock, and a clock cannot be missed.

use std::collections::VecDeque;

use alloy_primitives::Address;

/// Spreads and sizes are in hundredths of a basis point, as everywhere else in this crate.
pub const BPS_E2: u64 = 10_000;

/// How the RFQ side is priced and bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerParams {
    /// Half-spread at zero volatility, in hundredths of a bp.
    pub base_half_spread_e2: u32,
    /// Added half-spread per unit of `sigma_millibps`, in hundredths of a bp per millibp.
    ///
    /// The same shape the curve uses. Sharing the shape rather than the number is deliberate: the
    /// two surfaces face different selection, so they should be tunable apart while moving
    /// together.
    pub sigma_coefficient_e2: u32,
    /// Ceiling on the half-spread however volatile it gets.
    pub max_half_spread_e2: u32,
    /// Largest notional a single order may commit, in the quote token's own units.
    ///
    /// Notional rather than base, and the distinction is not cosmetic: a cap of "50" means 50
    /// mWETH on one pair and 50 mWBTC on another, which at these prices is a fifty-fold difference
    /// in the risk one order can take. A limit a maker sets is a statement about money, so it is
    /// denominated in money and converted to base at the pair's own fair value.
    pub max_notional_per_order: u128,
    /// How long a signed order stays fillable. Also how long its reservation is held.
    pub ttl_secs: u64,
    /// Floor on a single fill, in bps of the maker leg. Stops an order being nibbled into dust.
    pub min_fill_bps: u16,
}

impl MakerParams {
    /// Half-spread for the current volatility, capped.
    ///
    /// Saturating rather than checked. The cap is applied last and every intermediate only grows,
    /// so an overflow and the cap give the same answer — whereas wrapping would turn a violent
    /// market into a *narrow* spread, which is the one direction a volatility term must never fail
    /// in. Debug builds would have panicked instead, which is not better on a live maker.
    #[must_use]
    pub fn half_spread_e2(&self, sigma_millibps: u64) -> u32 {
        let scaled = u64::from(self.base_half_spread_e2)
            .saturating_add(u64::from(self.sigma_coefficient_e2).saturating_mul(sigma_millibps) / 1_000);
        u32::try_from(scaled).unwrap_or(u32::MAX).min(self.max_half_spread_e2)
    }
}

/// The cycle's view of one market, handed to the quoter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketState {
    /// Which market.
    pub pair_id: u16,
    /// Base token.
    pub base: Address,
    /// Quote token.
    pub quote: Address,
    /// Mid, in the pair's price units.
    pub fair: u128,
    /// The pair's decimal alignment.
    pub price_scale_exp: u8,
    /// Current volatility estimate, shared with the curve. `u64` because that is the width
    /// `skew::Volatility` produces it at, and narrowing at the boundary would be a silent cap.
    pub sigma_millibps: u64,
    /// Base the maker holds and may sell.
    pub base_balance: u128,
    /// Quote the maker holds and may spend.
    pub quote_balance: u128,
    /// Base the curve's epoch has already committed to selling, and to buying.
    pub epoch_ask_base: u128,
    /// Base the curve's epoch has already committed to buying.
    pub epoch_bid_base: u128,
}

/// Why a size could not be quoted. Every variant is a normal answer, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The pair is not one this maker quotes.
    UnknownPair,
    /// Zero, or more than `max_notional_per_order` once converted.
    SizeOutOfRange,
    /// The inventory left after the curve's epoch and outstanding reservations will not cover it.
    InsufficientInventory,
    /// The arithmetic overflowed, which means the inputs were nonsense.
    Undefined,
}

/// A quote, before it is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// Which market.
    pub pair_id: u16,
    /// What the maker delivers.
    pub maker_asset: Address,
    /// What the maker receives.
    pub taker_asset: Address,
    /// The maker's leg, at full size.
    pub maker_amount: u128,
    /// The taker's leg, at full size.
    pub taker_amount: u128,
    /// True when the maker sells base.
    pub maker_sells_base: bool,
    /// The base leg, whichever side it is on. What gets reserved.
    pub base_amount: u128,
    /// The half-spread this quote was priced at, for the log.
    pub half_spread_e2: u32,
    /// Last second at which the order may be filled.
    pub expiry: u64,
    /// Floor on a single fill, in bps of the maker leg.
    pub min_fill_bps: u16,
}

/// One outstanding commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reservation {
    pair_id: u16,
    /// True when the reserved base is base the maker must deliver.
    sells_base: bool,
    base_amount: u128,
    expires_at: u64,
}

/// Everything the maker has promised and not yet seen expire.
#[derive(Debug, Default)]
pub struct Book {
    open: VecDeque<Reservation>,
    /// Orders signed since start. Also the nonce source — see [`Book::next_nonce`].
    signed: u64,
}

impl Book {
    /// An empty book: nothing promised, no nonces spent.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops reservations whose orders can no longer be filled.
    pub fn expire(&mut self, now_secs: u64) {
        self.open.retain(|r| r.expires_at > now_secs);
    }

    /// Base reserved on one side of one pair.
    #[must_use]
    pub fn reserved(&self, pair_id: u16, sells_base: bool) -> u128 {
        self.open
            .iter()
            .filter(|r| r.pair_id == pair_id && r.sells_base == sells_base)
            .fold(0u128, |a, r| a.saturating_add(r.base_amount))
    }

    /// How many orders are outstanding.
    #[must_use]
    pub fn open_len(&self) -> usize {
        self.open.len()
    }

    /// A nonce that has not been used by this process.
    ///
    /// Monotone from a counter rather than random, because `PmmSettle` treats the nonce as the
    /// maker's cancellation handle: a run of consecutive nonces can be cancelled as a range, and a
    /// random one can only be cancelled one at a time. It is explicitly *not* the replay guard —
    /// the fill accounting is — so a collision across restarts costs nothing but a shared
    /// cancellation slot.
    #[must_use]
    pub fn next_nonce(&self) -> u64 {
        self.signed
    }

    /// Prices an exact taker input, reserves the base it commits, and returns the order to sign.
    ///
    /// `taker_buys_base` is the taker's direction: true when the taker wants base out of the
    /// maker, which is the maker selling base. `taker_amount` is denominated in whatever the taker
    /// is handing over — base when selling base, quote when buying it.
    ///
    /// # Exact in, and why it has to be
    ///
    /// The taker asks "what do I get for exactly this much", so `taker_amount` comes out of this
    /// unchanged and the *maker's* leg is the one derived. Quoting by base and reporting whatever
    /// taker amount fell out would look equivalent and is not: an aggregator checks the returned
    /// `takerAmount` against the size it asked for, and a maker that answers a neighbouring size
    /// has answered a different question. Rounding therefore lands entirely on the maker's leg,
    /// always downward, so the taker's side of the order is exactly what was requested and the
    /// remainder is the maker's.
    ///
    /// # Errors
    ///
    /// [`Refusal`], each of which is a normal outcome the caller should report rather than log as
    /// a failure.
    pub fn quote(
        &mut self,
        params: &MakerParams,
        state: &MarketState,
        taker_buys_base: bool,
        taker_amount: u128,
        now_secs: u64,
    ) -> Result<Quote, Refusal> {
        self.expire(now_secs);

        if taker_amount == 0 {
            return Err(Refusal::SizeOutOfRange);
        }

        let half = params.half_spread_e2(state.sigma_millibps);
        let scale = 10u128.checked_pow(u32::from(state.price_scale_exp)).ok_or(Refusal::Undefined)?;
        // The maker's side of the spread, always against itself. Selling base is priced up from
        // fair, buying base is priced down from it. The sign is not configurable.
        let num = u128::from(BPS_E2) * 100;

        // Both branches round the maker's leg down: fewer tokens delivered, never more.
        //
        // The per-order cap is checked *before* the conversion, not after. `taker_amount * scale`
        // is the widest intermediate here — with an 18/6 pair `scale` is 10^24 — and an
        // out-of-range request would overflow it and surface as `Undefined`, which claims the
        // inputs were nonsense when they were merely too big. Bounding the taker's leg by what the
        // notional cap permits keeps the multiply inside `u128` and gives the honest refusal.
        let (maker_amount, base_amount) = if taker_buys_base {
            let price = state.fair.checked_mul(num + u128::from(half)).ok_or(Refusal::Undefined)? / num;
            if price == 0 {
                return Err(Refusal::Undefined);
            }
            // The taker's leg is already the notional here, so the cap applies to it directly.
            if taker_amount > params.max_notional_per_order {
                return Err(Refusal::SizeOutOfRange);
            }
            let base = taker_amount.checked_mul(scale).ok_or(Refusal::Undefined)? / price;
            (base, base)
        } else {
            // Selling base, so the notional is the maker's leg. Bound the base first so the
            // multiply below cannot overflow, then check what it comes to.
            let ceiling = params
                .max_notional_per_order
                .checked_mul(scale)
                .ok_or(Refusal::Undefined)?
                .checked_div(state.fair.max(1))
                .ok_or(Refusal::Undefined)?;
            if taker_amount > ceiling {
                return Err(Refusal::SizeOutOfRange);
            }
            let price = state.fair.checked_mul(num - u128::from(half)).ok_or(Refusal::Undefined)? / num;
            let quote = taker_amount.checked_mul(price).ok_or(Refusal::Undefined)? / scale;
            if quote > params.max_notional_per_order {
                return Err(Refusal::SizeOutOfRange);
            }
            (quote, taker_amount)
        };

        if base_amount == 0 || maker_amount == 0 {
            return Err(Refusal::SizeOutOfRange);
        }

        let room = self.room(state, taker_buys_base)?;
        if base_amount > room {
            return Err(Refusal::InsufficientInventory);
        }

        let expiry = now_secs.saturating_add(params.ttl_secs);
        self.open.push_back(Reservation {
            pair_id: state.pair_id,
            sells_base: taker_buys_base,
            base_amount,
            expires_at: expiry,
        });
        self.signed += 1;

        Ok(Quote {
            pair_id: state.pair_id,
            maker_asset: if taker_buys_base { state.base } else { state.quote },
            taker_asset: if taker_buys_base { state.quote } else { state.base },
            maker_amount,
            taker_amount,
            maker_sells_base: taker_buys_base,
            base_amount,
            half_spread_e2: half,
            expiry,
            min_fill_bps: params.min_fill_bps,
        })
    }

    /// Base still uncommitted on one side, after the curve's epoch and the open reservations.
    ///
    /// The curve's epoch is subtracted rather than shared. Both surfaces draw on one balance, and
    /// the epoch is a promise the pool has already published on chain — RFQ is the newer claim, so
    /// RFQ is the one that yields.
    fn room(&self, state: &MarketState, sells_base: bool) -> Result<u128, Refusal> {
        let held = if sells_base {
            state.base_balance
        } else {
            // Buying base spends quote, so what bounds the base leg is the quote on hand converted
            // at fair. An approximation by exactly the spread, and in the safe direction: valuing
            // the base at the mid rather than at the bid understates what the quote will buy.
            let scale = 10u128.checked_pow(u32::from(state.price_scale_exp)).ok_or(Refusal::Undefined)?;
            if state.fair == 0 {
                return Err(Refusal::Undefined);
            }
            state.quote_balance.checked_mul(scale).ok_or(Refusal::Undefined)? / state.fair
        };
        let epoch = if sells_base { state.epoch_ask_base } else { state.epoch_bid_base };
        Ok(held
            .saturating_sub(epoch)
            .saturating_sub(self.reserved(state.pair_id, sells_base)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const ONE_ETH: u128 = 1_000_000_000_000_000_000;
    /// Roughly one base token's worth of quote at the fair value below. The taker's leg when
    /// buying base is denominated in quote, so sizes on the two sides are not interchangeable —
    /// which is the whole point of the exact-in shape.
    const ONE_ETH_IN_USDC: u128 = 2_000_000_000;

    fn params() -> MakerParams {
        MakerParams {
            base_half_spread_e2: 300, // 3 bp
            sigma_coefficient_e2: 10,
            max_half_spread_e2: 2_000,
            max_notional_per_order: 100 * ONE_ETH_IN_USDC, // $200k
            ttl_secs: 30,
            min_fill_bps: 1_000,
        }
    }

    fn state() -> MarketState {
        MarketState {
            pair_id: 1,
            base: address!("00000000000000000000000000000000000000B1"),
            quote: address!("00000000000000000000000000000000000000C2"),
            // $2,000 on an 18/6 pair, priceScaleExp 24 — the value `Deploy` prints for this market.
            fair: 2_000_000_000_000_000,
            price_scale_exp: 24,
            sigma_millibps: 0,
            base_balance: 1_000 * ONE_ETH,
            quote_balance: 10_000_000_000_000,
            epoch_ask_base: 0,
            epoch_bid_base: 0,
        }
    }

    /// The taker's leg is honoured exactly; the maker's is what moves. An aggregator checks the
    /// returned `takerAmount` against the size it asked for, so a neighbouring size is a wrong
    /// answer rather than a rounding detail.
    #[test]
    fn the_taker_leg_comes_back_exactly_as_it_went_in() {
        let mut book = Book::new();
        for (buys, amount) in [(true, ONE_ETH_IN_USDC + 7), (false, ONE_ETH + 7)] {
            let q = book.quote(&params(), &state(), buys, amount, 1_000).expect("quotable");
            assert_eq!(q.taker_amount, amount, "taker_buys_base = {buys}");
        }
    }

    #[test]
    fn the_spread_is_charged_against_the_maker_in_both_directions() {
        let mut book = Book::new();
        let scale = 10u128.pow(24);

        // Buying base: the base delivered is less than the taker's quote would buy at mid.
        let buy = book.quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000).expect("quotable");
        assert!(buy.maker_amount < ONE_ETH_IN_USDC * scale / state().fair, "buying must cost above mid");

        // Selling base: the quote paid is less than the base is worth at mid.
        let sell = book.quote(&params(), &state(), false, ONE_ETH, 1_000).expect("quotable");
        assert!(sell.maker_amount < ONE_ETH * state().fair / scale, "selling must pay below mid");
    }

    /// The property that makes two venues from one maker safe. Whatever the spreads, going in one
    /// side and out the other loses.
    #[test]
    fn a_round_trip_through_both_sides_always_loses() {
        let mut book = Book::new();
        // Buy some base for a known amount of quote, then sell exactly that base back.
        let buy = book.quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000).expect("quotable");
        let sell = book.quote(&params(), &state(), false, buy.maker_amount, 1_000).expect("quotable");
        assert!(sell.maker_amount < ONE_ETH_IN_USDC, "the round trip must not be free money");
    }

    #[test]
    fn volatility_widens_the_spread_up_to_the_cap() {
        assert!(params().half_spread_e2(50_000) > params().half_spread_e2(0));
        assert_eq!(params().half_spread_e2(u64::MAX), params().max_half_spread_e2);
    }

    /// The cap is a notional however the size was expressed, so both directions refuse the same
    /// amount of money. Refused, not overflowed into `Undefined`.
    #[test]
    fn a_size_above_the_per_order_cap_is_refused_in_either_denomination() {
        let mut book = Book::new();
        assert_eq!(
            book.quote(&params(), &state(), true, 101 * ONE_ETH_IN_USDC, 1_000),
            Err(Refusal::SizeOutOfRange)
        );
        assert_eq!(
            book.quote(&params(), &state(), false, 101 * ONE_ETH, 1_000),
            Err(Refusal::SizeOutOfRange)
        );
        assert_eq!(book.quote(&params(), &state(), true, 0, 1_000), Err(Refusal::SizeOutOfRange));
    }

    /// The point of denominating the cap in money: the same cap on a pair priced fifty times
    /// higher admits fifty times less base, rather than fifty times the risk.
    #[test]
    fn the_cap_is_money_not_tokens() {
        let mut book = Book::new();
        let mut pricey = state();
        pricey.fair *= 50; // a $100k base token instead of a $2k one
        pricey.base_balance = 1_000 * ONE_ETH;

        let cheap = book.quote(&params(), &state(), true, 100 * ONE_ETH_IN_USDC, 1_000).expect("quotable");
        let dear = book.quote(&params(), &pricey, true, 100 * ONE_ETH_IN_USDC, 1_000).expect("quotable");
        assert_eq!(cheap.taker_amount, dear.taker_amount, "the same money either way");
        assert!(dear.base_amount * 40 < cheap.base_amount, "and far less of the dearer token");
    }

    /// The whole point of the book: two quotes for the same inventory must not both be honoured.
    #[test]
    fn a_reservation_stops_the_same_inventory_being_promised_twice() {
        let mut book = Book::new();
        let mut s = state();
        s.base_balance = 10 * ONE_ETH;

        book.quote(&params(), &s, true, 6 * ONE_ETH_IN_USDC, 1_000).expect("first fits");
        assert!(book.reserved(1, true) > 5 * ONE_ETH, "roughly six base reserved");
        assert_eq!(
            book.quote(&params(), &s, true, 6 * ONE_ETH_IN_USDC, 1_000),
            Err(Refusal::InsufficientInventory)
        );
        assert!(book.quote(&params(), &s, true, 3 * ONE_ETH_IN_USDC, 1_000).is_ok());
    }

    #[test]
    fn a_reservation_is_released_when_the_order_can_no_longer_be_filled() {
        let mut book = Book::new();
        let mut s = state();
        s.base_balance = 10 * ONE_ETH;
        book.quote(&params(), &s, false, 10 * ONE_ETH, 1_000).expect("fits");
        assert_eq!(book.open_len(), 1);

        book.expire(1_000 + params().ttl_secs - 1);
        assert_eq!(book.reserved(1, false), 10 * ONE_ETH, "still live one second before expiry");

        book.expire(1_000 + params().ttl_secs + 1);
        assert_eq!(book.reserved(1, false), 0);
        assert!(book.quote(&params(), &s, false, 10 * ONE_ETH, 2_000).is_ok());
    }

    /// The two sides draw on different assets, so a sell reservation must not block a buy.
    #[test]
    fn the_two_sides_reserve_independently() {
        let mut book = Book::new();
        let mut s = state();
        s.base_balance = 10 * ONE_ETH;
        book.quote(&params(), &s, true, 10 * ONE_ETH_IN_USDC, 1_000).expect("fits");
        assert_eq!(book.reserved(1, false), 0);
        assert!(book.quote(&params(), &s, false, ONE_ETH, 1_000).is_ok());
    }

    /// RFQ is the newer claim, so RFQ yields to what the pool has already published on chain.
    #[test]
    fn the_curves_epoch_is_subtracted_before_rfq_may_quote() {
        let mut book = Book::new();
        let mut s = state();
        s.base_balance = 10 * ONE_ETH;
        s.epoch_ask_base = 9 * ONE_ETH;
        assert_eq!(
            book.quote(&params(), &s, true, 5 * ONE_ETH_IN_USDC, 1_000),
            Err(Refusal::InsufficientInventory)
        );
        assert!(book.quote(&params(), &s, true, ONE_ETH_IN_USDC / 2, 1_000).is_ok());
    }

    /// Buying base spends quote, so an empty quote balance must stop the bid even with base to spare.
    #[test]
    fn buying_base_is_bounded_by_the_quote_on_hand() {
        let mut book = Book::new();
        let mut s = state();
        s.quote_balance = 0;
        assert_eq!(book.quote(&params(), &s, false, ONE_ETH, 1_000), Err(Refusal::InsufficientInventory));
    }

    #[test]
    fn nonces_do_not_repeat_within_a_run() {
        let mut book = Book::new();
        let first = book.next_nonce();
        book.quote(&params(), &state(), false, ONE_ETH, 1_000).expect("quotable");
        assert_ne!(book.next_nonce(), first);
    }

    #[test]
    fn the_expiry_is_the_ttl_from_now_and_the_order_says_so() {
        let mut book = Book::new();
        let q = book.quote(&params(), &state(), false, ONE_ETH, 1_000).expect("quotable");
        assert_eq!(q.expiry, 1_000 + params().ttl_secs);
        assert_eq!(q.min_fill_bps, params().min_fill_bps);
    }

    /// Which asset is on which leg is the field most likely to be silently inverted, and an
    /// inverted order is one the taker's own validation should reject — but ours should never emit
    /// it in the first place.
    #[test]
    fn the_legs_name_the_right_assets_for_each_direction() {
        let mut book = Book::new();
        let s = state();

        let buy = book.quote(&params(), &s, true, ONE_ETH_IN_USDC, 1_000).expect("quotable");
        assert_eq!(buy.maker_asset, s.base, "the taker buying base means the maker delivers base");
        assert_eq!(buy.taker_asset, s.quote);
        assert_eq!(buy.taker_amount, ONE_ETH_IN_USDC);
        assert_eq!(buy.base_amount, buy.maker_amount, "the reserved leg is the base one");

        let sell = book.quote(&params(), &s, false, ONE_ETH, 1_000).expect("quotable");
        assert_eq!(sell.maker_asset, s.quote);
        assert_eq!(sell.taker_asset, s.base);
        assert_eq!(sell.taker_amount, ONE_ETH);
        assert_eq!(sell.base_amount, ONE_ETH);
    }
}
