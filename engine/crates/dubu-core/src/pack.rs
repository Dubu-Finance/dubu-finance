//! `updateQuote` calldata word packing.
//!
//! One EVM word carries an entire quote row:
//!
//! ```text
//! bit   0.. 55   minBid    (uint56)
//! bit  56..111   maxBid    (uint56)
//! bit 112..167   minAsk    (uint56)
//! bit 168..223   maxAsk    (uint56)
//! bit 224..239   pairId    (uint16)
//! bit 240..255   unused, must be zero
//! ```
//!
//! 240 of 256 bits, so the word does not fit `u128` and is held as two limbs plus a
//! byte-exact `[u8; 32]` in EVM big-endian order. `minAsk` straddles the limb boundary
//! (16 bits low, 40 bits high); everything else sits inside one limb.
//!
//! On an L2 the calldata byte, not the opcode, is the cost (archi_v2 §4.3: 16 gas per
//! non-zero byte against ~5 gas for a `mul`), so packing four prices and a pair id into one
//! word is worth the shifts on both sides.
//!
//! Unpacking rejects a word with bits set above 239 rather than masking them away. Silently
//! discarding bits would make `pack(unpack(w)) != w` and would hide an encoder bug in the
//! one place it matters.

use crate::curve::{Ladder, MAX_PRICE};
use crate::error::PackError;

/// Bit offset of each field.
const OFF_MIN_BID: u32 = 0;
const OFF_MAX_BID: u32 = 56;
const OFF_MIN_ASK: u32 = 112;
const OFF_MAX_ASK: u32 = 168;
const OFF_PAIR_ID: u32 = 224;
/// First bit that must be zero.
const OFF_UNUSED: u32 = 240;

const M56: u128 = (1u128 << 56) - 1;
const M16: u128 = (1u128 << 16) - 1;
/// Bits of `minAsk` that live in the low limb (112..127).
const MIN_ASK_LOW_BITS: u32 = 128 - OFF_MIN_ASK;
const M_MIN_ASK_LOW: u128 = (1u128 << MIN_ASK_LOW_BITS) - 1;
/// Bits of `minAsk` that live in the high limb (128..167).
const M_MIN_ASK_HIGH: u128 = (1u128 << (56 - MIN_ASK_LOW_BITS)) - 1;

/// One `updateQuote` row, unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct QuoteWord {
    /// Pair index, `uint16`.
    pub pair_id: u16,
    /// The four prices.
    pub ladder: Ladder,
}

impl QuoteWord {
    /// Build from a pair id and a ladder.
    #[must_use]
    pub const fn new(pair_id: u16, ladder: Ladder) -> Self {
        Self { pair_id, ladder }
    }

    /// Pack into `(lo, hi)` limbs, where the word is `hi * 2^128 + lo`.
    ///
    /// # Errors
    /// [`PackError::PriceOutOfRange`] if any price exceeds [`MAX_PRICE`].
    pub fn pack_limbs(&self) -> Result<(u128, u128), PackError> {
        let l = &self.ladder;
        for p in [l.min_bid, l.max_bid, l.min_ask, l.max_ask] {
            if p > MAX_PRICE {
                return Err(PackError::PriceOutOfRange);
            }
        }
        let lo = (l.min_bid << OFF_MIN_BID)
            | (l.max_bid << OFF_MAX_BID)
            | ((l.min_ask & M_MIN_ASK_LOW) << OFF_MIN_ASK);
        let hi = (l.min_ask >> MIN_ASK_LOW_BITS)
            | (l.max_ask << (OFF_MAX_ASK - 128))
            | (u128::from(self.pair_id) << (OFF_PAIR_ID - 128));
        Ok((lo, hi))
    }

    /// Pack into a big-endian 32-byte EVM word, ready to concatenate into calldata.
    ///
    /// # Errors
    /// [`PackError::PriceOutOfRange`] if any price exceeds [`MAX_PRICE`].
    pub fn pack(&self) -> Result<[u8; 32], PackError> {
        let (lo, hi) = self.pack_limbs()?;
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(&hi.to_be_bytes());
        out[16..].copy_from_slice(&lo.to_be_bytes());
        Ok(out)
    }

    /// Recover a row from `(lo, hi)` limbs.
    ///
    /// # Errors
    /// [`PackError::NonCanonical`] if any bit at or above 240 is set.
    pub fn unpack_limbs(lo: u128, hi: u128) -> Result<Self, PackError> {
        if (hi >> (OFF_UNUSED - 128)) != 0 {
            return Err(PackError::NonCanonical);
        }
        let min_ask = ((lo >> OFF_MIN_ASK) & M_MIN_ASK_LOW) | ((hi & M_MIN_ASK_HIGH) << MIN_ASK_LOW_BITS);
        let ladder = Ladder {
            min_bid: (lo >> OFF_MIN_BID) & M56,
            max_bid: (lo >> OFF_MAX_BID) & M56,
            min_ask,
            max_ask: (hi >> (OFF_MAX_ASK - 128)) & M56,
        };
        let pair_id = ((hi >> (OFF_PAIR_ID - 128)) & M16) as u16;
        Ok(Self { pair_id, ladder })
    }

    /// Recover a row from a big-endian 32-byte EVM word.
    ///
    /// # Errors
    /// [`PackError::NonCanonical`] if any bit at or above 240 is set.
    pub fn unpack(word: &[u8; 32]) -> Result<Self, PackError> {
        let mut hi_b = [0u8; 16];
        let mut lo_b = [0u8; 16];
        hi_b.copy_from_slice(&word[..16]);
        lo_b.copy_from_slice(&word[16..]);
        Self::unpack_limbs(u128::from_be_bytes(lo_b), u128::from_be_bytes(hi_b))
    }

    /// `0x`-prefixed hex of the packed word.
    ///
    /// # Errors
    /// [`PackError::PriceOutOfRange`] if any price exceeds [`MAX_PRICE`].
    pub fn to_hex(&self) -> Result<String, PackError> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let w = self.pack()?;
        let mut s = String::with_capacity(66);
        s.push_str("0x");
        for b in w {
            // `b >> 4` and `b & 0x0f` are both nibbles, so both are in `0..16`, which is the
            // length of `HEX`. Reaching for `get()` here would mean inventing a fallback character
            // for a case the type system already rules out.
            #[allow(clippy::indexing_slicing)]
            s.push(char::from(HEX[usize::from(b >> 4)]));
            #[allow(clippy::indexing_slicing)]
            s.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn word(min_bid: u128, max_bid: u128, min_ask: u128, max_ask: u128, pair_id: u16) -> QuoteWord {
        QuoteWord::new(pair_id, Ladder { min_bid, max_bid, min_ask, max_ask })
    }

    #[test]
    fn field_offsets_are_where_the_spec_says() {
        // One field set at a time; the packed word must be exactly that field shifted.
        let cases: [(QuoteWord, u32); 5] = [
            (word(1, 0, 0, 0, 0), OFF_MIN_BID),
            (word(0, 1, 0, 0, 0), OFF_MAX_BID),
            (word(0, 0, 1, 0, 0), OFF_MIN_ASK),
            (word(0, 0, 0, 1, 0), OFF_MAX_ASK),
            (word(0, 0, 0, 0, 1), OFF_PAIR_ID),
        ];
        for (w, off) in cases {
            let (lo, hi) = w.pack_limbs().unwrap();
            let bit = if off < 128 { lo >> off } else { hi >> (off - 128) };
            assert_eq!(bit, 1, "field at offset {off} is misplaced");
            let other = if off < 128 { hi } else { lo };
            assert_eq!(other, 0, "field at offset {off} leaked into the other limb");
        }
    }

    #[test]
    fn min_ask_straddles_the_limb_boundary() {
        let w = word(0, 0, MAX_PRICE, 0, 0);
        let (lo, hi) = w.pack_limbs().unwrap();
        assert_eq!(lo, M_MIN_ASK_LOW << OFF_MIN_ASK);
        assert_eq!(hi, M_MIN_ASK_HIGH);
        assert_eq!(QuoteWord::unpack_limbs(lo, hi).unwrap(), w);
    }

    #[test]
    fn full_word_at_the_domain_corner() {
        let w = word(MAX_PRICE, MAX_PRICE, MAX_PRICE, MAX_PRICE, u16::MAX);
        let bytes = w.pack().unwrap();
        assert_eq!(QuoteWord::unpack(&bytes).unwrap(), w);
        // 240 bits set, top 16 clear.
        assert_eq!(bytes[0], 0x00);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], 0xff);
        assert_eq!(bytes[31], 0xff);
    }

    #[test]
    fn out_of_range_price_is_refused() {
        assert_eq!(word(MAX_PRICE + 1, 0, 0, 0, 0).pack(), Err(PackError::PriceOutOfRange));
        assert_eq!(word(0, 0, 0, MAX_PRICE + 1, 0).pack(), Err(PackError::PriceOutOfRange));
    }

    #[test]
    fn dirty_high_bits_are_refused() {
        assert_eq!(QuoteWord::unpack_limbs(0, 1 << (OFF_UNUSED - 128)), Err(PackError::NonCanonical));
        let mut bytes = [0u8; 32];
        bytes[1] = 1;
        assert_eq!(QuoteWord::unpack(&bytes), Err(PackError::NonCanonical));
    }

    #[test]
    fn hex_is_a_full_evm_word() {
        let w = word(1, 2, 3, 4, 5);
        let h = w.to_hex().unwrap();
        assert_eq!(h.len(), 66);
        assert!(h.starts_with("0x"));
    }

    proptest! {
        #[test]
        fn round_trip(
            min_bid in 0u128..=MAX_PRICE,
            max_bid in 0u128..=MAX_PRICE,
            min_ask in 0u128..=MAX_PRICE,
            max_ask in 0u128..=MAX_PRICE,
            pair_id: u16,
        ) {
            let w = word(min_bid, max_bid, min_ask, max_ask, pair_id);
            prop_assert_eq!(QuoteWord::unpack(&w.pack().unwrap()).unwrap(), w);
        }

        /// The other direction: any canonical word survives a decode/encode cycle byte for
        /// byte, which is what proves no field overlaps another.
        #[test]
        fn round_trip_from_the_word_side(lo: u128, hi in 0u128..(1u128 << 112)) {
            let w = QuoteWord::unpack_limbs(lo, hi).unwrap();
            prop_assert_eq!(w.pack_limbs().unwrap(), (lo, hi));
        }
    }
}
