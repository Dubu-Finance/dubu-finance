//! EIP-712 encoding for the RFQ leg — an exact mirror of `contracts/src/PmmSettle.sol`.
//!
//! # Why this is in the mirror crate and not in the bot
//!
//! [`crate::curve`] exists because the engine posts a ladder and then believes it is quoting a
//! particular price; if the two implementations disagree, the disagreement is silent. The RFQ
//! leg has the same shape of hazard with a sharper edge. The engine hands out a signature over
//! a digest it computed. If the digest it computed is not the digest `PmmSettle` computes, then
//! either
//!
//!   * `ecrecover` returns some address that is not the maker and every fill reverts
//!     `BadSignature` — a quote the maker believes is live and that no taker can settle, which
//!     shows up as nothing at all in the engine's logs; or
//!   * the digest is the digest of a *different order* than the one the engine meant to sign,
//!     and the maker's inventory settles against terms nobody chose.
//!
//! The second is the one worth building infrastructure against, and it is not hypothetical
//! paranoia: EIP-712 offers no independent canonicalisation. The type string, the struct's field
//! order, and the `encodeData` order are three separate pieces of text that must agree, in two
//! languages, and nothing in either compiler checks that they do. So both sides pin the same
//! [`vectors`] byte for byte and both fail if either moves.
//!
//! # What is deliberately not here
//!
//! **No signer, and no key handling of any kind.** This crate is a pure function library; it
//! computes the digest and stops. Whatever holds the maker's key holds it somewhere else.
//! `dubu-updater` already owns that responsibility for the ladder path and is where a signer
//! belongs if one is added.
//!
//! **No pricing.** The decay, the pro-rata split and the settlement floor are on chain, and
//! `PmmSettle.previewFill` is `pure` precisely so an off-chain caller reads them out of the
//! contract rather than re-deriving them. That is the opposite of the [`crate::curve`]
//! decision, and the difference is real: the curve has to be mirrored because the engine needs
//! to *solve* it (`crate::inverse`), and you cannot invert a function you can only call. Nobody
//! inverts the decay — it is a scalar of the settlement timestamp.
//!
//! # Numeric domain
//!
//! `makerAmount` and `takerAmount` are `uint256` on chain and `u128` here, and that is **not**
//! a narrowing: `PmmSettle.MAX_AMOUNT` rejects anything wider, for the same reason and at the
//! same value as [`crate::curve::MAX_AMOUNT_OUT`]. `chainId` is `uint256` on chain and `u64`
//! here, which *is* a narrowing, and a safe one — the field is an EIP-155 chain id, no real
//! chain has ever come close to `2^64`, and a mirror that refuses to encode an impossible chain
//! cannot lose money.
//!
//! # Example
//!
//! ```
//! use dubu_core::rfq::{Domain, Order};
//!
//! let order = Order {
//!     maker_amount: 3_000_000_000,
//!     taker_amount: 1_000_000_000_000_000_000,
//!     expiry: 1_800_000_060,
//!     ..Order::default()
//! };
//! let domain = Domain::new(91_342, [0x11; 20]);
//!
//! // The digest a wallet would sign, and the digest `PmmSettle.hashOrder` computes.
//! let digest = order.digest(&domain);
//! assert_eq!(digest.len(), 32);
//!
//! // Every signed field is covered: change one and the digest moves.
//! let mut tampered = order;
//! tampered.maker_amount += 1;
//! assert_ne!(tampered.digest(&domain), digest);
//! ```

use sha3::{Digest, Keccak256};

/// The `Order` EIP-712 type string.
///
/// Byte-identical to the literal `PmmSettle.ORDER_TYPEHASH` hashes. EIP-712 §"Definition of
/// encodeType" fixes this spelling completely — no spaces, members in declaration order,
/// `uint`/`int` written out at full width — so there is exactly one correct string and any
/// deviation is a different type, not a formatting choice.
pub const ORDER_TYPE_STRING: &str = concat!(
    "Order(",
    "address maker,",
    "address makerAsset,",
    "address takerAsset,",
    "uint256 makerAmount,",
    "uint256 takerAmount,",
    "uint64 nonce,",
    "uint64 expiry,",
    "uint64 decayStart,",
    "uint32 decayPerSec,",
    "uint32 decayCap,",
    "uint16 minFillBps",
    ")"
);

/// The EIP-712 domain type string. Four members, so the domain carries a `version`.
pub const DOMAIN_TYPE_STRING: &str = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// `PmmSettle`'s EIP-712 domain name.
pub const DOMAIN_NAME: &str = "DuBu PmmSettle";

/// `PmmSettle`'s EIP-712 domain version.
pub const DOMAIN_VERSION: &str = "1";

/// `keccak256(ORDER_TYPE_STRING)`, and the value `PmmSettle.ORDER_TYPEHASH` holds.
///
/// Hard-coded rather than computed at startup because `keccak256` is not a `const fn` and this
/// crate carries no lazy-initialisation dependency. `typehash_matches_the_type_string` asserts
/// the two agree, so the constant cannot drift from the string it claims to be the hash of, and
/// `PmmSettle.t.sol` asserts the same 32 bytes from the other side.
///
/// Written as hex through [`hex32`] rather than as a byte-array literal so it can be diffed by
/// eye against the `0x23e6...` in the Solidity, which is the only form in which a reviewer can
/// actually check it.
pub const ORDER_TYPEHASH: [u8; 32] = hex32("23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56");

/// Number of 32-byte words in an `Order`'s `encodeData`: the type hash plus eleven members.
pub const ORDER_ENCODE_DATA_WORDS: usize = 12;

/// Parse a 64-character hex literal into 32 bytes.
///
/// A `const fn` so a malformed constant is a compile error rather than a test failure.
/// The indexing is bounded by the `assert!` above it and evaluated in `const` context, so a
/// malformed literal fails the build rather than the process. That is the point of the function.
#[allow(clippy::indexing_slicing)]
#[must_use]
pub const fn hex32(s: &str) -> [u8; 32] {
    let bytes = s.as_bytes();
    assert!(bytes.len() == 64, "expected 64 hex characters");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(bytes[2 * i]) * 16 + nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("not a hex digit"),
    }
}

/// `keccak256` over an arbitrary byte string.
///
/// Exposed because the vector tests and any caller assembling a domain by hand need the same
/// primitive, and because a second keccak implementation appearing anywhere in this workspace
/// would defeat the point of the file.
#[must_use]
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The EIP-712 domain a signature is bound to.
///
/// Both fields are load bearing against replay across deployments, and `chain_id` is the one
/// that matters most: `PmmSettle` caches its separator in bytecode and re-derives it when
/// `block.chainid` no longer matches, precisely so a chain fork cannot leave one signature
/// valid on two chains. A mirror that treated the chain id as decoration would produce
/// signatures with exactly that defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Domain {
    /// EIP-155 chain id of the chain `PmmSettle` is deployed on.
    pub chain_id: u64,
    /// The `PmmSettle` address.
    pub verifying_contract: [u8; 20],
}

impl Domain {
    /// Build a domain.
    #[must_use]
    pub const fn new(chain_id: u64, verifying_contract: [u8; 20]) -> Self {
        Self {
            chain_id,
            verifying_contract,
        }
    }

    /// `keccak256(abi.encode(DOMAIN_TYPEHASH, keccak256(name), keccak256(version), chainId,
    /// verifyingContract))` — the value `PmmSettle.DOMAIN_SEPARATOR()` returns.
    #[must_use]
    pub fn separator(&self) -> [u8; 32] {
        let mut buf = [0u8; 32 * 5];
        buf[0..32].copy_from_slice(&keccak256(DOMAIN_TYPE_STRING.as_bytes()));
        buf[32..64].copy_from_slice(&keccak256(DOMAIN_NAME.as_bytes()));
        buf[64..96].copy_from_slice(&keccak256(DOMAIN_VERSION.as_bytes()));
        write_u64(&mut buf[96..128], self.chain_id);
        write_address(&mut buf[128..160], &self.verifying_contract);
        keccak256(&buf)
    }
}

/// One firm quote, mirroring `IPmmSettle.Order` field for field and in declaration order.
///
/// The field order is not a style choice and reordering it is a consensus break: EIP-712 hashes
/// members in the order the type string lists them, so the struct here, [`ORDER_TYPE_STRING`],
/// [`Order::encode_data`], and the Solidity struct are four expressions of one ordering that
/// must be changed together or not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Order {
    /// Signer, and the account both legs settle against.
    pub maker: [u8; 20],
    /// What the maker delivers.
    pub maker_asset: [u8; 20],
    /// What the maker receives.
    pub taker_asset: [u8; 20],
    /// Maker leg at full size — the quoted amount. Bounded by `PmmSettle.MAX_AMOUNT`.
    pub maker_amount: u128,
    /// Taker leg at full size. Bounded by `PmmSettle.MAX_AMOUNT`.
    pub taker_amount: u128,
    /// The maker's cancellation handle. Not the replay guard; the fill accounting is.
    pub nonce: u64,
    /// Last block timestamp at which the order may be filled, inclusive.
    pub expiry: u64,
    /// Timestamp at which the decay begins. Zero disables it.
    pub decay_start: u64,
    /// Decay accrued per second, in ppm of the maker leg. Zero disables it.
    pub decay_per_sec: u32,
    /// Ceiling on the accrued decay, in ppm. Zero disables it; `PmmSettle.MAX_DECAY_CAP` bounds
    /// it above.
    pub decay_cap: u32,
    /// The maker's floor on a single fill, in bps of `maker_amount`, measured after the decay.
    pub min_fill_bps: u16,
}

impl Order {
    /// EIP-712 `encodeData`: the type hash followed by every member, each left-padded to a
    /// 32-byte word.
    ///
    /// Returned as the raw buffer rather than hashed so a test can inspect the padding directly.
    /// Every narrow member — the `u64`s, the `u32`s, the `u16` — occupies a full word with
    /// leading zeros, which is why widening or narrowing a field in the Solidity struct costs
    /// nothing on the wire but changes the *type string*, and therefore the type hash, and
    /// therefore every digest.
    #[must_use]
    pub fn encode_data(&self) -> [u8; 32 * ORDER_ENCODE_DATA_WORDS] {
        let mut buf = [0u8; 32 * ORDER_ENCODE_DATA_WORDS];
        buf[0..32].copy_from_slice(&ORDER_TYPEHASH);
        write_address(&mut buf[32..64], &self.maker);
        write_address(&mut buf[64..96], &self.maker_asset);
        write_address(&mut buf[96..128], &self.taker_asset);
        write_u128(&mut buf[128..160], self.maker_amount);
        write_u128(&mut buf[160..192], self.taker_amount);
        write_u64(&mut buf[192..224], self.nonce);
        write_u64(&mut buf[224..256], self.expiry);
        write_u64(&mut buf[256..288], self.decay_start);
        write_u64(&mut buf[288..320], u64::from(self.decay_per_sec));
        write_u64(&mut buf[320..352], u64::from(self.decay_cap));
        write_u64(&mut buf[352..384], u64::from(self.min_fill_bps));
        buf
    }

    /// `keccak256(encodeData)` — the value `PmmSettle._structHash` computes.
    #[must_use]
    pub fn struct_hash(&self) -> [u8; 32] {
        keccak256(&self.encode_data())
    }

    /// The EIP-712 digest: `keccak256(0x19 || 0x01 || domainSeparator || structHash)`.
    ///
    /// This is what a wallet signs and what `PmmSettle.hashOrder` returns, and it is also the
    /// key the fill accounting is stored under — so two orders differing in any signed field,
    /// the nonce included, have independent remaining amounts.
    #[must_use]
    pub fn digest(&self, domain: &Domain) -> [u8; 32] {
        let mut buf = [0u8; 2 + 32 + 32];
        buf[0] = 0x19;
        buf[1] = 0x01;
        buf[2..34].copy_from_slice(&domain.separator());
        buf[34..66].copy_from_slice(&self.struct_hash());
        keccak256(&buf)
    }
}

/// Left-pad a 20-byte address into a 32-byte word.
///
/// `word` is always exactly 32 bytes at every call site; the slice form keeps `encode_data`
/// readable as a table of offsets, which is how it is checked against the Solidity.
///
/// The three writers here index with constant bounds into a window `encode_data` cut from a
/// fixed-size buffer. Taking `&mut [u8; 32]` instead would make that a type-level fact and remove
/// the lint exemption — but it would also stop `encode_data` reading as a list of offsets, and the
/// offsets are what a reviewer diffs against `PmmSettle`. The exemption is the cheaper of the two,
/// and `encode_data`'s own tests pin every offset.
#[allow(clippy::indexing_slicing)]
fn write_address(word: &mut [u8], address: &[u8; 20]) {
    word[12..32].copy_from_slice(address);
}

/// Left-pad a `u128` into a 32-byte word, big-endian.
#[allow(clippy::indexing_slicing)]
fn write_u128(word: &mut [u8], value: u128) {
    word[16..32].copy_from_slice(&value.to_be_bytes());
}

/// Left-pad a `u64` into a 32-byte word, big-endian.
#[allow(clippy::indexing_slicing)]
fn write_u64(word: &mut [u8], value: u64) {
    word[24..32].copy_from_slice(&value.to_be_bytes());
}

/// Fixed differential vectors, shared verbatim with `contracts/test/PmmSettle.t.sol`.
///
/// Neither side computes these: both assert them. That is the only arrangement under which the
/// vectors are evidence rather than a tautology — a test that derived the expected value from
/// the implementation under test would pass for any implementation.
///
/// The five cases are chosen to move every distinct encoding rule at least once: the all-zero
/// order (which pins the padding, since a bug that dropped a word entirely still produces a
/// plausible-looking hash), a realistic WETH/USDC quote, one at the `u128` domain ceiling, one
/// with every narrow field at its maximum, and one that differs from a sibling in a single
/// `uint16` (which pins that the narrowest field is genuinely covered).
pub mod vectors {
    use super::{hex32, Domain, Order};

    /// One vector: an order, the domain it is bound to, and the two hashes both languages must
    /// agree on.
    #[derive(Debug, Clone, Copy)]
    pub struct Vector {
        /// Label, used in failure messages on both sides.
        pub name: &'static str,
        /// The order.
        pub order: Order,
        /// The domain.
        pub domain: Domain,
        /// Expected `keccak256(encodeData)`.
        pub struct_hash: [u8; 32],
        /// Expected `keccak256(0x19 || 0x01 || separator || structHash)`.
        pub digest: [u8; 32],
    }

    const fn addr(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    /// Every vector, in the order `PmmSettle.t.sol` indexes them.
    #[must_use]
    pub fn all() -> [Vector; 5] {
        [
            Vector {
                name: "zero",
                order: Order {
                    maker: [0; 20],
                    maker_asset: [0; 20],
                    taker_asset: [0; 20],
                    maker_amount: 0,
                    taker_amount: 0,
                    nonce: 0,
                    expiry: 0,
                    decay_start: 0,
                    decay_per_sec: 0,
                    decay_cap: 0,
                    min_fill_bps: 0,
                },
                domain: Domain::new(0, [0; 20]),
                struct_hash: hex32("e4f4bdaa8c4f12a4f520a975132dc664c5289e80fab437a5ba98fd51dc2c7b29"),
                digest: hex32("bcc0c92d5f4d665e0ad5acda83ccc00e485acfac544532beed93b3df0689d84f"),
            },
            Vector {
                name: "weth_usdc",
                order: Order {
                    maker: addr(0xa1),
                    maker_asset: addr(0xb2),
                    taker_asset: addr(0xc3),
                    maker_amount: 3_000_000_000,
                    taker_amount: 1_000_000_000_000_000_000,
                    nonce: 7,
                    expiry: 1_800_000_060,
                    decay_start: 1_800_000_000,
                    decay_per_sec: 250,
                    decay_cap: 50_000,
                    min_fill_bps: 0,
                },
                domain: Domain::new(91_342, addr(0x11)),
                struct_hash: hex32("9dc8b4546d5a28923f50a4374402e8b0774db53704d9d668aa1be1a75a30259e"),
                digest: hex32("fd8b2f15795e0412c2494fb162b8e8c84ae531843730ad9fd27e78dc6b5006dc"),
            },
            Vector {
                name: "domain_ceiling",
                order: Order {
                    maker: addr(0xff),
                    maker_asset: addr(0x01),
                    taker_asset: addr(0x02),
                    maker_amount: u128::MAX,
                    taker_amount: u128::MAX,
                    nonce: 0,
                    expiry: u64::MAX,
                    decay_start: 0,
                    decay_per_sec: 0,
                    decay_cap: 0,
                    min_fill_bps: 10_000,
                },
                domain: Domain::new(1, addr(0xde)),
                struct_hash: hex32("e5bafa60feed25b42a0e0ca9ef1230af79d5ecdafed4eb60cc93953256305b7b"),
                digest: hex32("d076d897e2cef09edf1c915695a37b4ccb28d6ef230cb3e44c797cecda588415"),
            },
            Vector {
                name: "narrow_fields_max",
                order: Order {
                    maker: addr(0x0a),
                    maker_asset: addr(0x0b),
                    taker_asset: addr(0x0c),
                    maker_amount: 1,
                    taker_amount: 1,
                    nonce: u64::MAX,
                    expiry: u64::MAX,
                    decay_start: u64::MAX,
                    decay_per_sec: u32::MAX,
                    decay_cap: u32::MAX,
                    min_fill_bps: u16::MAX,
                },
                domain: Domain::new(u64::MAX, addr(0xee)),
                struct_hash: hex32("a754907b3a90f7c66922923dc858007d4e2235aaf44c56bec298937ad69134ac"),
                digest: hex32("f4a14b260933d10c42a3e7f80909e898e396b8e92accc08c1c72466797bb581e"),
            },
            Vector {
                // Identical to `weth_usdc` except `min_fill_bps`. If the narrowest member were
                // dropped from `encodeData`, this vector and that one would collide.
                name: "weth_usdc_min_fill",
                order: Order {
                    maker: addr(0xa1),
                    maker_asset: addr(0xb2),
                    taker_asset: addr(0xc3),
                    maker_amount: 3_000_000_000,
                    taker_amount: 1_000_000_000_000_000_000,
                    nonce: 7,
                    expiry: 1_800_000_060,
                    decay_start: 1_800_000_000,
                    decay_per_sec: 250,
                    decay_cap: 50_000,
                    min_fill_bps: 1,
                },
                domain: Domain::new(91_342, addr(0x11)),
                struct_hash: hex32("83fe65ecbd2f178813110780f7fea08b239bc0192296d6afa13218dd060bf5a7"),
                digest: hex32("e332ed6b3fb00d5917406e37e578c2f22363b216ab21f4389ae463aeebbc2e64"),
            },
        ]
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn keccak_matches_published_vectors() {
        // The two canonical Keccak-256 (not SHA3-256) test values. If this fails, nothing else
        // in this file means anything, so it is checked first and separately.
        assert_eq!(
            hex(&keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        assert_eq!(
            hex(&keccak256(b"abc")),
            "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
        );
    }

    #[test]
    fn typehash_matches_the_type_string() {
        assert_eq!(ORDER_TYPEHASH, keccak256(ORDER_TYPE_STRING.as_bytes()));
    }

    #[test]
    fn type_string_lists_members_in_struct_order() {
        // A cheap guard against the one edit that silently breaks everything: reordering the
        // Rust struct's fields without reordering the type string. Field names are checked to
        // appear in ascending position, which is exactly the property EIP-712 depends on.
        let names = [
            "address maker,",
            "address makerAsset,",
            "address takerAsset,",
            "uint256 makerAmount,",
            "uint256 takerAmount,",
            "uint64 nonce,",
            "uint64 expiry,",
            "uint64 decayStart,",
            "uint32 decayPerSec,",
            "uint32 decayCap,",
            "uint16 minFillBps)",
        ];
        let mut cursor = 0;
        for name in names {
            let at = ORDER_TYPE_STRING[cursor..]
                .find(name)
                .unwrap_or_else(|| panic!("`{name}` missing or out of order in ORDER_TYPE_STRING"));
            cursor += at + name.len();
        }
        assert_eq!(cursor, ORDER_TYPE_STRING.len());
    }

    #[test]
    fn encode_data_is_word_aligned_and_left_padded() {
        let order = Order {
            maker: [0xaa; 20],
            maker_asset: [0xbb; 20],
            taker_asset: [0xcc; 20],
            maker_amount: u128::MAX,
            taker_amount: 1,
            nonce: u64::MAX,
            expiry: 2,
            decay_start: 3,
            decay_per_sec: u32::MAX,
            decay_cap: 4,
            min_fill_bps: u16::MAX,
        };
        let buf = order.encode_data();
        assert_eq!(buf.len(), 32 * ORDER_ENCODE_DATA_WORDS);

        // Addresses occupy the low 20 bytes and the high 12 are zero.
        for word in 1..=3 {
            assert_eq!(&buf[word * 32..word * 32 + 12], &[0u8; 12]);
        }
        // `uint256` members at full width still leave nothing above 16 bytes, because the domain
        // is `u128`.
        assert_eq!(&buf[128..144], &[0u8; 16]);
        // Every narrow member is left-padded to 24 zero bytes.
        for word in 6..=11 {
            assert_eq!(&buf[word * 32..word * 32 + 24], &[0u8; 24]);
        }
    }

    #[test]
    fn fixed_vectors_are_stable() {
        for v in vectors::all() {
            assert_eq!(v.order.struct_hash(), v.struct_hash, "{} struct hash", v.name);
            assert_eq!(v.order.digest(&v.domain), v.digest, "{} digest", v.name);
        }
    }

    #[test]
    fn vectors_that_differ_only_in_min_fill_bps_do_not_collide() {
        let all = vectors::all();
        let a = all.iter().find(|v| v.name == "weth_usdc").unwrap();
        let b = all.iter().find(|v| v.name == "weth_usdc_min_fill").unwrap();
        assert_eq!(a.order.min_fill_bps + 1, b.order.min_fill_bps);
        assert_ne!(a.digest, b.digest);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        // Any two orders differing in any single field must produce different digests. This is
        // the property that "every signed field is covered" reduces to, and it is the one a
        // dropped or duplicated `copy_from_slice` in `encode_data` would break.
        #[test]
        fn every_field_is_covered(
            maker in prop::array::uniform20(any::<u8>()),
            maker_asset in prop::array::uniform20(any::<u8>()),
            taker_asset in prop::array::uniform20(any::<u8>()),
            maker_amount in any::<u128>(),
            taker_amount in any::<u128>(),
            nonce in any::<u64>(),
            expiry in any::<u64>(),
            decay_start in any::<u64>(),
            decay_per_sec in any::<u32>(),
            decay_cap in any::<u32>(),
            min_fill_bps in any::<u16>(),
            chain_id in any::<u64>(),
            verifying in prop::array::uniform20(any::<u8>()),
            which in 0usize..13,
        ) {
            let base = Order {
                maker, maker_asset, taker_asset, maker_amount, taker_amount,
                nonce, expiry, decay_start, decay_per_sec, decay_cap, min_fill_bps,
            };
            let domain = Domain::new(chain_id, verifying);

            let mut other = base;
            let mut other_domain = domain;
            match which {
                0 => other.maker[19] ^= 1,
                1 => other.maker_asset[0] ^= 1,
                2 => other.taker_asset[19] ^= 1,
                3 => other.maker_amount ^= 1,
                4 => other.taker_amount ^= 1,
                5 => other.nonce ^= 1,
                6 => other.expiry ^= 1,
                7 => other.decay_start ^= 1,
                8 => other.decay_per_sec ^= 1,
                9 => other.decay_cap ^= 1,
                10 => other.min_fill_bps ^= 1,
                11 => other_domain.chain_id ^= 1,
                _ => other_domain.verifying_contract[0] ^= 1,
            }

            prop_assert_ne!(base.digest(&domain), other.digest(&other_domain));
        }

        // The digest is a function of the order and the domain and of nothing else — no
        // interior mutability, no ambient state, no dependence on how the struct was built.
        #[test]
        fn digest_is_deterministic(
            maker_amount in any::<u128>(),
            taker_amount in any::<u128>(),
            chain_id in any::<u64>(),
        ) {
            let order = Order { maker_amount, taker_amount, ..Order::default() };
            let domain = Domain::new(chain_id, [0x42; 20]);
            prop_assert_eq!(order.digest(&domain), order.digest(&domain));
        }

        // `encode_data` never loses a high byte: the buffer contains each amount's full
        // big-endian representation at the offset the table claims.
        #[test]
        fn amounts_round_trip_out_of_encode_data(
            maker_amount in any::<u128>(),
            taker_amount in any::<u128>(),
        ) {
            let order = Order { maker_amount, taker_amount, ..Order::default() };
            let buf = order.encode_data();
            let mut m = [0u8; 16];
            let mut t = [0u8; 16];
            m.copy_from_slice(&buf[144..160]);
            t.copy_from_slice(&buf[176..192]);
            prop_assert_eq!(u128::from_be_bytes(m), maker_amount);
            prop_assert_eq!(u128::from_be_bytes(t), taker_amount);
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
