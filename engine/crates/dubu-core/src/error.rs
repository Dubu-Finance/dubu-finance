//! Error types.
//!
//! [`CurveError`] is the boundary between "the chain would revert" and "the chain would not".
//! Everything the engine refuses to send must be traceable to one of these.

use core::fmt;

/// Failure modes of the [`crate::curve`] ports.
///
/// The first six variants are named **exactly** after the custom errors declared in
/// `PropCurve.sol` and are raised under exactly the same conditions. If the chain reverts,
/// the port returns the matching variant; if the port returns one of these, the chain reverts.
///
/// The last two are *not* Solidity custom errors. They exist because this port narrows
/// `uint256` to `u128`, and honesty about where that narrowing bites is worth more than a
/// tidy enum. Neither is reachable inside the documented domain
/// ([`crate::curve::MAX_PRICE`] / [`crate::curve::MAX_AMOUNT`] /
/// [`crate::curve::MAX_PRICE_SCALE_EXP`]) with `priceScaleExp >= 8`; see
/// [`crate::curve`] module docs for the proof and the one gap.
///
/// # Why `AmountOutOfDomain` is now a Solidity error
///
/// It used to be folded into [`Self::DomainOverflow`] and documented as a port-only concern:
/// somewhere the mirror was narrower than the chain. That was the wrong resolution. The chain
/// would have *settled* a trade the engine cannot represent, so the engine would treat a size
/// as unquotable while the pool filled it — a silent-loss failure, not a crash.
///
/// `PropCurve` was amended to revert above `type(uint128).max`, closing the gap by narrowing
/// the *chain* to the engine rather than the other way round. Nothing reachable is rejected:
/// a full `uint128` of an 18-decimal token is ~3.4e20 whole units. What is left in
/// `DomainOverflow` is the genuinely port-only case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum CurveError {
    /// `PropCurve.AmountExceedsCapacity()` — `used + amountIn > capacity`.
    AmountExceedsCapacity,
    /// `PropCurve.ZeroCapacity()` — the capacity epoch has no posted size.
    ZeroCapacity,
    /// `PropCurve.ZeroPrice()` — the ask side resolved to an average price of zero.
    ZeroPrice,
    /// `PropCurve.CrossedBook()` — the ladder is crossed or inverted.
    CrossedBook,
    /// `PropCurve.BidBelowMinPrice()` — `minBid < minPrice`.
    BidBelowMinPrice,
    /// `PropCurve.AmountOutOfDomain()` — the computed output exceeds
    /// `PropCurve.MAX_AMOUNT_OUT` (`type(uint128).max`). Both sides reject it.
    AmountOutOfDomain,

    /// Not a custom error. Solidity 0.8 would raise `Panic(0x11)` (arithmetic
    /// overflow/underflow) or `Panic(0x12)` (division by zero) on these inputs — for example
    /// `maxBid < minBid` reaching `amountOutBid` without passing `validateLadder` first.
    ArithmeticPanic,
    /// Not a custom error, and the chain does **not** reject these inputs.
    ///
    /// Raised when `priceScaleExp > MAX_PRICE_SCALE_EXP`. `PropCurve` itself does not check
    /// the exponent — `10 ** 39` computes perfectly well in `uint256` — so on this input the
    /// chain succeeds where the port refuses. That asymmetry is deliberate and safe in one
    /// direction only: refusing to quote can never lose money, whereas quoting something the
    /// chain would price differently can.
    ///
    /// It is also unreachable in practice, because the bound is enforced one level up:
    /// `PropPool.addPair` rejects `priceScaleExp > PropCurve.MAX_PRICE_SCALE_EXP`, so no pair
    /// carrying such an exponent can exist to be quoted.
    DomainOverflow,
}

impl CurveError {
    /// The Solidity error signature this maps to, or `None` for the two non-Solidity
    /// variants. Useful for decoding a revert returned by `eth_call`.
    #[must_use]
    pub const fn solidity_signature(self) -> Option<&'static str> {
        match self {
            Self::AmountExceedsCapacity => Some("AmountExceedsCapacity()"),
            Self::ZeroCapacity => Some("ZeroCapacity()"),
            Self::ZeroPrice => Some("ZeroPrice()"),
            Self::CrossedBook => Some("CrossedBook()"),
            Self::BidBelowMinPrice => Some("BidBelowMinPrice()"),
            Self::AmountOutOfDomain => Some("AmountOutOfDomain()"),
            Self::ArithmeticPanic | Self::DomainOverflow => None,
        }
    }

    /// `true` when the on-chain call would revert with the matching custom error.
    ///
    /// `false` means the divergence is ours, not the chain's: the chain would have
    /// succeeded. Test-vector generation asserts this is never `false` for an emitted
    /// record.
    #[must_use]
    pub const fn is_solidity_revert(self) -> bool {
        self.solidity_signature().is_some()
    }
}

impl fmt::Display for CurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::AmountExceedsCapacity => "amount exceeds remaining capacity",
            Self::ZeroCapacity => "capacity is zero",
            Self::ZeroPrice => "average price resolved to zero",
            Self::CrossedBook => "ladder is crossed or inverted",
            Self::BidBelowMinPrice => "minBid is below the pair's minPrice floor",
            Self::AmountOutOfDomain => "output exceeds PropCurve.MAX_AMOUNT_OUT",
            Self::ArithmeticPanic => "on-chain arithmetic panic (0x11/0x12)",
            Self::DomainOverflow => "priceScaleExp above PropCurve.MAX_PRICE_SCALE_EXP",
        };
        f.write_str(s)
    }
}

impl std::error::Error for CurveError {}

/// Failure modes of the off-chain ladder solver and builder. These have no on-chain
/// counterpart: they mean "the engine will not produce a row", which is always safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum LadderError {
    /// Capacity is zero, so there is no interval to average a price over.
    ZeroCapacity,
    /// A price argument exceeds [`crate::curve::MAX_PRICE`] (`uint56`).
    PriceOutOfRange,
    /// An amount argument exceeds [`crate::curve::MAX_AMOUNT`] (`uint96`).
    AmountOutOfRange,
    /// `target < min_price`: the requested price is already below the floor, so no ladder
    /// with that average exists.
    TargetBelowFloor,
    /// `target > max_price`: the requested price is already above the ceiling.
    TargetAboveCeiling,
    /// `min_price > max_price`, or `min_price` leaves no room for the strict
    /// `maxAsk > minBid` that `validateLadder` demands.
    InfeasibleBounds,
    /// The produced row failed the on-chain validator. Unreachable by construction; if this
    /// ever surfaces, a proof in this crate is wrong and the row must not be sent.
    Rejected(CurveError),
}

impl fmt::Display for LadderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => f.write_str("capacity is zero"),
            Self::PriceOutOfRange => f.write_str("price exceeds uint56"),
            Self::AmountOutOfRange => f.write_str("amount exceeds uint96"),
            Self::TargetBelowFloor => f.write_str("target price is below the floor"),
            Self::TargetAboveCeiling => f.write_str("target price is above the ceiling"),
            Self::InfeasibleBounds => f.write_str("price bounds admit no valid ladder"),
            Self::Rejected(e) => write!(f, "constructed ladder failed on-chain validation: {e}"),
        }
    }
}

impl std::error::Error for LadderError {}

impl From<CurveError> for LadderError {
    fn from(e: CurveError) -> Self {
        Self::Rejected(e)
    }
}

/// Failure modes of the `updateQuote` calldata packing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum PackError {
    /// A price field exceeds [`crate::curve::MAX_PRICE`] (`uint56`) and cannot be packed.
    PriceOutOfRange,
    /// The word has bits set above bit 239, which no encoder of ours produces. Rejecting
    /// these keeps `unpack(pack(x)) == x` and `pack(unpack(w)) == w` both total.
    NonCanonical,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PriceOutOfRange => f.write_str("price exceeds uint56"),
            Self::NonCanonical => f.write_str("packed word has bits set above bit 239"),
        }
    }
}

impl std::error::Error for PackError {}
