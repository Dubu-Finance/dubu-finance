// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

library PropCurve {
    error AmountExceedsCapacity();
    error ZeroCapacity();
    error ZeroPrice();
    error CrossedBook();
    error BidBelowMinPrice();
    error AmountOutOfDomain();

    uint8 internal constant MAX_PRICE_SCALE_EXP = 38;

    uint256 internal constant MAX_AMOUNT_OUT = type(uint128).max;

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

        if (bidUsed + amountIn > bidCapacity) revert AmountExceedsCapacity();

        amountOut = _bidGross(amountIn, minBid, maxBid, bidCapacity, bidUsed, 10 ** priceScaleExp);
        if (amountOut > MAX_AMOUNT_OUT) revert AmountOutOfDomain();
    }

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

        if (_bidGross(hi, minBid, maxBid, bidCapacity, bidUsed, scale) < amountOut) {
            revert AmountExceedsCapacity();
        }

        uint256 lo = 1;
        uint256 ys = amountOut * scale;

        if (maxBid != 0) {
            uint256 seed = (ys + maxBid - 1) / maxBid;
            if (seed > lo && seed <= hi) lo = seed;
        }

        if (minBid != 0) {
            uint256 seed = (ys + minBid - 1) / minBid;
            if (seed >= lo && seed < hi) hi = seed;
        }

        (lo, hi) = _refineBidBracket(ys, lo, hi, maxBid, maxBid - minBid, bidUsed, 2 * bidCapacity);

        while (lo < hi) {
            uint256 mid = lo + (hi - lo) / 2;
            if (_bidGross(mid, minBid, maxBid, bidCapacity, bidUsed, scale) >= amountOut) hi = mid;
            else lo = mid + 1;
        }
        amountIn = hi;
    }

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

        if (maxAsk == 0) revert ZeroPrice();

        amountIn = _askCost(amountOut, minAsk, maxAsk, askCapacity, askUsed, 10 ** priceScaleExp);
        if (amountIn > MAX_AMOUNT_OUT) revert AmountOutOfDomain();
    }

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

        if (amountIn > MAX_AMOUNT_OUT) revert AmountOutOfDomain();

        uint256 scale = 10 ** priceScaleExp;
        uint256 lo;
        uint256 hi;

        {
            uint256 room = askCapacity - askUsed;

            uint256 full = _askCost(room, minAsk, maxAsk, askCapacity, askUsed, scale);
            if (amountIn > full) revert AmountExceedsCapacity();
            if (amountIn == full) return room;

            hi = room - 1;
        }

        uint256 xs = amountIn * scale;

        {
            uint256 seed = xs / maxAsk;
            if (seed > hi) seed = hi;
            if (seed > lo) lo = seed;

            if (minAsk != 0) {
                seed = xs / minAsk;
                if (seed < hi && seed >= lo) hi = seed;
            }
        }

        (lo, hi) = _refineAskBracket(xs, lo, hi, minAsk, maxAsk - minAsk, askUsed, 2 * askCapacity);

        while (lo < hi) {
            uint256 mid = lo + (hi - lo + 1) / 2;
            if (_askCost(mid, minAsk, maxAsk, askCapacity, askUsed, scale) <= amountIn) lo = mid;
            else hi = mid - 1;
        }
        amountOut = lo;
    }

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
            uint256 p = minAsk + (span * (2 * u + lo)) / den;
            if (p != 0) {
                uint256 seed = xs / p;
                if (seed < hi) hi = seed;
            }
            uint256 num = span * (2 * u + hi);
            p = minAsk + (num + den - 1) / den;
            if (p != 0) {
                uint256 seed = xs / p;
                if (seed > hi) seed = hi;
                if (seed > lo) lo = seed;
            }
        }
        return (lo, hi);
    }

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

            uint256 p = maxBid - (span * (2 * u + lo)) / den;
            if (p != 0) {
                uint256 seed = (ys + p - 1) / p;
                if (seed > hi) seed = hi;
                if (seed > lo) lo = seed;
            }

            uint256 num = span * (2 * u + hi);
            p = maxBid - (num + den - 1) / den;
            if (p != 0) {
                uint256 seed = (ys + p - 1) / p;
                if (seed < hi && seed >= lo) hi = seed;
            }
        }
        return (lo, hi);
    }

    function _bidGross(uint256 q, uint256 minBid, uint256 maxBid, uint256 c, uint256 u, uint256 scale)
        private
        pure
        returns (uint256)
    {
        uint256 span = maxBid - minBid;
        return (q * (2 * maxBid * c - span * (2 * u + q))) / (2 * c * scale);
    }

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

    function validateLadder(uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk, uint256 minPrice)
        internal
        pure
    {
        if (minBid < minPrice) revert BidBelowMinPrice();

        if (!(maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid)) revert CrossedBook();
        if (maxAsk <= minBid) revert CrossedBook();
    }

    function executableTopBid(uint256 minBid, uint256 maxBid, uint256 bidCapacity, uint256 bidUsed)
        internal
        pure
        returns (uint256)
    {
        if (bidCapacity == 0) return 0;
        if (bidUsed >= bidCapacity) return minBid;

        uint256 drift = (maxBid - minBid) * bidUsed;
        return maxBid - (drift + bidCapacity - 1) / bidCapacity;
    }

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
