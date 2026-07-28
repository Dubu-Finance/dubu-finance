//! Turning a priced [`crate::quoting::Quote`] into a signed order a taker can settle.
//!
//! [`crate::quoting`] decides the price and reserves the inventory. This puts the maker's name on
//! it. The split is not cosmetic: everything up to here is arithmetic that can be tested without a
//! key, and everything here is a signature that authorises somebody to pull tokens out of the
//! maker's balance.
//!
//! # The key is not the updater's key
//!
//! `tx::Intent` opens with "the only two things the updater key is allowed to do", and this would
//! be a third. It is deliberately not given to that key, because the two authorities are not
//! comparable:
//!
//! - A leaked updater key can push a wrong ladder. That costs whatever the pool is quoting until
//!   somebody notices, and the killswitches are watching.
//! - A leaked RFQ key can sign an order that transfers the maker's balance to an address of the
//!   attacker's choosing, up to the standing allowance, immediately and once per nonce. There is
//!   nothing to notice in time.
//!
//! So [`MakerKey`] is loaded from its own [`crate::config::KeySource`], and RFQ is **off** unless
//! that source is configured. Off means the aggregator routes AMM-only, which is a worse quote and
//! a working system. `DeployRfq` already anticipated this by taking `RFQ_MAKER` separately from
//! the deployer.
//!
//! # The digest is computed here and nowhere else
//!
//! [`dubu_core::rfq`] owns the EIP-712 encoding, and it is the same code the settlement contract's
//! tests pin from the other side. This module does not re-derive a domain separator or re-encode a
//! struct; it fills in an [`Order`] and asks for the digest. A maker whose digest disagrees with
//! the chain's by one byte produces quotes nobody can fill and sees no error of its own, which is
//! exactly why there is one implementation rather than a convenient local copy.

use alloy_primitives::{Address, B256};
use dubu_core::rfq::{Domain, Order};

use crate::config::KeySource;
use crate::quoting::Quote;
use crate::tx::{Signer, TxError};

/// A signed order, ready to hand to a taker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedOrder {
    /// The order, field for field as `PmmSettle.Order`.
    pub order: Order,
    /// 65 bytes of `(r, s, v)`, with `v` in `{27, 28}`.
    pub signature: [u8; 65],
    /// The EIP-712 digest. Also the key `PmmSettle` accounts fills against, so it is what a log
    /// line or an indexer joins on.
    pub digest: B256,
}

/// The RFQ signing key and the domain it signs for.
///
/// Holds the domain rather than taking it per call so that a misconfigured chain id or settlement
/// address is a startup error, not a stream of unfillable quotes.
#[derive(Debug)]
pub struct MakerKey {
    signer: Signer,
    domain: Domain,
}

impl MakerKey {
    /// Loads the key and pins the domain.
    ///
    /// # Errors
    ///
    /// [`TxError::Key`] if the key cannot be read or is not a valid secp256k1 scalar. The message
    /// never quotes the key material.
    pub fn load(source: &KeySource, chain_id: u64, pmm_settle: Address) -> Result<Self, TxError> {
        Ok(Self::new(Signer::load(source)?, chain_id, pmm_settle))
    }

    /// Pins a domain onto an already-loaded key.
    #[must_use]
    pub fn new(signer: Signer, chain_id: u64, pmm_settle: Address) -> Self {
        Self {
            signer,
            domain: Domain::new(chain_id, pmm_settle.into()),
        }
    }

    /// The address every order this key signs will name as `maker`.
    ///
    /// The taker checks that the recovered signer is the maker it expected, so this is the value
    /// that has to match what `PmmSettle` was approved by and what the aggregator was configured
    /// with. Exposed so startup can log it and a misconfiguration is visible before a quote is.
    #[must_use]
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// The `PmmSettle.DOMAIN_SEPARATOR()` this key signs against.
    ///
    /// Worth logging at startup and comparing against the deployed contract: a maker whose domain
    /// disagrees with the chain's signs quotes that recover to somebody else entirely, and the
    /// symptom is silence rather than an error.
    #[must_use]
    pub fn domain_separator(&self) -> B256 {
        B256::from(self.domain.separator())
    }

    /// Signs a priced quote.
    ///
    /// `nonce` comes from [`crate::quoting::Book::next_nonce`] — the cancellation handle, not a
    /// replay guard.
    ///
    /// # Errors
    ///
    /// [`TxError::Sign`] if the curve rejects the digest, which cannot happen for a well-formed
    /// 32-byte hash and is propagated rather than unwrapped anyway.
    pub fn sign(&self, quote: &Quote, nonce: u64) -> Result<SignedOrder, TxError> {
        let order = Order {
            maker: self.signer.address().into(),
            maker_asset: quote.maker_asset.into(),
            taker_asset: quote.taker_asset.into(),
            maker_amount: quote.maker_amount,
            taker_amount: quote.taker_amount,
            nonce,
            expiry: quote.expiry,
            // Decay is off. It exists so a streamed quote can widen as it ages instead of being
            // cancelled and re-signed; this maker re-signs on request, so a decaying order would
            // be a second, slower price control fighting the first. Turning it on is a change to
            // how quotes are distributed, not a parameter to tune.
            decay_start: 0,
            decay_per_sec: 0,
            decay_cap: 0,
            min_fill_bps: quote.min_fill_bps,
        };

        let digest = B256::from(order.digest(&self.domain));
        let signature = self.signer.sign_digest_65(digest)?;

        Ok(SignedOrder {
            order,
            signature,
            digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quoting::{Book, MakerParams, MarketState};
    use alloy_primitives::address;

    /// A throwaway key. Signatures below are real, so "the signature verifies" is tested rather
    /// than asserted.
    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const PMM_SETTLE: Address = address!("68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E");
    const CHAIN_ID: u64 = 91342;
    const ONE_ETH: u128 = 1_000_000_000_000_000_000;
    /// Roughly one base token's worth of quote at the fair value below. The taker's leg is
    /// denominated in whatever it hands over, so the two directions take different numbers.
    const ONE_ETH_IN_USDC: u128 = 2_000_000_000;

    /// What a taker hands over for a one-base-token trade, in the direction's own units.
    const fn taker_leg(taker_buys_base: bool) -> u128 {
        if taker_buys_base {
            ONE_ETH_IN_USDC
        } else {
            ONE_ETH
        }
    }

    fn key() -> MakerKey {
        MakerKey::new(
            Signer::from_hex(KEY).expect("valid key"),
            CHAIN_ID,
            PMM_SETTLE,
        )
    }

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

    fn state() -> MarketState {
        MarketState {
            pair_id: 1,
            base: address!("81e46C6379498beBEB5DCcD47ab2DdFaf967d445"),
            quote: address!("d28596C6750D87C53EA146134AfAB53de86C5155"),
            fair: 2_000_000_000_000_000,
            price_scale_exp: 24,
            sigma_millibps: 0,
            base_balance: 1_000 * ONE_ETH,
            quote_balance: 10_000_000_000_000,
            epoch_ask_base: 0,
            epoch_bid_base: 0,
        }
    }

    fn signed(taker_buys_base: bool) -> SignedOrder {
        let mut book = Book::new();
        let q = book
            .quote(
                &params(),
                &state(),
                taker_buys_base,
                taker_leg(taker_buys_base),
                1_000,
            )
            .expect("quotable");
        key().sign(&q, book.next_nonce()).expect("signable")
    }

    /// The value `PmmSettle.DOMAIN_SEPARATOR()` returns on GIWA Sepolia, read off the deployed
    /// contract and independently derived by `DeployRfq` before it broadcast.
    ///
    /// This is the check that catches a maker signing into the void: a domain that disagrees with
    /// the chain's by one byte produces signatures that recover to somebody else, and the maker
    /// sees no error of its own — just quotes nobody fills.
    #[test]
    fn the_domain_matches_the_deployed_settlement_contract() {
        assert_eq!(
            format!("{:#x}", key().domain_separator()),
            "0x785df92cfa961225995c562e9a42c1b5645097a5bd5b868c303785afb34c5ee7"
        );
    }

    /// `PmmSettle._verifySignature` rejects anything else outright.
    #[test]
    fn the_signature_is_65_bytes_with_a_recovery_id_the_evm_accepts() {
        let s = signed(true);
        assert_eq!(s.signature.len(), 65);
        assert!(
            s.signature[64] == 27 || s.signature[64] == 28,
            "v = {}",
            s.signature[64]
        );
    }

    /// `MalleableSignature` is rejected on chain. k256 normalises to low-S, and this asserts that
    /// rather than trusting it.
    #[test]
    fn the_signature_is_low_s() {
        let s = signed(true);
        // secp256k1 n/2.
        const HALF_N: [u8; 32] = [
            0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46,
            0x68, 0x1b, 0x20, 0xa0,
        ];
        assert!(
            s.signature[32..64] <= HALF_N[..],
            "s must be in the lower half of the curve order"
        );
    }

    /// The whole point: what the taker recovers has to be the maker the order names.
    #[test]
    fn the_signature_recovers_to_the_maker_the_order_names() {
        let s = signed(true);
        let recovered = recover(&s.digest, &s.signature).expect("recoverable");
        assert_eq!(recovered, key().address());
        assert_eq!(Address::from(s.order.maker), key().address());
    }

    #[test]
    fn both_directions_recover_to_the_same_maker() {
        for buys in [true, false] {
            let s = signed(buys);
            assert_eq!(
                recover(&s.digest, &s.signature).expect("recoverable"),
                key().address()
            );
        }
    }

    /// A digest is over the whole order, so any field moving must move the digest. Testing one
    /// field would leave the others free to be dropped from `encode_data` unnoticed.
    #[test]
    fn every_field_changes_the_digest() {
        let mut book = Book::new();
        let q = book
            .quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000)
            .expect("quotable");
        let base = key().sign(&q, 0).expect("signable");

        let mut bigger = q;
        bigger.maker_amount += 1;
        assert_ne!(
            key().sign(&bigger, 0).expect("signable").digest,
            base.digest
        );

        let mut later = q;
        later.expiry += 1;
        assert_ne!(key().sign(&later, 0).expect("signable").digest, base.digest);

        let mut fussier = q;
        fussier.min_fill_bps += 1;
        assert_ne!(
            key().sign(&fussier, 0).expect("signable").digest,
            base.digest
        );

        assert_ne!(
            key().sign(&q, 1).expect("signable").digest,
            base.digest,
            "the nonce is in the digest"
        );
    }

    /// The nonce is the cancellation handle, so two orders from one book must not share one.
    #[test]
    fn consecutive_quotes_get_distinct_digests() {
        let mut book = Book::new();
        let k = key();
        let a = book
            .quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000)
            .expect("quotable");
        let sa = k.sign(&a, book.next_nonce()).expect("signable");
        let b = book
            .quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000)
            .expect("quotable");
        let sb = k.sign(&b, book.next_nonce()).expect("signable");
        assert_ne!(sa.digest, sb.digest);
        assert_ne!(sa.order.nonce, sb.order.nonce);
    }

    /// Decay is off, and the three fields that switch it on stay zero. A non-zero decay would let
    /// a taker fill at a worse price than the one that was quoted.
    #[test]
    fn decay_is_off() {
        let s = signed(true);
        assert_eq!(
            (
                s.order.decay_start,
                s.order.decay_per_sec,
                s.order.decay_cap
            ),
            (0, 0, 0)
        );
    }

    /// A different settlement contract is a different domain, so the same order signs differently.
    /// This is what stops an order for one deployment being replayed against another.
    #[test]
    fn a_different_settlement_contract_yields_a_different_digest() {
        let other = MakerKey::new(
            Signer::from_hex(KEY).expect("valid key"),
            CHAIN_ID,
            address!("00000000000000000000000000000000000000BD"),
        );
        let mut book = Book::new();
        let q = book
            .quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000)
            .expect("quotable");
        assert_ne!(
            other.sign(&q, 0).expect("signable").digest,
            key().sign(&q, 0).expect("signable").digest
        );
    }

    #[test]
    fn a_different_chain_yields_a_different_digest() {
        let other = MakerKey::new(Signer::from_hex(KEY).expect("valid key"), 1, PMM_SETTLE);
        let mut book = Book::new();
        let q = book
            .quote(&params(), &state(), true, ONE_ETH_IN_USDC, 1_000)
            .expect("quotable");
        assert_ne!(
            other.sign(&q, 0).expect("signable").digest,
            key().sign(&q, 0).expect("signable").digest
        );
    }

    /// `ecrecover`, as the EVM does it, so the assertions above are testing the same thing the
    /// chain will.
    fn recover(digest: &B256, sig: &[u8; 65]) -> Option<Address> {
        use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
        let signature = Signature::from_slice(&sig[..64]).ok()?;
        let rec = RecoveryId::from_byte(sig[64].checked_sub(27)?)?;
        let vk = VerifyingKey::recover_from_prehash(digest.as_slice(), &signature, rec).ok()?;
        let point = vk.to_encoded_point(false);
        let hash = dubu_core::rfq::keccak256(&point.as_bytes()[1..]);
        Some(Address::from_slice(&hash[12..]))
    }
}
