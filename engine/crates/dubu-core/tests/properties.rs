//! Property tests for the PropCurve mirror.
//!
//! Five families, matching the five claims this crate makes:
//!
//! 1. **Totality** — no input, however absurd, panics. A quoter that panics stops quoting.
//! 2. **Monotonicity** — every amount is non-decreasing in its argument, across the whole
//!    domain. This is the defect the `ceil(discount)` quantisation caused and the amended
//!    single-rounding form fixes; it is also the precondition that makes `PropPool.getAmountIn`'s
//!    binary search exact.
//! 3. **Additivity** — no decomposition of a trade beats executing it whole, on *either* side,
//!    and no decomposition beats the exact rational integral of the ladder. Both are now
//!    theorems rather than bounded residuals; see the module docs of [`dubu_core::curve`].
//! 4. **Inversion** — the two exact-output directions are the exact integer inverses of the two
//!    exact-input directions, and the inverse ladder solver's round trip never favours the taker.
//! 5. **Validator equivalence** — [`dubu_core::validate_ladder`] accepts exactly the set
//!    `PropCurve.validateLadder` accepts, checked against an independent transliteration.

use dubu_core::curve::{
    amount_in_ask, amount_in_bid, amount_out_ask, amount_out_bid, avg_ask_price, avg_bid_price,
    executable_top_ask, executable_top_bid, validate_ladder, AMOUNT_MAX, NO_ASK, PRICE_MAX,
    PRICE_SCALE_EXP_MAX,
};
use dubu_core::error::{CurveError, LadderError};
use dubu_core::inverse::{solve_ask, solve_bid, solve_two_sided, SolveInput, WidthBinding};
use dubu_core::ladder::LadderBuilder;
use dubu_core::vectors;
use num_bigint::BigUint;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// In-domain price (`uint56`).
fn price() -> impl Strategy<Value = u128> {
    prop_oneof![
        3 => 0u128..=PRICE_MAX,
        1 => Just(0u128),
        1 => Just(1u128),
        1 => Just(PRICE_MAX),
        1 => Just(PRICE_MAX - 1),
    ]
}

/// In-domain amount (`uint96`).
fn amount() -> impl Strategy<Value = u128> {
    prop_oneof![
        3 => 0u128..=AMOUNT_MAX,
        1 => Just(0u128),
        1 => Just(1u128),
        1 => Just(AMOUNT_MAX),
    ]
}

/// Any `u128`, including values far outside the documented domain.
fn wild() -> impl Strategy<Value = u128> {
    prop_oneof![
        2 => any::<u128>(),
        1 => 0u128..=AMOUNT_MAX,
        1 => Just(u128::MAX),
        1 => Just(0u128),
    ]
}

/// An ordered pair of in-domain prices, `(low, high)`.
fn ladder_pair() -> impl Strategy<Value = (u128, u128)> {
    (0u128..=PRICE_MAX, 0u128..=PRICE_MAX).prop_map(|(a, b)| (a.min(b), a.max(b)))
}

/// An ordered, in-domain ladder that `validateLadder` accepts against `min_price = 0`.
fn valid_ladder() -> impl Strategy<Value = (u128, u128, u128, u128)> {
    (
        0u128..=PRICE_MAX,
        0u128..=PRICE_MAX,
        0u128..=PRICE_MAX,
        0u128..=PRICE_MAX,
    )
        .prop_map(|(a, b, c, d)| {
            let mut v = [a, b, c, d];
            v.sort_unstable();
            // Guarantee the strict maxAsk > minBid the validator demands.
            if v[3] == v[0] {
                if v[3] < PRICE_MAX {
                    v[3] += 1;
                } else {
                    v[0] -= 1;
                }
            }
            (v[0], v[1], v[2], v[3])
        })
}

// ---------------------------------------------------------------------------
// Exact rational oracles, in BigUint
// ---------------------------------------------------------------------------
//
// Nothing on chain computes these. They are independent reference implementations of the *exact
// integral of the ladder* over `[used, used + q]`, so a differential failure means the U256 path
// is wrong rather than that the two agree on being wrong. The numerators reach 2^250, which is
// why the production code needs `U256` and this needs BigUint.

fn b(v: u128) -> BigUint {
    BigUint::from(v)
}

/// `(numerator, denominator)` of the exact rational quote leg of the bid side.
fn exact_bid(
    q: u128,
    min_bid: u128,
    max_bid: u128,
    capacity: u128,
    used: u128,
    exp: u8,
) -> (BigUint, BigUint) {
    let span = b(max_bid) - b(min_bid);
    let factor = b(2) * b(max_bid) * b(capacity) - span * (b(2) * b(used) + b(q));
    (
        b(q) * factor,
        b(2) * b(capacity) * b(10).pow(u32::from(exp)),
    )
}

/// `(numerator, denominator)` of the exact rational quote leg of the ask side.
fn exact_ask(
    q: u128,
    min_ask: u128,
    max_ask: u128,
    capacity: u128,
    used: u128,
    exp: u8,
) -> (BigUint, BigUint) {
    let span = b(max_ask) - b(min_ask);
    let factor = b(2) * b(min_ask) * b(capacity) + span * (b(2) * b(used) + b(q));
    (
        b(q) * factor,
        b(2) * b(capacity) * b(10).pow(u32::from(exp)),
    )
}

fn floor_div(n: &BigUint, d: &BigUint) -> BigUint {
    n / d
}

fn ceil_div(n: &BigUint, d: &BigUint) -> BigUint {
    (n + d - 1u32) / d
}

/// Split `q` into `n` contiguous pieces and quote each from the usage the previous one left.
fn pieces_of(q: u128, n: u32) -> Vec<u128> {
    let mut out = Vec::with_capacity(n as usize);
    let mut remaining = q;
    for i in 0..n {
        let piece = remaining / u128::from(n - i);
        out.push(piece);
        remaining -= piece;
    }
    out
}

// ---------------------------------------------------------------------------
// 1. Totality
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Out-of-domain inputs trip `debug_assert!`s by design, so totality is asserted on the
    /// documented domain. `wild_inputs_never_panic_in_release` covers the rest.
    #[test]
    fn in_domain_inputs_never_panic(
        arg in amount(),
        p0 in price(),
        p1 in price(),
        capacity in amount(),
        used in amount(),
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let _ = amount_out_bid(arg, p0, p1, capacity, used, exp);
        let _ = amount_in_bid(arg, p0, p1, capacity, used, exp);
        let _ = amount_in_ask(arg, p0, p1, capacity, used, exp);
        let _ = amount_out_ask(arg, p0, p1, capacity, used, exp);
        let _ = validate_ladder(p0, p1, p0, p1, p0);
        let _ = executable_top_bid(p0, p1, capacity, used);
        let _ = executable_top_ask(p0, p1, capacity, used);
    }

    /// Totality outside the domain, where the debug assertions are compiled out. Values here
    /// reach `u128::MAX`, which no on-chain state can produce — the point is only that a
    /// malformed RPC response cannot take the quoter down.
    #[test]
    #[cfg(not(debug_assertions))]
    fn wild_inputs_never_panic_in_release(
        arg in wild(),
        p0 in wild(),
        p1 in wild(),
        capacity in wild(),
        used in wild(),
        exp: u8,
    ) {
        let _ = amount_out_bid(arg, p0, p1, capacity, used, exp);
        let _ = amount_in_bid(arg, p0, p1, capacity, used, exp);
        let _ = amount_in_ask(arg, p0, p1, capacity, used, exp);
        let _ = amount_out_ask(arg, p0, p1, capacity, used, exp);
        let _ = validate_ladder(p0, p1, p0, p1, p0);
        let _ = executable_top_bid(p0, p1, capacity, used);
        let _ = executable_top_ask(p0, p1, capacity, used);
    }

    /// The helpers that do run outside `debug_assert` guards must be total everywhere.
    #[test]
    fn helpers_never_panic(p0 in wild(), p1 in wild(), capacity in wild(), usage in wild(), q in wild()) {
        let _ = avg_bid_price(p0, p1, capacity, usage, q);
        let _ = avg_ask_price(p0, p1, capacity, usage, q);
        let _ = validate_ladder(p0, p1, capacity, usage, p0);
    }

    /// The builder is fed straight from strategy state and must never panic or fail on any
    /// combination of knobs.
    #[test]
    fn ladder_builder_never_panics(
        mid in wild(),
        half_spread_bps_e2: u32,
        width_bps_e2: u32,
        skew_bps: i16,
        min_price in price(),
    ) {
        let bldr = LadderBuilder { reference_mid: mid, half_spread_bps_e2, width_bps_e2, skew_bps,
                                   min_price, round_ask_up: true };
        match bldr.build() {
            Ok(l) => {
                prop_assert_eq!(l.validate(min_price), Ok(()));
                prop_assert!(l.max_ask <= PRICE_MAX);
            }
            Err(e) => prop_assert!(
                matches!(e, LadderError::PriceOutOfRange | LadderError::InfeasibleBounds),
                "unexpected builder error {e:?}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Monotonicity — DEFECT 2
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// **The property `ceil(discount)` broke.** `amountOutBid` must be non-decreasing in
    /// `amountIn` across the whole domain: paying one more wei can never return less quote.
    ///
    /// The old form quantised the price to a whole unit and then multiplied by the entire
    /// `amountIn`, so a one-unit jump in the ceiled discount cost `amountIn / scale` output units
    /// while the marginal wei bought only `avgBid / scale`. Once `amountIn > 10**priceScaleExp`
    /// the quantisation loss won, and the witness in
    /// `dubu_core::curve::tests::one_extra_wei_never_returns_less` is a realistic 18/6 pair.
    ///
    /// Folding the price into the amount makes the numerator a downward parabola in `amountIn`
    /// whose vertex provably lies at or beyond `capacity - used` for every ladder (see the
    /// `curve` module docs), and `floor` preserves monotonicity. So this holds unconditionally,
    /// with no side condition on the ladder — which is what `PropPool.getAmountIn`'s binary
    /// search needs to be exact.
    #[test]
    fn bid_output_is_non_decreasing_in_input(
        (min_bid, max_bid) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        q in amount(),
        delta in 1u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let used = used % capacity;
        let room = capacity - used;
        prop_assume!(room >= 2);
        let q1 = q % room;
        let q2 = (q1 + (delta % room) + 1).min(room);
        prop_assume!(q1 < q2);

        let a1 = amount_out_bid(q1, min_bid, max_bid, capacity, used, exp);
        let a2 = amount_out_bid(q2, min_bid, max_bid, capacity, used, exp);
        if let (Ok(x), Ok(y)) = (a1, a2) {
            prop_assert!(y >= x, "output fell from {x} to {y} as size grew {q1} -> {q2}");
        }
        // A larger size can leave the shared domain — the quotient grows past `uint128` — but
        // that is the only way it may fail where the smaller succeeded. Anything else would mean
        // the domain is not an upward-closed interval in `amountIn`, which the binary search in
        // `PropPool.getAmountIn` also relies on.
        if let (Ok(_), Err(e)) = (a1, a2) {
            prop_assert_eq!(e, CurveError::AmountOutOfDomain, "size grew {} -> {}", q1, q2);
        }
    }

    /// Adjacent sizes, which is where the old defect actually bit: `q` and `q + 1`.
    #[test]
    fn one_more_unit_never_returns_less(
        (min_bid, max_bid) in ladder_pair(),
        capacity in 2u128..=AMOUNT_MAX,
        used in amount(),
        q in amount(),
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let used = used % capacity;
        let room = capacity - used;
        prop_assume!(room >= 2);
        let q = q % (room - 1);
        let (lo, hi) = (
            amount_out_bid(q, min_bid, max_bid, capacity, used, exp),
            amount_out_bid(q + 1, min_bid, max_bid, capacity, used, exp),
        );
        if let (Ok(lo), Ok(hi)) = (lo, hi) {
            prop_assert!(hi >= lo, "q={q}: {lo} -> {hi}");
        }
        // The ask cost is strictly increasing (an upward parabola through the origin), so the
        // same step on the ask side must never get cheaper either.
        let (lo, hi) = (
            amount_in_ask(q, min_bid, max_bid, capacity, used, exp),
            amount_in_ask(q + 1, min_bid, max_bid, capacity, used, exp),
        );
        if let (Ok(lo), Ok(hi)) = (lo, hi) {
            prop_assert!(hi >= lo, "ask cost fell at q={q}: {lo} -> {hi}");
        }
    }

    /// **The sharp form of the monotonicity property, and the one that actually kills the
    /// pre-amendment arithmetic.**
    ///
    /// The two properties above have an escape hatch: a larger size may legitimately leave the
    /// shared domain, and at small `priceScaleExp` with `uint96` sizes it usually does — so the
    /// region where the old `ceil(discount)` defect bit was being *skipped*, not tested. Verified
    /// by mutation: reinstating the pre-amendment form leaves both of them green.
    ///
    /// This one derives the capacity from the domain bound instead of rejecting afterwards. With
    /// `q * maxBid / scale <= AMOUNT_OUT_MAX` guaranteed by construction, every quote below is
    /// in domain, no case is discarded, and `q -> q + 1` is checked with nothing swallowed. The
    /// same construction sharpens the ask leg.
    #[test]
    fn monotonic_wei_by_wei_with_the_domain_bound_built_in(
        max_bid in 1u128..=PRICE_MAX,
        width_bps in 0u128..=2_000,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
        capacity_seed: u128,
        offset: u128,
        used_seed: u128,
    ) {
        let scale = 10u128.pow(u32::from(exp));
        let min_bid = max_bid - max_bid * width_bps / 10_000;
        // Largest size whose quote leg still fits `AMOUNT_OUT_MAX`, since the leg is bounded
        // above by `q * maxBid / scale`.
        let q_cap = dubu_core::math::mul_div_floor(u128::MAX, scale, max_bid)
            .unwrap_or(AMOUNT_MAX)
            .clamp(2, AMOUNT_MAX);
        let capacity = 2 + capacity_seed % (q_cap - 1);
        let used = used_seed % capacity;
        let room = capacity - used;
        prop_assume!(room >= 2);
        let q = offset % (room - 1);

        // In domain by construction, on both legs: assert that rather than tolerate a failure.
        let lo = amount_out_bid(q, min_bid, max_bid, capacity, used, exp)
            .expect("bid leg is in domain by construction");
        let hi = amount_out_bid(q + 1, min_bid, max_bid, capacity, used, exp)
            .expect("bid leg is in domain by construction");
        prop_assert!(hi >= lo, "bid: q={q} gave {lo}, q+1 gave {hi} (one extra unit returned less)");

        let lo = amount_in_ask(q, min_bid, max_bid, capacity, used, exp)
            .expect("ask leg is in domain by construction");
        let hi = amount_in_ask(q + 1, min_bid, max_bid, capacity, used, exp)
            .expect("ask leg is in domain by construction");
        prop_assert!(hi >= lo, "ask: q={q} cost {lo}, q+1 cost {hi} (one extra unit cost less)");
    }

    /// **A constructed family that provably triggers the pre-amendment defect.**
    ///
    /// Random search will essentially never find DEFECT 2, and it is worth being explicit about
    /// why rather than trusting a green fuzzer. The old `discount = ceil(span * m / C)` only
    /// *steps* between `q` and `q + 1` when `span * m / C` sits exactly on an integer, which for
    /// unconstrained `(span, m, C)` happens with probability about `span / C` — around `2^-16` on
    /// the strategies above, and `10^-12` on the realistic 18/6 witness the Foundry suite used.
    /// The Foundry witness was hand-picked for precisely that reason. A property test that only
    /// samples randomly reports green against the defective arithmetic; verified by mutation.
    ///
    /// So construct the coincidence instead. With
    ///
    /// ```text
    /// span = 2,  C = 2M,  u = M/2,  q = M,  maxBid = M,  exp = 0
    /// ```
    ///
    /// the doubled midpoint is `2u + q = 2M`, so `span*(2u+q) / (2C) = 4M / 4M = 1` exactly: the
    /// old ceiling was poised on an integer and stepped on the very next unit. One more unit of
    /// size took the old `discount` from 1 to 2 while buying only `avgBid` more, and since
    /// `q = M > avgBid = M - 1` the quantisation loss won:
    ///
    /// ```text
    /// old:  out(q)   = M*(M - 1)     = M^2 - M
    ///       out(q+1) = (M+1)*(M - 2) = M^2 - M - 2      <-- two units LESS for one more wei
    /// ```
    ///
    /// The amended form cannot express that, because there is no intermediate price to step.
    #[test]
    fn the_quantisation_witness_family_is_monotonic(
        m in 4u128..=(PRICE_MAX - 1) / 2,
        exp in 0u8..=3,
    ) {
        let m = m * 2; // even, so `u = m/2` is exact
        let (min_bid, max_bid) = (m - 2, m);
        let (capacity, used, q) = (2 * m, m / 2, m);
        prop_assume!(used + q < capacity);

        let lo = amount_out_bid(q, min_bid, max_bid, capacity, used, exp).expect("in domain");
        let hi = amount_out_bid(q + 1, min_bid, max_bid, capacity, used, exp).expect("in domain");
        prop_assert!(hi >= lo, "M={m}: q gave {lo}, q+1 gave {hi} — the DEFECT 2 witness is back");

        // The ask leg's cost must not fall on the same family either.
        let lo = amount_in_ask(q, min_bid, max_bid, capacity, used, exp).expect("in domain");
        let hi = amount_in_ask(q + 1, min_bid, max_bid, capacity, used, exp).expect("in domain");
        prop_assert!(hi >= lo, "M={m}: ask cost fell from {lo} to {hi}");
    }

    /// The quote-denominated ask path is non-decreasing in the budget: more quote never buys
    /// less base.
    #[test]
    fn ask_inversion_is_non_decreasing_in_budget(
        (min_ask, max_ask) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        x in amount(),
        delta in 1u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let used = used % capacity;
        let x2 = x.saturating_add(delta);
        match (
            amount_out_ask(x, min_ask, max_ask, capacity, used, exp),
            amount_out_ask(x2, min_ask, max_ask, capacity, used, exp),
        ) {
            (Ok(a), Ok(c)) => prop_assert!(c >= a, "budget {x} -> {x2} bought {a} -> {c}"),
            // The larger budget may exceed the epoch's ceiling; the smaller may not.
            (Ok(_), Err(CurveError::AmountExceedsCapacity)) => {}
            (Err(_), _) => {}
            (a, c) => prop_assert!(false, "unexpected pair {a:?} / {c:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Additivity — DEFECT 1
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Differential check of the bid leg against an independent BigUint evaluation of the exact
    /// rational integral. This is what makes the `U256` path trustworthy: the production code and
    /// this oracle share no arithmetic.
    #[test]
    fn bid_leg_is_the_floored_exact_integral(
        (min_bid, max_bid) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        q in amount(),
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let used = used % capacity;
        let q = q % (capacity - used + 1);
        let (num, den) = exact_bid(q, min_bid, max_bid, capacity, used, exp);
        let want = floor_div(&num, &den);
        match amount_out_bid(q, min_bid, max_bid, capacity, used, exp) {
            Ok(got) => prop_assert_eq!(BigUint::from(got), want),
            Err(CurveError::AmountOutOfDomain) => prop_assert!(want > BigUint::from(u128::MAX)),
            Err(e) => prop_assert!(false, "unexpected {e:?}"),
        }
    }

    /// The same, for the ask leg, which ceils.
    #[test]
    fn ask_leg_is_the_ceiled_exact_integral(
        (min_ask, max_ask) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        q in amount(),
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        prop_assume!(max_ask > 0);
        let used = used % capacity;
        let q = q % (capacity - used + 1);
        prop_assume!(q > 0);
        let (num, den) = exact_ask(q, min_ask, max_ask, capacity, used, exp);
        let want = ceil_div(&num, &den);
        match amount_in_ask(q, min_ask, max_ask, capacity, used, exp) {
            Ok(got) => prop_assert_eq!(BigUint::from(got), want),
            Err(CurveError::AmountOutOfDomain) => prop_assert!(want > BigUint::from(u128::MAX)),
            Err(e) => prop_assert!(false, "unexpected {e:?}"),
        }
    }

    /// **DEFECT 1, both sides, as an exact theorem rather than a bounded residual.**
    ///
    /// The exact rational integral is additive: carving `[0, q]` into contiguous pieces leaves
    /// the sum unchanged. The single rounding per piece then decides the direction, and both
    /// directions point at the pool:
    ///
    /// ```text
    /// bid:  sum floor(x_i) <= floor(sum x_i)   =>  split <= whole,  whole - split <= n - 1
    /// ask:  sum  ceil(x_i) >=  ceil(sum x_i)   =>  split >= whole,  split - whole <= n - 1
    /// ```
    ///
    /// The upper bounds hold because each piece's rounding moves it by strictly less than one
    /// unit and the whole trade's rounding accounts for one of them.
    ///
    /// This replaces the old `splitting_a_bid_costs_at_most_one_price_tick`, whose residual was
    /// *one price tick times the trade size* and which therefore could not assert `split <=
    /// whole` at all. On the ask side the old arithmetic was strictly worse than a residual:
    /// output was the reciprocal of the midpoint price, which is convex, so splitting an ask was
    /// **systematically** profitable for the taker (0.004 bps of excess at a 25 bps ladder, 20.7
    /// bps at 2000 bps). Both are gone.
    #[test]
    fn splitting_never_beats_one_shot_on_either_side(
        (lo_price, hi_price) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        q in 2u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
        n in 1u32..=8,
    ) {
        let q = q % capacity.max(2);
        prop_assume!(q >= u128::from(n));
        let pieces = pieces_of(q, n);
        let slack = u128::from(n - 1);

        // -- bid: floors, so the split collects at most the whole ------------------------
        if let Ok(whole) = amount_out_bid(q, lo_price, hi_price, capacity, 0, exp) {
            let mut total = 0u128;
            let mut used = 0u128;
            let mut ok = true;
            for &piece in &pieces {
                match amount_out_bid(piece, lo_price, hi_price, capacity, used, exp) {
                    Ok(part) => total += part,
                    Err(_) => { ok = false; break }
                }
                used += piece;
            }
            if ok {
                prop_assert!(total <= whole, "{n} pieces collected {total} > {whole}");
                prop_assert!(whole - total <= slack, "shortfall {} exceeds {slack}", whole - total);
            }
        }

        // -- ask: ceils, so the split pays at least the whole ---------------------------
        if hi_price > 0 {
            if let Ok(whole) = amount_in_ask(q, lo_price, hi_price, capacity, 0, exp) {
                let mut total = 0u128;
                let mut used = 0u128;
                let mut ok = true;
                for &piece in &pieces {
                    match amount_in_ask(piece, lo_price, hi_price, capacity, used, exp) {
                        Ok(part) => total += part,
                        Err(_) => { ok = false; break }
                    }
                    used += piece;
                }
                if ok {
                    prop_assert!(total >= whole, "{n} pieces paid {total} < {whole}");
                    prop_assert!(total - whole <= slack, "excess {} exceeds {slack}", total - whole);
                }
            }
        }
    }

    /// No decomposition of a bid extracts more quote than the exact rational integral, and none
    /// of an ask pays less than it. The stronger form of the previous property: it compares
    /// against the ladder's true value rather than against the undivided trade, so it would catch
    /// a bug that inflated *both*.
    #[test]
    fn no_decomposition_beats_the_exact_curve(
        (lo_price, hi_price) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        q in 2u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
        n in 1u32..=8,
    ) {
        let q = q % capacity.max(2);
        prop_assume!(q >= u128::from(n));
        let pieces = pieces_of(q, n);

        let (num, den) = exact_bid(q, lo_price, hi_price, capacity, 0, exp);
        let exact_out = floor_div(&num, &den);
        let mut total = BigUint::ZERO;
        let mut used = 0u128;
        let mut ok = true;
        for &piece in &pieces {
            match amount_out_bid(piece, lo_price, hi_price, capacity, used, exp) {
                Ok(part) => total += b(part),
                Err(_) => { ok = false; break }
            }
            used += piece;
        }
        if ok {
            prop_assert!(total <= exact_out, "{n} pieces extracted {total} against exact {exact_out}");
        }

        if hi_price > 0 {
            let (num, den) = exact_ask(q, lo_price, hi_price, capacity, 0, exp);
            let exact_in = ceil_div(&num, &den);
            let mut total = BigUint::ZERO;
            let mut used = 0u128;
            let mut ok = true;
            for &piece in &pieces {
                match amount_in_ask(piece, lo_price, hi_price, capacity, used, exp) {
                    Ok(part) => total += b(part),
                    Err(_) => { ok = false; break }
                }
                used += piece;
            }
            if ok {
                prop_assert!(total >= exact_in, "{n} pieces paid {total} against exact {exact_in}");
            }
        }
    }
}

proptest! {
    // The quote-denominated ask path runs a bisection with a 256-bit divider inside, so keep the
    // case count modest here; the arithmetic it exercises is already covered above.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Splitting a **quote-denominated** ask cannot buy more base either.
    ///
    /// `f_u(X) = max{ q : cost_u(q) <= X }` is super-additive, which is the exact inverse
    /// statement of the ask leg's subadditivity: if `q1 = f_u(X1)` and `q2 = f_{u+q1}(X2)` then
    /// `cost_u(q1 + q2) = ceil(exact_u(q1) + exact_{u+q1}(q2)) <= cost_u(q1) + cost_{u+q1}(q2)
    /// <= X1 + X2`, so `q1 + q2` is affordable in one go and `f_u(X1 + X2) >= q1 + q2`. ∎
    ///
    /// The residual runs the other way from the base-out side and is worth stating: the one-shot
    /// trade can be strictly *better* for the taker, because each piece forfeits the quote left
    /// between its budget and the next base unit's marginal cost. That is bounded by `n - 1`
    /// marginal steps, i.e. `n - 1` base units when a base unit costs at least one quote unit,
    /// and by `(n - 1) * scale / minAsk` base units in the dust regime where one quote unit buys
    /// many base units. It is pool-favourable, so it is not a leak — the pool always collects at
    /// or above the exact integral of what it hands over, which
    /// `the_ask_inversion_never_gives_away_more_than_it_collects` states separately.
    #[test]
    fn splitting_a_quote_denominated_ask_never_buys_more_base(
        (min_ask, max_ask) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        x1 in 1u128..=AMOUNT_MAX,
        x2 in 1u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        prop_assume!(max_ask > 0);
        let Ok(q1) = amount_out_ask(x1, min_ask, max_ask, capacity, 0, exp) else { return Ok(()) };
        let Ok(q2) = amount_out_ask(x2, min_ask, max_ask, capacity, q1, exp) else { return Ok(()) };
        let Ok(whole) = amount_out_ask(x1 + x2, min_ask, max_ask, capacity, 0, exp) else { return Ok(()) };
        prop_assert!(whole >= q1 + q2, "split bought {} > one-shot {whole}", q1 + q2);
    }

    /// The inversion is exactly maximal, and the pool is never short: the base it hands over
    /// costs at most the quote it collected.
    #[test]
    fn the_ask_inversion_never_gives_away_more_than_it_collects(
        (min_ask, max_ask) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        x in 1u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        prop_assume!(max_ask > 0);
        let used = used % capacity;
        let Ok(q) = amount_out_ask(x, min_ask, max_ask, capacity, used, exp) else { return Ok(()) };
        // What the pool collects (x) covers the exact integral of what it sells (q).
        if q > 0 {
            let cost = amount_in_ask(q, min_ask, max_ask, capacity, used, exp).expect("q is affordable");
            prop_assert!(cost <= x, "sold {q} for {x} but it costs {cost}");
        }
        // Maximal: one more base unit is unaffordable.
        if used + q < capacity {
            match amount_in_ask(q + 1, min_ask, max_ask, capacity, used, exp) {
                Ok(more) => prop_assert!(more > x, "q={q} was not maximal for budget {x}"),
                // Above AMOUNT_OUT_MAX is certainly above `x <= AMOUNT_MAX`.
                Err(CurveError::AmountOutOfDomain) => {}
                Err(e) => prop_assert!(false, "unexpected {e:?}"),
            }
        }
    }

    /// `amount_in_bid` returns the *least* base input that delivers the requested quote.
    #[test]
    fn the_bid_exact_output_path_is_minimal(
        (min_bid, max_bid) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
        y in 1u128..=AMOUNT_MAX,
        exp in 0u8..=PRICE_SCALE_EXP_MAX,
    ) {
        let used = used % capacity;
        let Ok(q) = amount_in_bid(y, min_bid, max_bid, capacity, used, exp) else { return Ok(()) };
        prop_assert!(q >= 1 && used + q <= capacity);
        let got = amount_out_bid(q, min_bid, max_bid, capacity, used, exp).expect("in domain");
        prop_assert!(got >= y, "least input {q} delivered {got} < {y}");
        if q > 1 {
            let less = amount_out_bid(q - 1, min_bid, max_bid, capacity, used, exp).expect("in domain");
            prop_assert!(less < y, "input {} already delivered {less} >= {y}", q - 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Inverse round trip
// ---------------------------------------------------------------------------

fn solve_input() -> impl Strategy<Value = SolveInput> {
    (
        0u128..=PRICE_MAX,
        amount(),
        1u128..=AMOUNT_MAX,
        prop_oneof![Just(PRICE_MAX), 0u128..=PRICE_MAX],
        0u128..=PRICE_MAX,
        0u128..=PRICE_MAX,
    )
        .prop_map(|(target, capture, capacity, requested_width, a, c)| {
            let (min_price, max_price) = (a.min(c), a.max(c));
            SolveInput {
                target: target.clamp(min_price, max_price),
                capture,
                capacity,
                requested_width,
                min_price,
                max_price,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4096, ..ProptestConfig::default() })]

    /// The headline claim, restated for the amended forward map. The produced ladder fed back
    /// through the chain's own arithmetic over the capture size lands **at or below** the target
    /// on the bid side, within one price unit, and exactly on it whenever `2*capacity` divides
    /// `width * effective_capture`.
    ///
    /// Exactness for arbitrary widths is no longer available and asserting it would be the bug:
    /// the forward map's implied average is the exact rational `maxBid - W*K/(2C)`, which is
    /// integral only when `2C | W*K`, and forcing that would collapse the width to zero for a
    /// small capture against a large capacity. See `inverse` §3.
    #[test]
    fn bid_round_trip_never_favours_the_taker(input in solve_input()) {
        let sol = solve_bid(&input).expect("in-domain input must solve");
        let realised = avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();

        prop_assert!(realised <= input.target, "realised {realised} beat target {}", input.target);
        prop_assert!(input.target - realised <= 1);
        // Exact precisely when the division is.
        let exact = dubu_core::math::mul_div_rem(sol.width, sol.effective_capture, 2 * input.capacity)
            .map(|(_, r)| r == 0);
        if exact == Some(true) {
            prop_assert_eq!(realised, input.target, "the residual must vanish when 2C divides W*K");
        }

        // Structural guarantees.
        prop_assert_eq!(sol.high - sol.low, sol.width);
        prop_assert!(sol.low >= input.min_price);
        prop_assert!(sol.high <= input.max_price);
        prop_assert!(sol.high <= PRICE_MAX);
        prop_assert!(sol.width <= input.requested_width);
        prop_assert_eq!(sol.effective_capture, input.capture.min(input.capacity));

        // In amount terms: never pays a taker more than a flat ladder posted at the target.
        for exp in [8u8, 18, 24, PRICE_SCALE_EXP_MAX] {
            let solved = amount_out_bid(sol.effective_capture, sol.low, sol.high, input.capacity, 0, exp);
            let flat = amount_out_bid(sol.effective_capture, input.target, input.target, input.capacity, 0, exp);
            if let (Ok(solved), Ok(flat)) = (solved, flat) {
                prop_assert!(solved <= flat, "solved {solved} beat the flat target ladder {flat}");
            }
        }
    }

    #[test]
    fn ask_round_trip_never_favours_the_taker(input in solve_input()) {
        let sol = solve_ask(&input).expect("in-domain input must solve");
        let realised = avg_ask_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();

        prop_assert!(realised >= input.target, "realised {realised} beat target {}", input.target);
        prop_assert!(realised - input.target <= 1);

        prop_assert_eq!(sol.high - sol.low, sol.width);
        prop_assert!(sol.low >= input.min_price);
        prop_assert!(sol.high <= input.max_price);
        prop_assert!(sol.width <= input.requested_width);

        // In amount terms: never charges a taker less than a flat ladder posted at the target.
        for exp in [8u8, 18, 24, PRICE_SCALE_EXP_MAX] {
            let solved = amount_in_ask(sol.effective_capture, sol.low, sol.high, input.capacity, 0, exp);
            let flat = amount_in_ask(sol.effective_capture, input.target, input.target, input.capacity, 0, exp);
            if let (Ok(solved), Ok(flat)) = (solved, flat) {
                prop_assert!(solved >= flat, "solved {solved} undercharged against the flat ladder {flat}");
            }
        }
    }

    /// Widest, not merely feasible: whatever [`WidthBinding`] the solver reports is the thing that
    /// actually bound the width, and one more unit would breach it.
    ///
    /// The probe must use the same FLOOR the solver uses. Probing with a ceiling tests a solver
    /// that does not exist, and would have declared the previous revision's ceiled `W_boundary`
    /// maximal while it was in fact one unit over the price ceiling.
    #[test]
    fn the_solved_width_is_maximal(input in solve_input()) {
        let sol = solve_bid(&input).expect("in-domain input must solve");
        match sol.binding {
            WidthBinding::Requested => prop_assert_eq!(sol.width, input.requested_width),
            WidthBinding::Saturated => prop_assert_eq!(sol.width, PRICE_MAX),
            WidthBinding::Boundary | WidthBinding::Endpoint => {
                prop_assert!(sol.width < PRICE_MAX && sol.width < input.requested_width);
                let w = sol.width + 1;
                let impact =
                    dubu_core::math::mul_div_floor(w, sol.effective_capture, 2 * input.capacity).unwrap();
                let breaks_ceiling = input.target + impact > input.max_price;
                let breaks_floor = w - impact > input.target - input.min_price;
                prop_assert!(breaks_ceiling || breaks_floor, "width {} was not maximal", sol.width);
            }
        }
    }

    /// Two-sided rows always satisfy the on-chain validator, and the bid side is never silently
    /// degraded to make that happen.
    #[test]
    fn two_sided_rows_are_always_valid(a in solve_input(), c in solve_input()) {
        let floor = a.min_price.min(c.min_price);
        let bid = SolveInput { min_price: floor, target: a.target.max(floor), ..a };
        let ask = SolveInput { min_price: floor, target: c.target.max(floor), ..c };
        prop_assume!(bid.target <= bid.max_price && ask.target <= ask.max_price);

        match solve_two_sided(&bid, &ask) {
            Ok(out) => {
                prop_assert_eq!(out.ladder.validate(floor), Ok(()));
                prop_assert_eq!(out.ladder.min_bid, out.bid.low);
                prop_assert_eq!(out.ladder.max_bid, out.bid.high);
                if out.ask_repaired {
                    prop_assert!(out.ladder.min_ask >= out.ask.low);
                    prop_assert!(out.ladder.max_ask >= out.ask.high);
                } else {
                    prop_assert_eq!(out.ladder.min_ask, out.ask.low);
                    prop_assert_eq!(out.ladder.max_ask, out.ask.high);
                }
            }
            Err(e) => prop_assert_eq!(e, LadderError::InfeasibleBounds),
        }
    }

    /// Solving never fails on a well-formed in-domain request.
    #[test]
    fn solving_is_total_on_the_domain(input in solve_input()) {
        prop_assert!(solve_bid(&input).is_ok());
        prop_assert!(solve_ask(&input).is_ok());
    }

    /// `executableTop{Bid,Ask}` are **exactly** the zero-size limit of the average-price
    /// reporting helpers, on both sides.
    ///
    /// This is the resolution of the floor/ceil asymmetry the previous revision only flagged.
    /// Both helpers used to floor their drift term, which reports a bid *above* and an ask
    /// *below* the exact rational zero-size limit — better for the taker on both sides, and the
    /// wrong direction for an update trigger, which would then re-quote late. Both now ceil, so
    /// each reported price is at or worse for the taker than the exact limit by strictly less
    /// than one price unit, and the equality below holds with no slack at all.
    #[test]
    fn executable_top_is_exactly_the_zero_size_average(
        (lo_price, hi_price) in ladder_pair(),
        capacity in 1u128..=AMOUNT_MAX,
        used in amount(),
    ) {
        let top_bid = executable_top_bid(lo_price, hi_price, capacity, used).unwrap();
        prop_assert!(top_bid >= lo_price && top_bid <= hi_price);
        let top_ask = executable_top_ask(lo_price, hi_price, capacity, used).unwrap();
        prop_assert!(top_ask >= lo_price && top_ask <= hi_price);
        prop_assert_ne!(top_ask, NO_ASK, "NO_ASK is only for zero capacity");

        if used < capacity {
            prop_assert_eq!(Ok(top_bid), avg_bid_price(lo_price, hi_price, capacity, used, 0));
            prop_assert_eq!(Ok(top_ask), avg_ask_price(lo_price, hi_price, capacity, used, 0));
            // And each is on the pool's side of the exact rational drift, by under one unit.
            let drift = b(hi_price - lo_price) * b(used);
            let den = b(capacity);
            prop_assert!(b(hi_price) - b(top_bid) >= floor_div(&drift, &den));
            prop_assert!(b(top_ask) - b(lo_price) >= floor_div(&drift, &den));
            prop_assert!(b(hi_price) - b(top_bid) <= ceil_div(&drift, &den));
            prop_assert!(b(top_ask) - b(lo_price) <= ceil_div(&drift, &den));
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Validator equivalence
// ---------------------------------------------------------------------------

/// Independent transliteration of `PropCurve.validateLadder`, written from the Solidity source
/// rather than from `dubu_core`'s implementation, so the two can disagree.
///
/// ```solidity
/// if (minBid < minPrice) revert BidBelowMinPrice();
/// if (!(maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid)) revert CrossedBook();
/// if (maxAsk <= minBid) revert CrossedBook();
/// ```
fn solidity_validate(
    min_bid: u128,
    max_bid: u128,
    min_ask: u128,
    max_ask: u128,
    min_price: u128,
) -> Option<&'static str> {
    if min_bid < min_price {
        return Some("BidBelowMinPrice");
    }
    if !(max_ask >= min_ask && min_ask >= max_bid && max_bid >= min_bid) {
        return Some("CrossedBook");
    }
    if max_ask <= min_bid {
        return Some("CrossedBook");
    }
    None
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 8192, ..ProptestConfig::default() })]

    /// Unbiased inputs: mostly rejections, but they pin the rejection *reason*.
    #[test]
    fn validator_matches_the_solidity_transliteration(
        min_bid in wild(), max_bid in wild(), min_ask in wild(), max_ask in wild(), min_price in wild(),
    ) {
        let ours = validate_ladder(min_bid, max_bid, min_ask, max_ask, min_price);
        let theirs = solidity_validate(min_bid, max_bid, min_ask, max_ask, min_price);
        match (ours, theirs) {
            (Ok(()), None) => {}
            (Err(e), Some(name)) => {
                prop_assert_eq!(e.solidity_signature().unwrap().trim_end_matches("()"), name);
            }
            (a, c) => prop_assert!(false, "disagreement: ours={a:?} solidity={c:?}"),
        }
    }

    /// Biased toward acceptance, so the accepting branch actually gets exercised.
    #[test]
    fn validator_accepts_every_well_ordered_ladder(
        (min_bid, max_bid, min_ask, max_ask) in valid_ladder(),
        floor in 0u128..=PRICE_MAX,
    ) {
        let min_price = floor.min(min_bid);
        prop_assert_eq!(validate_ladder(min_bid, max_bid, min_ask, max_ask, min_price), Ok(()));
        prop_assert_eq!(solidity_validate(min_bid, max_bid, min_ask, max_ask, min_price), None);
        // Every accepted ladder has `maxAsk >= 1`, which is what makes `ZeroPrice` unreachable
        // on the ask side through `PropPool`.
        prop_assert!(max_ask >= 1);
    }

    /// Perturbing an accepted ladder by one unit in the wrong direction must reject.
    #[test]
    fn validator_rejects_every_one_unit_perturbation(
        (min_bid, max_bid, min_ask, max_ask) in valid_ladder(),
    ) {
        prop_assume!(min_bid > 0);
        prop_assert_eq!(
            validate_ladder(min_bid, max_bid, min_ask, max_ask, min_bid + 1),
            Err(CurveError::BidBelowMinPrice)
        );
        prop_assert_eq!(
            validate_ladder(min_bid, min_ask + 1, min_ask, max_ask.max(min_ask + 1), 0),
            Err(CurveError::CrossedBook)
        );
        prop_assert_eq!(
            validate_ladder(min_bid, min_bid - 1, min_ask, max_ask, 0),
            Err(CurveError::CrossedBook)
        );
    }
}

// ---------------------------------------------------------------------------
// Vector-file consistency
// ---------------------------------------------------------------------------

#[test]
fn generated_vectors_are_self_consistent() {
    let all = vectors::generate();
    assert!(!all.is_empty());
    for v in &all {
        assert!(
            vectors::verify(v),
            "vector `{}` does not reproduce its own output",
            v.name
        );
        if !v.expectRevert.is_empty() {
            assert!(
                vectors::SOLIDITY_ERRORS.contains(&v.expectRevert.as_str()),
                "vector `{}` expects {}",
                v.name,
                v.expectRevert
            );
        }
    }
    assert!(
        all.iter().any(|v| v.expectRevert == "AmountOutOfDomain"),
        "no vector asserts the shared AMOUNT_OUT_MAX boundary"
    );
    assert!(
        all.iter().any(|v| v.expectRevert == "ZeroPrice"),
        "no vector asserts the zero-ask-ladder guard"
    );
}

/// The checked-in file must match what the generator produces right now. If this fails, re-run
/// `cargo run --bin gen-vectors` and commit the result alongside the change.
#[test]
fn checked_in_vector_file_is_current() {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..3 {
        path.pop();
    }
    path.push("contracts/testdata/curve_vectors.json");

    let Ok(on_disk) = std::fs::read_to_string(&path) else {
        eprintln!("skipping: {} not generated yet", path.display());
        return;
    };
    let expected = vectors::to_json(&vectors::generate()).unwrap();
    assert_eq!(
        on_disk,
        expected,
        "{} is stale; re-run `cargo run --bin gen-vectors`",
        path.display()
    );
}
