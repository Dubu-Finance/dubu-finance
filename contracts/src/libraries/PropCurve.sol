// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title PropCurve
/// @notice Pure integer math for the DuBu proprietary AMM curve.
///
/// # The curve
///
/// A linear-impact single tick, not a constant product. Each side is a price ladder pushed by
/// the off-chain quoter, and **both sides are denominated in BASE units** — capacity, usage,
/// and the size the ladder is a function of:
///
///   bid side (pool buys base):  maxBid at zero usage, decaying linearly to minBid at full capacity
///   ask side (pool sells base): minAsk at zero usage, rising  linearly to maxAsk at full capacity
///
/// A trade consuming the base interval `[u, u + q]` of a side with capacity `C` is charged the
/// average price over that interval, which for a linear ladder is exactly its value at the
/// midpoint:
///
///   avgBid(u, q) = maxBid - (maxBid - minBid) * (u + q/2) / C
///   avgAsk(u, q) = minAsk + (maxAsk - minAsk) * (u + q/2) / C
///
/// and the quote leg is `q * avg / 10**priceScaleExp`. Both legs are therefore **linear in the
/// price**, which is what makes the curve additive: carving `[u, u+q]` into contiguous pieces
/// leaves the sum of the pieces' quote legs exactly unchanged.
///
/// Everything expressive — spread, skew, concentration, volatility response — is folded into
/// the four numbers off-chain. The chain does not model any of it.
///
/// # Amendment 1: base-denominated capacity on both sides
///
/// Ask capacity used to be denominated in the *quote* token so the ask side could take a
/// quote-denominated `amountIn` and compare it against capacity with no price conversion. That
/// made the ask output `amountIn * scale / avgAsk` — the *reciprocal* of the midpoint price.
/// `1/p` is convex, so by Jensen the midpoint rule always under-delivers and every extra slice
/// of a split integrates the true log-curve more finely. Splitting an ask was therefore
/// strictly dominant for the taker (measured excess: 0.004 bps at a 25 bps ladder, 0.062 at
/// 100, 1.49 at 500, 20.7 at 2000), which is the opposite of the design intent. The bid side
/// was never affected: `q * avgBid / scale` is linear in the price and exactly additive.
///
/// Denominating the ask in base removes the convexity. The cost is a change of
/// parameterisation: the ask side's natural input is now "how much base does the taker want",
/// with the quote cost derived ([`amountInAsk`]). Tracking base capacity while accepting a
/// quote-denominated input would be circular — `midUsage` would depend on the output, which
/// depends on the price, which depends on `midUsage`. A taker starting from a quote amount is
/// inverting that function, which is a quadratic in `q`; [`amountOutAsk`] solves it exactly.
///
/// # Amendment 2: one rounding per trade, and never on the price
///
/// The intermediate price is no longer quantised. Rounding the impact term to a whole price
/// unit and then multiplying by the trade size broke monotonicity: `ceil(discount)` jumps a
/// whole price unit, that unit multiplies the entire `amountIn`, and once
/// `amountIn > 10**priceScaleExp` the quantisation loss beats the marginal gain. An 18/6 pair
/// at price 3e9 with a 100 bps ladder over 30 base of capacity returned
/// `2.000000000000000000 -> 5998.000000` but `2.000000000000000001 -> 5997.999998`: one extra
/// wei paid, less quote back. Monotonicity is also the precondition that makes
/// `PropPool.getAmountIn`'s binary search exact, so breaking it broke that too.
///
/// Every function below folds the price into the amount and rounds **exactly once**, at the
/// end, always in the pool's favour: floor when the pool pays out, ceil when the pool collects.
///
/// ## The two single-rounding forms
///
/// Write `W = maxBid - minBid` (or `maxAsk - minAsk`), `C` for capacity and `u` for usage, both
/// in base units, `S = 10**priceScaleExp`, and `q` for the base leg. The exact rational quote
/// leg is
///
///   bid:  q * (maxBid - W*(u + q/2)/C) / S  ==  q * (2*maxBid*C - W*(2u + q)) / (2*C*S)
///   ask:  q * (minAsk + W*(u + q/2)/C) / S  ==  q * (2*minAsk*C + W*(2u + q)) / (2*C*S)
///
/// Doubling clears the half-integer midpoint, so the numerator is the *exact* integral of the
/// ladder over `[u, u+q]` with no intermediate rounding whatsoever. One multiply chain, one
/// division, one rounding:
///
///   amountOutBid = floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )
///   amountInAsk  =  ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )
///
/// The bid numerator cannot underflow: `u + q <= C` gives `2u + q <= 2C`, and `W <= maxBid`
/// gives `W*(2u + q) <= 2*maxBid*C`.
///
/// ## Monotonicity, proved on the whole domain
///
/// Both numerators are quadratics in `q`. With `A = 2*maxBid*C - 2*W*u` and
/// `B = 2*minAsk*C + 2*W*u`:
///
///   bid: N(q) = A*q - W*q^2   — a downward parabola, vertex at q* = A/(2W)
///   ask: M(q) = B*q + W*q^2   — an upward parabola, increasing for all q >= 0
///
/// The ask side is therefore strictly increasing everywhere. For the bid the vertex must lie at
/// or beyond the largest admissible `q`, which is `C - u`:
///
///   q* >= C - u
///     <=>  2*maxBid*C - 2*W*u >= 2*W*(C - u)
///     <=>  2*maxBid*C >= 2*W*C
///     <=>  maxBid >= maxBid - minBid
///     <=>  minBid >= 0                                                        ∎
///
/// which is true for **every** ladder, so no side condition on the ladder is needed and no
/// ladder `validateLadder` admits can violate it. The bound is tight only at `minBid == 0`,
/// where the vertex sits exactly at `q = C - u` and `N` is still non-decreasing on the closed
/// domain. Concretely `N(q+1) - N(q) = A - W*(2q+1) >= W >= 0` for all `q <= C - u - 1`. Since
/// `floor` and `ceil` are non-decreasing, both amounts are non-decreasing in `q` across the
/// entire domain.
///
/// ## Splitting
///
/// The exact rational integral is additive by construction. Therefore
///
///   * bid: `sum floor(x_i) <= floor(sum x_i)`, so **no decomposition of a bid ever collects
///     more quote than executing it whole**, and the shortfall is at most `n - 1` quote units
///     for `n` pieces — one output unit per extra piece, not a price tick times `amountIn`.
///   * ask (base-out): `sum ceil(x_i) >= ceil(sum x_i)`, so **no decomposition of an ask ever
///     pays less quote**, with the same `n - 1` unit bound in the pool's favour.
///   * ask (quote-in): `amountOutAsk` is the exact inverse of an additive function, hence
///     super-additive — `f(X1 + X2) >= f(X1) + f'(X2)` — so splitting cannot buy more base.
///
/// The pre-amendment comment on `amountOutBid` claimed one-directional rounding made splitting
/// "provably" unable to beat a whole trade. That was false: it compared two independently
/// quantised *prices*, and the residual was one price tick scaled by the whole trade size.
/// Folding the price into the amount is what makes the claim true, and it is now bounded by one
/// *amount* unit per piece.
///
/// # Overflow (re-derived; the pre-amendment argument no longer applies)
///
/// Widths from `IPropPool.PairSnapshot`: prices are `uint56`, capacities and usages `uint96`,
/// `priceScaleExp <= 38`. Every intermediate, in uint256:
///
///   | expression                | bound                                  | value      |
///   |---------------------------|----------------------------------------|------------|
///   | `2*maxBid*C`, `2*minAsk*C`| `2 * 2^56 * 2^96`                      | `< 2^153`  |
///   | `2u + q`                  | `2C`                                   | `< 2^97`   |
///   | `W * (2u + q)`            | `2^56 * 2^97`                          | `< 2^153`  |
///   | bid numerator `q * N`     | `2^96 * 2^153`                         | `< 2^249`  |
///   | ask numerator `q * M`     | `2^96 * 2^154`                         | `< 2^250`  |
///   | denominator `2*C*S`       | `2^97 * 10^38 < 2^97 * 2^127`          | `< 2^224`  |
///   | ceil's `num + den - 1`    | `2^250 + 2^224`                        | `< 2^251`  |
///   | bracket seeds `Y * S`     | `2^128 * 10^38`                        | `< 2^255`  |
///
/// The widest intermediate is `2^251`, leaving 32 bits of headroom in uint256. The bracket
/// seeds are the tightest at `2^255`, and they are bounded only because [`MAX_AMOUNT_OUT`]
/// caps the quote-denominated argument at `uint128`.
///
/// Note what changed: the old argument bounded `amountIn * 10**38` at `~7.9e66 < 2^223`. The
/// new numerators are `2^250`, an extra 27 bits, because the price is no longer divided out
/// early. The off-chain mirror can no longer carry these in `u128` behind a range proof — it
/// needs a genuine 256-bit intermediate, which `dubu_core::math::U256` provides.
///
/// # Gas
///
/// Two of the four directions are closed forms and two are bisections over the integer function,
/// because the quadratic's radical needs 512-bit intermediates *and* a 512-bit square root and
/// would invert the real curve rather than the integer one. Measured on a WETH/USDC 18/6 pair
/// with a 20 bps ladder over 100 WETH of capacity, `priceScaleExp = 24`, through an external
/// wrapper (so ~2.5k of each figure is call overhead):
///
///   | direction                    | gas   |
///   |------------------------------|-------|
///   | `amountInAsk`  (closed form) | ~3.0k |
///   | `amountOutBid` (closed form) | ~7.1k |
///   | `amountInBid`  (bisection)   | ~17k  |
///   | `amountOutAsk` (bisection)   | ~26k  |
///
/// The bisections are only affordable because of `_refineAskBracket` / `_refineBidBracket`:
/// without them the endpoint brackets are a *relative* interval and the two cost 67k and 87k.
/// Three refinement rounds is the measured optimum across both; more rounds are pure overhead on
/// a bps-scale ladder and fewer leave too many bisection steps.
///
/// **Known worst case, deliberately not optimised further.** The refinement is a fixed-point
/// iteration whose contraction factor is `(W*q/(2C)) / p`. On a ladder spanning most of the
/// `uint56` range — `minAsk = 1`, `maxAsk = 2^56 - 1` — that factor approaches 1 at the solution,
/// the refinement stalls, and the bisection runs its full course: ~112k gas, bounded above by
/// `ceil(log2(capacity - used)) <= 97` iterations. `validateLadder` admits such a ladder, but no
/// quoter produces one — the off-chain builder works in bps, and `PropPool`'s reference-oracle
/// deviation clamp bounds every price around the oracle mid — so this is a cost ceiling on a
/// misconfiguration, not a reachable production path. Closing it properly wants a Newton step,
/// which handles a unit multiplier where a fixed point cannot.
library PropCurve {
    error AmountExceedsCapacity();
    error ZeroCapacity();
    error ZeroPrice();
    error CrossedBook();
    error BidBelowMinPrice();
    error AmountOutOfDomain();

    /// @notice Upper bound on `priceScaleExp`. `10**38 < 2^127`, which is what keeps the
    ///         denominator `2*C*10**exp` under `2^224` and the bracket seeds under `2^255`.
    uint8 internal constant MAX_PRICE_SCALE_EXP = 38;

    /// @notice Hard ceiling on any quote-denominated amount this library returns or accepts.
    ///
    /// @dev This CLOSES THE DOMAIN, and it is still load bearing under the new arithmetic —
    /// arguably more so. Solidity computes in uint256; the Rust engine mirrors this math with a
    /// 256-bit intermediate but returns `u128`. The reachable quote legs are wider than before:
    /// `amountOutBid <= q * maxBid / S`, which at the `uint96` / `uint56` corners with `S == 1`
    /// is just under `2^152`. Without this bound the chain would settle a trade whose amount
    /// the engine cannot represent — the engine would treat a size as unquotable while the pool
    /// filled it, a silent-loss bug rather than a crash.
    ///
    /// It applies to four places, all of them quote-denominated:
    ///   * `amountOutBid`'s return  (quote out)
    ///   * `amountInAsk`'s return   (quote in)
    ///   * `amountOutAsk`'s argument (quote in) — an input bound, because the inversion's
    ///     bracket seeds multiply it by `10**38` and because a quote amount the mirror cannot
    ///     hold is outside the shared domain either way
    ///   * `amountInBid`'s argument  (quote out), for the same two reasons
    ///
    /// The base-denominated results need no check: they are bounded by `capacity - used`, hence
    /// by `uint96`.
    ///
    /// 2**128 of any token is astronomically beyond any real supply (a full uint128 of an
    /// 18-decimal token is ~3.4e20 whole units), so nothing reachable is rejected. The point is
    /// that on-chain and off-chain share exactly one domain.
    uint256 internal constant MAX_AMOUNT_OUT = type(uint128).max;

    // ---------------------------------------------------------------------
    // Bid side — the pool buys base, pays quote
    // ---------------------------------------------------------------------

    /// @notice Quote base -> quote (the pool buys `amountIn` of base). Exact input.
    /// @param amountIn       base token amount, in base's smallest unit
    /// @param minBid         worst bid, reached when bid capacity is fully consumed
    /// @param maxBid         best bid, offered at zero usage
    /// @param bidCapacity    total base the pool will buy this capacity epoch
    /// @param bidUsed        base already bought this capacity epoch
    /// @param priceScaleExp  per-pair decimal alignment; `quote = base * price / 10**exp`
    /// @return amountOut     quote token amount, floored — the pool pays out the lesser of the
    ///                       two representable amounts, and that is the only rounding applied.
    function amountOutBid(
        uint256 amountIn,
        uint256 minBid,
        uint256 maxBid,
        uint256 bidCapacity,
        uint256 bidUsed,
        uint8 priceScaleExp
    ) internal pure returns (uint256 amountOut) {
        if (amountIn == 0) return 0;
        if (bidCapacity == 0) revert ZeroCapacity();
        // Also establishes `2*bidUsed + amountIn <= 2*bidCapacity`, which is what makes the
        // numerator's subtraction underflow-free and puts the parabola's vertex out of reach.
        if (bidUsed + amountIn > bidCapacity) revert AmountExceedsCapacity();

        amountOut = _bidGross(amountIn, minBid, maxBid, bidCapacity, bidUsed, 10 ** priceScaleExp);
        if (amountOut > MAX_AMOUNT_OUT) revert AmountOutOfDomain();
    }

    /// @notice Least base input whose bid quote delivers at least `amountOut` of quote.
    ///
    /// @dev The exact-output counterpart of [`amountOutBid`]. Inverting the integer function
    ///      rather than the real-valued radical: `amountOutBid` is non-decreasing in `amountIn`
    ///      across the whole domain (proved in the module docs, unconditionally), so the least
    ///      `q` with `f(q) >= amountOut` is well defined and a bisection finds it exactly.
    ///
    ///      Reverts `AmountExceedsCapacity` when the epoch's whole remaining depth cannot
    ///      deliver `amountOut`, so the caller never has to interpret a clamped answer.
    /// @return amountIn base token amount, the least one that suffices — rounded up by
    ///         construction, which is the pool-favourable direction for an exact-output trade.
    function amountInBid(
        uint256 amountOut,
        uint256 minBid,
        uint256 maxBid,
        uint256 bidCapacity,
        uint256 bidUsed,
        uint8 priceScaleExp
    ) internal pure returns (uint256 amountIn) {
        if (amountOut == 0) return 0;
        if (bidCapacity == 0) revert ZeroCapacity();
        if (bidUsed >= bidCapacity) revert AmountExceedsCapacity();
        if (amountOut > MAX_AMOUNT_OUT) revert AmountOutOfDomain();

        uint256 scale = 10 ** priceScaleExp;
        uint256 hi = bidCapacity - bidUsed;
        // `_bidGross` is unbounded by MAX_AMOUNT_OUT on purpose: a gross above the cap is still
        // a correct comparison against `amountOut <= MAX_AMOUNT_OUT`, and reverting here would
        // refuse a request the epoch can in fact fill.
        if (_bidGross(hi, minBid, maxBid, bidCapacity, bidUsed, scale) < amountOut) {
            revert AmountExceedsCapacity();
        }

        uint256 lo = 1;
        uint256 ys = amountOut * scale; // < 2^255; see the overflow table
        // Bracket, from the ladder endpoints rather than the realised average, so it holds
        // regardless of rounding. `f(q) <= floor(q*maxBid/S)`, so `f(q) >= amountOut` forces
        // `q >= ceil(amountOut*S/maxBid)`: a lower bound on the answer.
        if (maxBid != 0) {
            uint256 seed = (ys + maxBid - 1) / maxBid;
            if (seed > lo && seed <= hi) lo = seed;
        }
        // `f(q) >= floor(q*minBid/S)`, so `q = ceil(amountOut*S/minBid)` already satisfies
        // `floor(q*minBid/S) >= amountOut`: an upper bound on the answer.
        if (minBid != 0) {
            uint256 seed = (ys + minBid - 1) / minBid;
            if (seed >= lo && seed < hi) hi = seed;
        }

        // Two-sided fixed-point refinement, mirroring the ask side's and for the same reason.
        // See `_refineBidBracket` for the derivation and the validity argument.
        (lo, hi) = _refineBidBracket(ys, lo, hi, maxBid, maxBid - minBid, bidUsed, 2 * bidCapacity);

        // Least-true bisection. Invariant: `f(hi) >= amountOut` and `f(lo - 1) < amountOut`.
        while (lo < hi) {
            uint256 mid = lo + (hi - lo) / 2;
            if (_bidGross(mid, minBid, maxBid, bidCapacity, bidUsed, scale) >= amountOut) hi = mid;
            else lo = mid + 1;
        }
        amountIn = hi;
    }

    // ---------------------------------------------------------------------
    // Ask side — the pool sells base, collects quote
    // ---------------------------------------------------------------------

    /// @notice Quote cost of `amountOut` base (the pool sells base). Exact output.
    ///
    /// @dev This is the ask side's *primitive*: base capacity in, quote cost out. It is the
    ///      direction that keeps the curve linear in the price; see amendment 1 in the module
    ///      docs for why the old quote-denominated parameterisation was convex.
    /// @param amountOut      base token amount the taker receives
    /// @param minAsk         best ask, offered at zero usage
    /// @param maxAsk         worst ask, reached when ask capacity is fully consumed
    /// @param askCapacity    total base the pool will sell this capacity epoch
    /// @param askUsed        base already sold this capacity epoch
    /// @return amountIn      quote token amount, ceiled — the pool collects the greater of the
    ///                       two representable amounts, and that is the only rounding applied.
    function amountInAsk(
        uint256 amountOut,
        uint256 minAsk,
        uint256 maxAsk,
        uint256 askCapacity,
        uint256 askUsed,
        uint8 priceScaleExp
    ) internal pure returns (uint256 amountIn) {
        if (amountOut == 0) return 0;
        if (askCapacity == 0) revert ZeroCapacity();
        if (askUsed + amountOut > askCapacity) revert AmountExceedsCapacity();
        // `maxAsk >= minAsk`, so this is exactly "the ask ladder prices nothing". Any validated
        // ladder has `maxAsk > minBid >= minPrice >= 0`, hence `maxAsk >= 1`, so this is
        // unreachable through `PropPool` — same reachability as the pre-amendment `ZeroPrice`,
        // which fired only on an all-zero ask ladder. Without it the pool would hand over base
        // for nothing.
        if (maxAsk == 0) revert ZeroPrice();

        amountIn = _askCost(amountOut, minAsk, maxAsk, askCapacity, askUsed, 10 ** priceScaleExp);
        if (amountIn > MAX_AMOUNT_OUT) revert AmountOutOfDomain();
    }

    /// @notice Most base `amountIn` of quote strictly pays for. Exact input, quote-denominated.
    ///
    /// @dev The inversion of [`amountInAsk`]. Over the reals this is the positive root of
    ///
    ///        W*q^2 + (2*minAsk*C + 2*W*u)*q - 2*C*S*X = 0
    ///
    ///      but the radical is unusable in a word: `b^2` reaches `2^308` and `2*C*S*X` reaches
    ///      `2^352`, so a closed form needs 512-bit intermediates *and* a 512-bit square root,
    ///      and it would invert the real curve rather than the integer one. Instead this
    ///      bisects the integer function, which is exact: `amountInAsk` is strictly increasing
    ///      in `q` (an upward parabola through the origin — module docs), so
    ///      `max{ q : cost(q) <= X }` is well defined, and `ceil(y) <= X` iff `y <= X` for
    ///      integral `X`, so the integer predicate is the rational one.
    ///
    ///      Rounding: the answer is the largest `q` the taker's quote covers, so the taker
    ///      never receives more base than `amountIn` strictly pays for. Any unspent remainder
    ///      stays with the pool.
    ///
    ///      Because the curve is additive and this is its exact inverse, the result is
    ///      super-additive in `amountIn`: splitting a quote-denominated ask can never buy more
    ///      base than spending it in one go.
    /// @return amountOut base token amount, in base's smallest unit.
    function amountOutAsk(
        uint256 amountIn,
        uint256 minAsk,
        uint256 maxAsk,
        uint256 askCapacity,
        uint256 askUsed,
        uint8 priceScaleExp
    ) internal pure returns (uint256 amountOut) {
        if (amountIn == 0) return 0;
        if (askCapacity == 0) revert ZeroCapacity();
        if (askUsed >= askCapacity) revert AmountExceedsCapacity();
        if (maxAsk == 0) revert ZeroPrice();
        // An input bound, not an output bound. See MAX_AMOUNT_OUT: the mirror cannot represent
        // a larger quote amount, and the bracket seeds below multiply this by `10**38`.
        if (amountIn > MAX_AMOUNT_OUT) revert AmountOutOfDomain();

        uint256 scale = 10 ** priceScaleExp;
        uint256 lo;
        uint256 hi;
        // Scoped so `room` and `full` are off the stack before the refinement call. Without this
        // the function exceeds the legacy code generator's 16-slot limit; `via_ir` is off for
        // fast iteration (foundry.toml) and this library must build either way.
        {
            uint256 room = askCapacity - askUsed;
            // The epoch's quote-denominated ceiling is the cost of all its remaining base.
            // Raising `AmountExceedsCapacity` above it preserves the pre-amendment contract for a
            // quote-denominated `amountIn`, and avoids silently charging more than the base is
            // worth. `_askCost` is deliberately uncapped here; `amountIn <= MAX_AMOUNT_OUT`, so a
            // cost above the cap is still a correct comparison.
            uint256 full = _askCost(room, minAsk, maxAsk, askCapacity, askUsed, scale);
            if (amountIn > full) revert AmountExceedsCapacity();
            if (amountIn == full) return room;
            // `cost(room) > amountIn`, so the answer is in `[0, room - 1]`.
            hi = room - 1;
        }

        uint256 xs = amountIn * scale; // < 2^255; see the overflow table
        // Bracket from the ladder endpoints. `cost(q) <= ceil(q*maxAsk/S)`, so any
        // `q <= floor(X*S/maxAsk)` is affordable: a lower bound on the answer. It provably cannot
        // exceed `hi` — affordability is monotone and `room` is not affordable — so the clamp is
        // a no-op for every in-domain input. It is written anyway, and the `>= lo` guard on the
        // second seed likewise, because the off-chain mirror needs them to stay total on
        // out-of-domain input and the two implementations must be the same algorithm, not merely
        // agree where both are defined.
        {
            uint256 seed = xs / maxAsk;
            if (seed > hi) seed = hi;
            if (seed > lo) lo = seed;
            // `cost(q) >= q*minAsk/S`, so affordability forces `q <= floor(X*S/minAsk)`: an upper
            // bound on the answer.
            if (minAsk != 0) {
                seed = xs / minAsk;
                if (seed < hi && seed >= lo) hi = seed;
            }
        }

        // Two-sided fixed-point refinement, and it is what makes this affordable on chain.
        // See `_refineAskBracket` for the derivation and the validity argument.
        (lo, hi) = _refineAskBracket(xs, lo, hi, minAsk, maxAsk - minAsk, askUsed, 2 * askCapacity);

        // Greatest-true bisection. Invariant: `cost(lo) <= amountIn < cost(hi + 1)`.
        while (lo < hi) {
            uint256 mid = lo + (hi - lo + 1) / 2;
            if (_askCost(mid, minAsk, maxAsk, askCapacity, askUsed, scale) <= amountIn) lo = mid;
            else hi = mid - 1;
        }
        amountOut = lo;
    }

    // ---------------------------------------------------------------------
    // The two numerators
    // ---------------------------------------------------------------------

    /// @dev Tighten the ask bracket `[lo, hi]` toward the answer without ever invalidating it.
    ///
    /// The endpoint brackets in `amountOutAsk` are a *relative* interval: their ratio is
    /// `maxAsk / minAsk`, so on a 20 bps ladder a bare bisection still needs ~53 iterations,
    /// about 40k gas, on the primary ask swap path. Each round here replaces a ladder endpoint
    /// with the average price at the current bracket end, which contracts the interval by the
    /// ladder's own relative width per round instead of by a factor of two.
    ///
    /// Write `p(q) = minAsk + W*(2u + q) / (2C)` for the exact rational average price over
    /// `[u, u + q]`. It is increasing in `q`, and affordability is `q * p(q) <= xs`, where
    /// `xs = amountIn * 10**priceScaleExp`.
    ///
    ///   * `lo` is affordable, hence `lo <= q*`, hence `p(lo) <= p(q*)`, hence
    ///     `q* <= xs / p(lo)`. Flooring the price only loosens the bound.        -> new `hi`
    ///   * every `q <= hi` has `p(q) <= p(hi)`, so `q * p(q) <= q * p(hi) <= xs` for every
    ///     `q <= xs / p(hi)`: such a `q` is affordable, hence `<= q*`. Ceiling the price only
    ///     loosens it.                                                           -> new `lo`
    ///
    /// Both ends therefore remain valid at every step, so the bisection that follows is exact
    /// regardless of how well this converges. Three rounds is a gas tuning constant, not a
    /// correctness parameter — but the off-chain mirror must run the same number to stay
    /// bit-exact, because the *bracket* is observable through the returned value only when the
    /// bisection is cut short, which it never is. It is mirrored anyway, on principle.
    function _refineAskBracket(
        uint256 xs,
        uint256 lo,
        uint256 hi,
        uint256 minAsk,
        uint256 span,
        uint256 u,
        uint256 den
    ) private pure returns (uint256, uint256) {
        for (uint256 r; r < 3 && lo < hi; ++r) {
            uint256 p = minAsk + (span * (2 * u + lo)) / den; // <= p(lo)
            if (p != 0) {
                uint256 seed = xs / p;
                if (seed < hi) hi = seed;
            }
            uint256 num = span * (2 * u + hi);
            p = minAsk + (num + den - 1) / den; // >= p(hi)
            if (p != 0) {
                uint256 seed = xs / p;
                if (seed > hi) seed = hi;
                if (seed > lo) lo = seed;
            }
        }
        return (lo, hi);
    }

    /// @dev Tighten the bid exact-output bracket. Mirror of [`_refineAskBracket`], with
    ///      `p(q) = maxBid - W*(2u + q)/(2C)` *decreasing* in `q` and feasibility `q*p(q) >= ys`:
    ///
    ///   * `hi` is feasible, so `q* <= hi`, so `p(q) >= p(hi)` for every `q <= hi`; hence any
    ///     `q >= ys / p(hi)` that is `<= hi` is feasible. Rounding the price DOWN keeps that
    ///     true.                                                                 -> new `hi`
    ///   * `q* >= lo`, so `p(q*) <= p(lo)`; hence `ys <= q* * p(q*) <= q* * p(lo)` forces
    ///     `q* >= ys / p(lo)`. Rounding the price UP keeps that true.            -> new `lo`
    function _refineBidBracket(
        uint256 ys,
        uint256 lo,
        uint256 hi,
        uint256 maxBid,
        uint256 span,
        uint256 u,
        uint256 den
    ) private pure returns (uint256, uint256) {
        for (uint256 r; r < 3 && lo < hi; ++r) {
            // ceil the price = floor the drift: `>= p(lo)`.
            uint256 p = maxBid - (span * (2 * u + lo)) / den;
            if (p != 0) {
                uint256 seed = (ys + p - 1) / p;
                if (seed > hi) seed = hi;
                if (seed > lo) lo = seed;
            }
            // floor the price = ceil the drift: `<= p(hi)`.
            uint256 num = span * (2 * u + hi);
            p = maxBid - (num + den - 1) / den;
            if (p != 0) {
                uint256 seed = (ys + p - 1) / p;
                if (seed < hi && seed >= lo) hi = seed;
            }
        }
        return (lo, hi);
    }

    /// @dev `floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )`, uncapped.
    ///
    ///      Preconditions, all established by the callers: `C > 0`, `u + q <= C`,
    ///      `maxBid >= minBid`. Together they make the subtraction underflow-free.
    function _bidGross(uint256 q, uint256 minBid, uint256 maxBid, uint256 c, uint256 u, uint256 scale)
        private
        pure
        returns (uint256)
    {
        uint256 span = maxBid - minBid;
        return (q * (2 * maxBid * c - span * (2 * u + q))) / (2 * c * scale);
    }

    /// @dev `ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )`, uncapped.
    function _askCost(uint256 q, uint256 minAsk, uint256 maxAsk, uint256 c, uint256 u, uint256 scale)
        private
        pure
        returns (uint256)
    {
        uint256 span = maxAsk - minAsk;
        uint256 num = q * (2 * minAsk * c + span * (2 * u + q));
        uint256 den = 2 * c * scale;
        return (num + den - 1) / den;
    }

    // ---------------------------------------------------------------------
    // Ladder validation
    // ---------------------------------------------------------------------

    /// @notice Reject any ladder that is crossed, inverted, or below the pair's floor.
    /// @dev This is the on-chain backstop against a compromised or buggy updater. It
    ///      cannot stop a signer from quoting a *wrong* price, only an *incoherent* one —
    ///      the reference-oracle deviation bound in PropPool handles the former.
    ///
    ///      Ordering required:  maxAsk >= minAsk >= maxBid >= minBid >= minPrice
    ///      plus a strict       maxAsk >  minBid
    ///
    ///      The strict comparison is also what makes `ZeroPrice` unreachable on the ask side:
    ///      it forces `maxAsk >= 1`.
    function validateLadder(uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk, uint256 minPrice)
        internal
        pure
    {
        if (minBid < minPrice) revert BidBelowMinPrice();
        // A single chained comparison: any inversion or crossing trips it.
        if (!(maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid)) revert CrossedBook();
        if (maxAsk <= minBid) revert CrossedBook();
    }

    // ---------------------------------------------------------------------
    // Introspection helpers (off-chain / router use)
    // ---------------------------------------------------------------------

    /// @notice The price a taker would receive *right now* for an infinitesimal bid.
    ///
    /// @dev The quoter compares against this, not against `maxBid`. `maxBid` is the price at
    ///      zero usage; once the epoch is partly consumed the executable top has already moved,
    ///      and triggering on `maxBid` would misjudge how far the book has drifted.
    ///
    ///      **The floor/ceil asymmetry is resolved, and the resolution is that BOTH sides ceil
    ///      the drift.** Both helpers used to floor it, which was justified as "matching the
    ///      quote path". Under the amended arithmetic the quote path rounds no price at all, so
    ///      the reference is the exact rational zero-size limit `maxBid - W*u/C`, and flooring
    ///      the *drift* reports a bid at or **above** it — a higher bid, which is better for the
    ///      taker. That was the real direction of the old asymmetry, on this side too, and it is
    ///      the wrong one for a trigger: reading the book as better than it executes makes the
    ///      quoter re-quote late.
    ///
    ///      Ceiling the drift on both sides moves the reported price away from the taker on both
    ///      sides — the bid down, the ask up — by strictly less than one price unit. It also
    ///      makes these helpers *exactly* the zero-size limit of the average-price reporting
    ///      helpers the off-chain engine uses, so the asymmetry does not merely change shape, it
    ///      disappears.
    function executableTopBid(uint256 minBid, uint256 maxBid, uint256 bidCapacity, uint256 bidUsed)
        internal
        pure
        returns (uint256)
    {
        if (bidCapacity == 0) return 0;
        if (bidUsed >= bidCapacity) return minBid;
        // `bidUsed < bidCapacity`, so the ceiling is at most `maxBid - minBid` and the result
        // stays at or above `minBid`.
        uint256 drift = (maxBid - minBid) * bidUsed;
        return maxBid - (drift + bidCapacity - 1) / bidCapacity;
    }

    /// @notice The price a taker would pay right now for an infinitesimal ask.
    ///
    /// @dev CEILS the drift, for the same reason and in the same direction as
    ///      [`executableTopBid`]: away from the taker. See that function's note.
    function executableTopAsk(uint256 minAsk, uint256 maxAsk, uint256 askCapacity, uint256 askUsed)
        internal
        pure
        returns (uint256)
    {
        if (askCapacity == 0) return type(uint256).max;
        if (askUsed >= askCapacity) return maxAsk;
        uint256 drift = (maxAsk - minAsk) * askUsed;
        return minAsk + (drift + askCapacity - 1) / askCapacity;
    }
}
