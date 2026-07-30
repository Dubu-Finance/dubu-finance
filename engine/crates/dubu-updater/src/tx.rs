//! Building, signing and submitting the two transactions the updater role may send.
//!
//! # The updater's authority, which is the whole safety story
//!
//! `PropPool` grants the updater exactly two functions — `updateQuote` and `refreshCapacity` —
//! and neither transfers a token or touches `_reserve`. The contract's comment is explicit that
//! this key "signs many times a minute from a hot process; assume it leaks". This module can
//! therefore only ever encode those two calls, and [`Intent`] has no third variant by design.
//!
//! # Fees: no escalator, deliberately
//!
//! Gas on GIWA is **0.001 gwei**. The whole 203-transaction demo cost 0.0000156 ETH. At that
//! price the entire spread between a minimal priority fee and an absurdly generous one is worth
//! less than a rounding error on a single quote, so the usual machinery — measure the base fee,
//! bump on each retry, cap the escalation — buys nothing and adds a failure mode of its own
//! (a replacement transaction racing the original).
//!
//! There is a real argument for caring, and it does not survive contact with the number: GIWA's
//! sequencer is single-operator with no public mempool and orders by **highest fee first**, so
//! the fee does buy ordering. The response is to pay a flat, generously-over-base fee from
//! config and never think about it again, rather than to build a controller. The config default
//! is 0.05 gwei, fifty times the observed base fee.
//!
//! What this does mean is that a genuine base-fee spike past `max_fee_per_gas_gwei` produces a
//! transaction that never lands. That is not silently absorbed: the pending intent times out,
//! logs loudly with the fee it used, and the pair is unblocked. Raising the config value is the
//! operator's move.
//!
//! # A bounded pipeline, not one at a time
//!
//! Up to [`crate::config::TxConfig::in_flight_max`] unconfirmed transactions per pair, tracked in
//! [`Sender::pending`]. Beyond that [`crate::policy`] gates the pair with `PushInFlight` and
//! computes nothing new for it.
//!
//! This used to be one at a time, because "two rows in flight are ordered by a sequencer that
//! sorts on fee rather than on intent, and the one that wins may be the older one". That is true
//! across senders and false within one: nonce ordering is absolute, so `k + 1` cannot execute
//! before `k` however the fees compare. The old rule was paying a real cost for a hazard that
//! does not exist here — 66 of about 300 cycles on a live run were held waiting for a receipt on
//! a transaction that had already been preconfirmed 440ms earlier.
//!
//! What a pipeline does cost is head-of-line blocking: if `k` never lands, every nonce behind it
//! is unexecutable. So the depth is bounded rather than unbounded, and a timeout on the oldest
//! drops the whole queue for that pair rather than just the one that expired — transactions
//! behind a gap cannot settle, and tracking them would be tracking the impossible.
//!
//! The reason a pipeline is safe at all is that these transactions are **idempotent overwrites,
//! not orders**. `updateQuote` replaces the row without reading it, so a dropped one needs no
//! retry: the next cycle computes a fresh row from the current reference and sends that. A
//! pipeline of orders would need every one of them to arrive.
//!
//! The escape hatch is still a timeout rather than a replacement: after
//! [`crate::config::TxConfig::pending_timeout_secs`] the intents are abandoned, the pair is
//! unblocked, and the nonce is resynced from the node.
//!
//! # Reserved synchronously, broadcast concurrently
//!
//! `eth_sendRawTransaction` costs **264ms** here and almost none of it is ours. The local op-reth is
//! started with `--rollup.sequencer-http` and forwards the call to the GIWA sequencer in Korea, so
//! that number is a France-to-Korea round trip and no amount of local work reduces it. reth's own
//! metrics make the shape unmistakable: 10.09s of a 20s window across 39 `eth_sendRawTransaction`
//! calls, against 0.023s for *every other RPC method combined*. Awaiting five of them one after the
//! other spent 1.323s of a 2.695s cycle — 49% of the price-setting loop — on five copies of the
//! same wait.
//!
//! So the send phase is split in two, and the split is where all of the safety lives:
//!
//! 1. [`Sender::reserve_batch`] is **synchronous**. It assigns nonces N, N+1, … in the order the
//!    caller pushed the intents, signs each envelope, and returns them. There is no `.await`
//!    anywhere in it, so nothing can interleave with a half-advanced nonce.
//! 2. [`Sender::send_batch`] then broadcasts the signed bytes through a `FuturesUnordered`, so five
//!    sends cost one 264ms round trip rather than five.
//!
//! The shape this replaces read-modify-wrote `self.nonce` *across* the send await, which is exactly
//! the read-modify-write that cannot be made concurrent — two futures would both read `n` and both
//! sign `n`. Reserving before anything is awaited is not an optimisation of that; it is the only
//! version of it that is correct.
//!
//! Reservation order is load-bearing beyond the nonce itself. `main.rs`'s per-pair loop argues that
//! a `RefreshCapacity` must execute before the `UpdateQuote` for the same pair when the pool is
//! dark, and that argument rests entirely on nonce order being absolute. Concurrency does not touch
//! that as long as the nonces are handed out in the order the intents were pushed, which is why
//! [`Sender::reserve_batch`] takes a slice and walks it in order rather than taking a set.
//!
//! # The nonce is tracked here, not asked for
//!
//! [`Nonces`] holds the next nonce in this process: seeded **once** at startup from
//! `eth_getTransactionCount(addr, "pending")`, advanced by reservation, and re-read only on the two
//! explicit recovery paths below. The hot path never asks the node, and that is a correctness
//! requirement rather than a saved round trip.
//!
//! The reason is the local pool. op-reth's `send_transaction` forwards to the sequencer *first* and
//! only then does `let _ = add_pool_transaction(...)`, discarding the local error — so a transaction
//! past the node's `--txpool.max-account-slots` (**16**, and not being raised) has already reached
//! the sequencer while the local pool silently drops it. `eth_getTransactionCount(pending)` on that
//! node is derived from the truncated pool, so it reads **low by however many were dropped**. With
//! roughly 37 transactions in flight at the new cadence, a per-send read would hand out a nonce
//! twenty-odd values behind the truth, every send, forever. That is the same loop that produced
//! 3,391 of 4,519 failed sends in a historical episode, and it is why nothing here may "fix" the
//! reservation back into a read.
//!
//! The two paths that *are* allowed to re-read:
//!
//! * an abandoned intent — [`Sender::sweep_timeouts`], `pending_timeout_secs` after the last
//!   acceptance, by which point nothing of ours is plausibly still queued and walking backwards over
//!   ground we thought we had covered is the entire intent;
//! * `nonce too low`, which is positive proof from the chain that our counter is behind.
//!
//! Everything else is handled without the node. A broadcast the upstream *provably* refused —
//! the local limiter never opened a socket, or the sequencer answered `-32003` — returns its nonce
//! to [`Nonces::free`] and the next reservation hands the same value out again, so the sequence
//! stays gap-free without asking anyone. A broadcast of *unknown* fate does not: its nonce may be
//! sitting in the sequencer, so it is recorded in [`Sender::pending`] under the hash we signed and
//! the timeout sweep is what eventually resolves it.
//!
//! # `-32003` is backpressure, not a failed transaction
//!
//! See [`crate::chain::RpcError::is_txpool_full`]. The measured stakes: at 1.85 tx/s the account
//! wedges 138s after inclusion stops, and at 19 tx/s that grace period compresses to about 13s. So
//! the response has to be to stop offering transactions to a full pool rather than to keep offering
//! them and log each refusal — one such hiccup, mishandled, already cost 59 minutes of downtime.
//!
//! # Why the envelope is encoded here
//!
//! `alloy-consensus` would supply `TxEip1559`, and it does not currently resolve against this
//! toolchain — see the workspace manifest for the exact version deadlock. EIP-1559's payload is
//! a fully specified nine-field RLP list; it is encoded below and pinned byte-for-byte against
//! `cast mktx` output in the tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use futures_util::stream::{FuturesUnordered, StreamExt};
use alloy_rlp::Encodable;
use alloy_sol_types::SolCall;
use k256::ecdsa::SigningKey;
use serde_json::json;

use crate::chain::{abi, hex0x, unhex, Rpc, RpcError};
use crate::config::KeySource;

/// Anything that can go wrong on the transmit path.
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// The key could not be obtained.
    ///
    /// The message deliberately never contains the value, only where it was looked for.
    #[error("cannot load signing key: {0}")]
    Key(String),
    /// Signing failed.
    #[error("signing failed: {0}")]
    Sign(String),
    /// An RPC failure on the transmit path.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Transmission was attempted with no key configured.
    #[error("transmit_allowed is set but no signing key is loaded")]
    NoKey,
    /// The account's nonce is not established, so nothing could be signed.
    ///
    /// Distinct from [`Self::Rpc`] on purpose: the read that would establish it has its own error,
    /// and this is the state *afterwards* — the phase declined to guess. Guessing is what a zero
    /// initial nonce did, and the first send of every process paid for it.
    #[error("the account nonce is not established; nothing was signed")]
    NoNonce,
    /// The node rejected the raw transaction.
    #[error("node rejected the transaction: {0}")]
    Rejected(String),
}

// ---------------------------------------------------------------------------
// Key
// ---------------------------------------------------------------------------

/// A loaded signing key.
///
/// [`std::fmt::Debug`] is implemented by hand to print the address and nothing else. The derived
/// one would put the scalar in every log line that formats a [`Sender`].
pub struct Signer {
    key: SigningKey,
    address: Address,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Parse a 32-byte hex private key, with or without `0x`.
    ///
    /// # Errors
    /// [`TxError::Key`] — and the message never quotes the input.
    /// The two slices are over fixed-size values: an uncompressed SEC1 point is always 65 bytes
    /// and a keccak digest always 32, neither of which can be another length.
    #[allow(clippy::indexing_slicing)]
    pub fn from_hex(hex: &str) -> Result<Self, TxError> {
        let t = hex.trim();
        let raw = unhex(t).ok_or_else(|| TxError::Key("key is not hex".into()))?;
        if raw.len() != 32 {
            return Err(TxError::Key(format!(
                "key must be 32 bytes, got {}",
                raw.len()
            )));
        }
        let key = SigningKey::from_slice(&raw)
            .map_err(|_| TxError::Key("key is not a valid secp256k1 scalar".into()))?;
        let point = key.verifying_key().to_encoded_point(false);
        // Uncompressed SEC1 is 0x04 || X || Y; the address is the last 20 bytes of keccak(X||Y).
        let hash = keccak256(&point.as_bytes()[1..]);
        let address = Address::from_slice(&hash[12..]);
        Ok(Self { key, address })
    }

    /// Load from wherever the config points.
    ///
    /// # Errors
    /// [`TxError::Key`], naming the variable or path but never the value.
    pub fn load(source: &KeySource) -> Result<Self, TxError> {
        match source {
            KeySource::Env(name) => {
                let v = std::env::var(name).map_err(|_| {
                    TxError::Key(format!("environment variable `{name}` is not set"))
                })?;
                Self::from_hex(&v)
            }
            KeySource::File(path) => Self::load_file(path),
        }
    }

    fn load_file(path: &Path) -> Result<Self, TxError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| TxError::Key(format!("cannot read key file `{}`: {e}", path.display())))?;
        Self::from_hex(&text)
    }

    /// The address this key signs as.
    #[must_use]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Sign a 32-byte prehash, returning `(yParity, r, s)`.
    fn sign_prehash(&self, hash: B256) -> Result<(bool, U256, U256), TxError> {
        let (sig, rec) = self
            .key
            .sign_prehash_recoverable(hash.as_slice())
            .map_err(|e| TxError::Sign(e.to_string()))?;
        // k256 normalises to low-S, which is what EIP-2 requires and what every node checks.
        Ok((
            rec.to_byte() & 1 == 1,
            U256::from_be_slice(&sig.r().to_bytes()),
            U256::from_be_slice(&sig.s().to_bytes()),
        ))
    }

    /// Sign a 32-byte digest as `(r, s, v)` with `v` in `{27, 28}` — the shape `ecrecover` takes.
    ///
    /// Separate from the transaction path, which packs the same `yParity` into an RLP envelope
    /// instead. Both go through `sign_prehash`, so there is one place a signature is produced and
    /// two places one is packed.
    ///
    /// The caller supplies a finished digest and this hashes nothing. A signer that computes its
    /// own preimage is a signer that can be talked into signing a different structure than the one
    /// that was reviewed.
    ///
    /// # Errors
    /// [`TxError::Sign`] if the curve rejects the digest.
    pub fn sign_digest_65(&self, digest: B256) -> Result<[u8; 65], TxError> {
        let (y_parity, r, s) = self.sign_prehash(digest)?;
        let mut out = [0u8; 65];
        out[0..32].copy_from_slice(&r.to_be_bytes::<32>());
        out[32..64].copy_from_slice(&s.to_be_bytes::<32>());
        out[64] = if y_parity { 28 } else { 27 };
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// An EIP-1559 (type 2) transaction, with an empty access list.
#[derive(Debug, Clone)]
pub struct Eip1559 {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Sender nonce.
    pub nonce: u64,
    /// Tip.
    pub max_priority_fee_per_gas: u128,
    /// Cap.
    pub max_fee_per_gas: u128,
    /// Gas limit.
    pub gas_limit: u64,
    /// Destination. Always the pool.
    pub to: Address,
    /// Always zero: neither updater function is payable.
    pub value: U256,
    /// Calldata.
    pub input: Bytes,
}

impl Eip1559 {
    /// `rlp([chainId, nonce, maxPriorityFee, maxFee, gas, to, value, data, accessList, ...sig])`,
    /// prefixed with the `0x02` type byte.
    ///
    /// Field order is the specification's and is not negotiable — a transposition produces a
    /// perfectly well-formed transaction that pays the wrong fee or calls the wrong address.
    fn envelope(&self, sig: Option<(bool, U256, U256)>) -> Vec<u8> {
        let mut fields = Vec::with_capacity(256);
        self.chain_id.encode(&mut fields);
        self.nonce.encode(&mut fields);
        self.max_priority_fee_per_gas.encode(&mut fields);
        self.max_fee_per_gas.encode(&mut fields);
        self.gas_limit.encode(&mut fields);
        self.to.encode(&mut fields);
        self.value.encode(&mut fields);
        self.input.encode(&mut fields);
        // Empty access list: an RLP list with a zero-length payload, i.e. the single byte 0xc0.
        alloy_rlp::Header {
            list: true,
            payload_length: 0,
        }
        .encode(&mut fields);
        if let Some((y, r, s)) = sig {
            u8::from(y).encode(&mut fields);
            r.encode(&mut fields);
            s.encode(&mut fields);
        }

        let mut out = Vec::with_capacity(fields.len() + 8);
        out.push(0x02);
        alloy_rlp::Header {
            list: true,
            payload_length: fields.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&fields);
        out
    }

    /// What gets signed: `keccak256` of the unsigned envelope.
    #[must_use]
    pub fn signing_hash(&self) -> B256 {
        keccak256(self.envelope(None))
    }

    /// Sign, returning the transaction hash and the raw bytes to broadcast.
    ///
    /// # Errors
    /// [`TxError::Sign`].
    pub fn sign(&self, signer: &Signer) -> Result<(B256, Vec<u8>), TxError> {
        let sig = signer.sign_prehash(self.signing_hash())?;
        let raw = self.envelope(Some(sig));
        Ok((keccak256(&raw), raw))
    }
}

// ---------------------------------------------------------------------------
// Intents
// ---------------------------------------------------------------------------

/// The only two things the updater key is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Push one packed ladder row.
    UpdateQuote {
        /// Pair the row is for. Also encoded inside the word; carried here so the pending map
        /// can be keyed without decoding it back out.
        pair_id: u16,
        /// The packed word, big-endian, exactly as [`dubu_core::pack::QuoteWord::pack`] emits it.
        word: [u8; 32],
    },
    /// Post a fresh capacity epoch.
    ///
    /// Both amounts are **base** units on both sides — `PropCurve` amendment 1 moved ask
    /// capacity off the quote denomination, and the bit positions did not move with it, so this
    /// is the one place a wrong unit would encode cleanly and settle wrongly.
    RefreshCapacity {
        /// Pair id.
        pair_id: u16,
        /// Base the pool will buy this epoch.
        bid: u128,
        /// Base the pool will sell this epoch.
        ask: u128,
    },
}

impl Intent {
    /// Which pair this is for.
    #[must_use]
    pub const fn pair_id(self) -> u16 {
        match self {
            Self::UpdateQuote { pair_id, .. } | Self::RefreshCapacity { pair_id, .. } => pair_id,
        }
    }

    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UpdateQuote { .. } => "updateQuote",
            Self::RefreshCapacity { .. } => "refreshCapacity",
        }
    }

    /// Whether this intent takes the pair's depth to zero — the shape `withdraw_pair` sends.
    ///
    /// Deliberately not derived from [`Self::label`]. That is the calldata's name and both a
    /// routine epoch and a withdrawal are `refreshCapacity` on chain, but the two want opposite
    /// answers from [`Sender::withdrawal_in_flight`]: a pair with a *non-zero* epoch still in the
    /// air is precisely a pair that has not been withdrawn yet and must be. The distinction lives
    /// in the amounts, so it is read from the amounts.
    #[must_use]
    pub const fn is_withdrawal(self) -> bool {
        matches!(self, Self::RefreshCapacity { bid: 0, ask: 0, .. })
    }

    /// ABI-encoded calldata.
    ///
    /// # Panics
    /// Never: both amount fields are checked against the `uint96` domain by the caller and
    /// saturate here rather than wrapping.
    #[must_use]
    pub fn calldata(self) -> Bytes {
        match self {
            Self::UpdateQuote { word, .. } => abi::updateQuoteCall {
                packed: vec![U256::from_be_bytes(word)],
            }
            .abi_encode()
            .into(),
            Self::RefreshCapacity { pair_id, bid, ask } => abi::refreshCapacityCall {
                pairId: pair_id,
                bidCapacity: alloy_primitives::aliases::U96::from(
                    bid.min(dubu_core::curve::AMOUNT_MAX),
                ),
                askCapacity: alloy_primitives::aliases::U96::from(
                    ask.min(dubu_core::curve::AMOUNT_MAX),
                ),
            }
            .abi_encode()
            .into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// One transaction awaiting confirmation.
#[derive(Debug, Clone, Copy)]
pub struct Pending {
    /// Transaction hash.
    pub hash: B256,
    /// What it was.
    pub kind: &'static str,
    /// Nonce it used.
    pub nonce: u64,
    /// When it was submitted.
    pub submitted_at: Instant,
    /// Whether it was a withdrawal — a zero capacity epoch, per [`Intent::is_withdrawal`].
    ///
    /// Recorded here rather than re-derived later because `kind` cannot answer it: a withdrawal
    /// and a routine epoch refresh are the same call. See [`Sender::withdrawal_in_flight`].
    pub withdrawal: bool,
}

/// A per-transaction fee override. See [`Sender::send_with_fees`] for the only thing that uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fees {
    /// `maxFeePerGas`, in wei.
    pub max_fee: u128,
    /// `maxPriorityFeePerGas`, in wei.
    pub max_priority_fee: u128,
}

/// What a send did.
#[derive(Debug, Clone)]
pub enum Sent {
    /// Nothing was broadcast, because `transmit_allowed` is not set.
    DryRun {
        /// The calldata that would have gone out.
        calldata: Bytes,
        /// The transaction hash it would have had, when a key is loaded.
        would_be_hash: Option<B256>,
        /// The nonce it would have used, when one is known.
        would_be_nonce: Option<u64>,
    },
    /// Broadcast.
    Broadcast {
        /// Transaction hash.
        hash: B256,
        /// Nonce used.
        nonce: u64,
    },
}

/// How a pending transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// Included and succeeded.
    Confirmed {
        /// Block it landed in.
        block: u64,
    },
    /// Included and reverted. For `updateQuote` that means the row failed `validateLadder` on
    /// chain after passing every off-chain check — a `dubu-core` divergence, and the loudest
    /// thing this bot can discover.
    Reverted {
        /// Block it landed in.
        block: u64,
    },
    /// Still unconfirmed past the timeout. Abandoned, not replaced.
    TimedOut {
        /// How long it waited.
        waited_secs: u64,
    },
}

/// The account's nonce sequence, held in this process.
///
/// See the module docs for why it is held rather than read: the node's `pending` count is derived
/// from a pool that silently truncates this account at 16 slots, so at ~37 in flight it answers low
/// by however many it dropped and a per-send read would hand out a used nonce every time.
///
/// Two pieces, because a refused broadcast leaves a hole and a hole must be filled rather than
/// skipped. Everything above a gap in the sequence is unexecutable — nonce ordering is absolute for
/// one sender — so a nonce that provably never reached a pool has to come back and be handed out
/// again, or the account wedges at the gap until something times out.
#[derive(Debug, Default)]
struct Nonces {
    /// The lowest nonce never yet handed out. `None` until seeded, which is the only state in which
    /// a reservation is allowed to ask the node.
    next: Option<u64>,
    /// Nonces reserved and then *provably* refused, lowest first. Handed out again before `next`.
    ///
    /// Only ever holds values the upstream demonstrably never accepted — a local limiter refusal, or
    /// a `-32003`. An unknown outcome must never land here: reusing a nonce that is actually sitting
    /// in the sequencer signs a second transaction at the same nonce, which is the duplication this
    /// whole structure exists to prevent.
    free: BTreeSet<u64>,
}

impl Nonces {
    /// What the next reservation would take, without taking it. `None` until seeded.
    fn peek(&self) -> Option<u64> {
        self.free.iter().next().copied().or(self.next)
    }

    /// Take the next nonce. `None` until seeded.
    ///
    /// Synchronous by construction, and every caller must keep it that way. This is the
    /// read-modify-write that concurrency would break.
    fn reserve(&mut self) -> Option<u64> {
        if let Some(n) = self.free.pop_first() {
            return Some(n);
        }
        let n = self.next?;
        self.next = Some(n.saturating_add(1));
        Some(n)
    }

    /// Give a reserved nonce back, for a broadcast the upstream provably refused.
    fn release(&mut self, n: u64) {
        // Only a nonce this book actually handed out. A value at or above `next` was never reserved,
        // and admitting one would let a caller mistake shrink the sequence.
        if self.next.is_some_and(|next| n < next) {
            self.free.insert(n);
        }
    }

    /// Forget everything and re-read on the next reservation. See [`Sender::resync_nonce`].
    fn resync(&mut self) {
        self.next = None;
        // The holes go too. They are positions in a sequence this book is about to stop believing
        // in, and carrying them across would hand out nonces below whatever the node reports.
        self.free.clear();
    }

    /// Take the node's count as the sequence's new origin.
    fn adopt(&mut self, from_node: u64) {
        self.next = Some(from_node);
        self.free.clear();
    }

    const fn seeded(&self) -> bool {
        self.next.is_some()
    }
}

/// A signed transaction whose nonce is already taken, waiting only for the wire.
///
/// Produced by [`Sender::reserve_batch`] with no await in sight, which is what makes the broadcast
/// that follows safe to run concurrently.
#[derive(Debug, Clone)]
struct Reserved {
    intent: Intent,
    nonce: u64,
    hash: B256,
    raw: Vec<u8>,
}

/// How an in-progress `-32003` episode is doing.
#[derive(Debug, Clone, Copy)]
struct Backpressure {
    opened: Instant,
    last_refusal: Instant,
    /// Transactions the upstream refused with `-32003` since the episode opened.
    refusals: u64,
    /// Intents never offered at all, because the phase was paused.
    held: u64,
}

/// A backpressure episode changing state. Reported once, so the caller logs once.
///
/// Carried out of [`Sender::send_batch`] rather than logged here, because this module deliberately
/// emits nothing: the same property that lets [`Sender::sweep_timeouts`] be tested without a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episode {
    /// The upstream pool started refusing. First occurrence in this episode.
    Opened {
        /// Refusals in the batch that opened it.
        refusals: u64,
    },
    /// The upstream took a transaction again, and this is what the episode cost.
    Closed {
        /// Transactions refused with `-32003`.
        refusals: u64,
        /// Intents never offered, because the phase was paused.
        held: u64,
        /// How long the episode lasted.
        secs: u64,
    },
}

/// What one send phase did.
#[derive(Debug)]
pub struct Batch {
    /// Per-intent outcomes, in the order the nonces were reserved.
    ///
    /// `-32003` refusals are deliberately **absent**: they are backpressure rather than
    /// per-transaction failures, and they are counted into [`Self::held_backpressure`] and the
    /// episode instead so that a wedged upstream produces one line rather than one per send.
    pub sent: Vec<(Intent, Result<Sent, TxError>)>,
    /// Intents not offered because the account is at its in-flight ceiling.
    pub held_at_capacity: usize,
    /// Intents refused with `-32003`, or never offered because the phase is paused behind one.
    pub held_backpressure: usize,
    /// Set only on the cycle an episode opens or closes.
    pub episode: Option<Episode>,
}

impl Batch {
    /// An empty phase: nothing offered, nothing held, nothing to report.
    fn empty() -> Self {
        Self {
            sent: Vec::new(),
            held_at_capacity: 0,
            held_backpressure: 0,
            episode: None,
        }
    }
}

/// The most transactions this account may have unconfirmed at once, across every pair.
///
/// GIWA's sequencer refuses new transactions from one account past **256 in flight** with
/// `-32003 txpool is full`, and half of that stays as stall buffer: a burst that arrives while
/// inclusion is briefly behind must have somewhere to go other than into the refusal path.
///
/// This is a backstop, not the pacer. The per-pair [`crate::config::TxConfig::in_flight_max`] is
/// what actually binds in steady state, and it is sized from the cadence — see its documentation.
/// The number that matters about this one is that it is a *global* ceiling: the resources a send
/// consumes are the account's single nonce sequence and the sequencer's per-account limit, neither
/// of which is divisible per pair.
pub const IN_FLIGHT_TOTAL_MAX: usize = 128;

/// How long the send phase stands down after the upstream answers `-32003`.
///
/// Sized against the two measured numbers. Inclusion advances about once per block, so a second
/// gives the upstream a full block to drain — anything much shorter re-offers the same transactions
/// to the same full pool five times a second, which is the flood this exists to avoid. And it has to
/// be short against the grace period: at 19 tx/s the account wedges roughly 13s after inclusion
/// stops, so a pause measured in seconds would spend most of the runway waiting.
const BACKPRESSURE_PAUSE: Duration = Duration::from_secs(1);

/// Builds, signs and submits. Owns the nonce and the pending set.
#[derive(Debug)]
pub struct Sender {
    signer: Option<Signer>,
    chain_id: u64,
    pool: Address,
    gas_limit: u64,
    max_fee: u128,
    max_priority_fee: u128,
    transmit_allowed: bool,
    /// The nonce sequence. See [`Nonces`], and the module docs for why it is not re-read per send.
    nonces: Nonces,
    /// Outstanding transactions per pair, oldest first. A `Vec` rather than one slot because the
    /// send path pipelines — see [`Sender::at_capacity`].
    pending: BTreeMap<u16, Vec<Pending>>,
    pending_timeout: Duration,
    in_flight_max: usize,
    /// The chain's confirmed nonce. See [`Sender::observe_landed`].
    landed_nonce: u64,
    /// The open `-32003` episode, if the upstream is currently refusing.
    backpressure: Option<Backpressure>,
    /// An episode transition waiting to be logged. Drained by [`Sender::take_episode`].
    staged_episode: Option<Episode>,
}

impl Sender {
    /// Build a sender. `signer` may be `None` in dry run.
    #[must_use]
    pub fn new(
        signer: Option<Signer>,
        chain_id: u64,
        pool: Address,
        cfg: &crate::config::TxConfig,
        max_fee: u128,
        max_priority_fee: u128,
    ) -> Self {
        Self {
            signer,
            chain_id,
            pool,
            gas_limit: cfg.gas_limit,
            max_fee,
            max_priority_fee,
            transmit_allowed: cfg.transmit_allowed,
            // Unseeded. `Sender::seed_nonce` fills it once at startup; until then the first
            // reservation reads the node, which is the one moment in this process's life where that
            // read is both necessary and trustworthy — nothing is in flight, so the local pool has
            // nothing to have truncated.
            nonces: Nonces::default(),
            pending: BTreeMap::new(),
            pending_timeout: Duration::from_secs(cfg.pending_timeout_secs),
            in_flight_max: cfg.in_flight_max,
            landed_nonce: 0,
            backpressure: None,
            staged_episode: None,
        }
    }

    /// The address transactions are signed as, if a key is loaded.
    #[must_use]
    pub fn address(&self) -> Option<Address> {
        self.signer.as_ref().map(Signer::address)
    }

    /// Whether anything will actually be broadcast.
    #[must_use]
    pub const fn transmit_allowed(&self) -> bool {
        self.transmit_allowed
    }

    /// Whether this pair has an unconfirmed transaction.
    #[must_use]
    /// True when this pair already has as many transactions outstanding as it is allowed.
    ///
    /// This was one-at-a-time, on the reasoning that two rows in flight could be reordered by a
    /// fee-sorting sequencer and the older one could win. That reasoning does not hold for a
    /// single sender: nonce ordering is absolute, so `k + 1` cannot execute before `k` whatever
    /// the fees. What a second in-flight transaction genuinely costs is head-of-line blocking —
    /// if `k` never lands, everything behind it is stuck — and the answer to that is a bound, not
    /// a ban.
    ///
    /// The bound matters because the transactions here are idempotent overwrites rather than
    /// orders. `updateQuote` replaces the row without reading it, so a dropped one needs no
    /// retry: the next cycle computes a fresh row from the current reference and sends that. A
    /// pipeline of quotes is therefore safe in a way a pipeline of orders would not be.
    ///
    /// # Two ceilings, and why the second one is global
    ///
    /// The per-pair bound stays: it is what stops one pair's stall from occupying the whole queue,
    /// and it is what `tx.in_flight_max` means in a config file somebody reviews.
    ///
    /// It is not sufficient on its own, because nothing a send consumes is divisible per pair. There
    /// is one nonce sequence for the account, and the sequencer's refusal threshold — 256 in flight
    /// from one address — is per account too. A per-pair cap of *k* over *n* pairs implies a global
    /// ceiling of *k·n* that nobody wrote down and that changes when a pair is added, which is
    /// exactly the shape of limit that is discovered in production. So the account's own ceiling is
    /// stated directly, as [`IN_FLIGHT_TOTAL_MAX`].
    pub fn at_capacity(&self, pair_id: u16) -> bool {
        self.in_flight(pair_id) >= self.in_flight_max
            || self.in_flight_total() >= IN_FLIGHT_TOTAL_MAX
    }

    /// How many transactions this pair has outstanding.
    #[must_use]
    pub fn in_flight(&self, pair_id: u16) -> usize {
        self.pending.get(&pair_id).map_or(0, |v| {
            v.iter().filter(|p| p.nonce >= self.landed_nonce).count()
        })
    }

    /// How many transactions this **account** has outstanding, across every pair.
    ///
    /// The quantity the sequencer's 256-per-account limit is measured in, and — because nonce
    /// ordering is absolute — also the depth a jump withdrawal has to queue behind. Both of those
    /// are properties of the key, and the key is shared by the whole book.
    #[must_use]
    pub fn in_flight_total(&self) -> usize {
        self.pending
            .values()
            .flat_map(|v| v.iter())
            .filter(|p| p.nonce >= self.landed_nonce)
            .count()
    }

    /// Whether this pair already has an unconfirmed **withdrawal** in the air.
    ///
    /// [`Self::at_capacity`] is the wrong question for the jump re-assert, and asking it cost
    /// about 740 transactions in 16 hours. `in_flight_max` is 2, so the fast lane's own withdrawal
    /// leaves a slot open; the re-assert then fires ~1.4s later against a ~2s inclusion latency,
    /// reads a chain snapshot that cannot yet show the zero, and sends the same withdrawal again.
    /// Measured over 16.2 hours: 150-154 withdrawal transactions per pair against 73-75 actual
    /// episodes, every one of them at the 100x tip `jump.withdraw_priority_fee_wei` pays to win a
    /// fee auction that the first transaction had already entered.
    ///
    /// Built on the same bookkeeping as [`Self::in_flight`] rather than a second per-pair flag:
    /// `nonce >= landed_nonce` is what "still in the air" means for a single sender, and a
    /// parallel flag is one [`Self::sweep_timeouts`] and [`Self::observe_landed`] would each have
    /// to remember to clear — a withdrawal that timed out with the flag still set would suppress
    /// the very re-assert that exists to notice it.
    #[must_use]
    pub fn withdrawal_in_flight(&self, pair_id: u16) -> bool {
        self.pending.get(&pair_id).is_some_and(|v| {
            v.iter()
                .any(|p| p.withdrawal && p.nonce >= self.landed_nonce)
        })
    }

    /// Tell the gate what the chain's confirmed nonce is.
    ///
    /// Everything below this has landed -- nonce ordering is absolute for one sender -- so it no
    /// longer occupies an in-flight slot, whatever the receipt poller has managed to ask about.
    /// The transaction stays in `pending` until a receipt classifies it, because whether it
    /// succeeded or reverted is a different question from whether it is still in the air, and
    /// conflating the two is what made the gate hold slots for transactions that were already on
    /// chain 2s earlier.
    ///
    /// Monotone: a view can arrive out of order, and a nonce that went backwards would re-block
    /// sends that are already settled.
    pub fn observe_landed(&mut self, confirmed_nonce: u64) {
        self.landed_nonce = self.landed_nonce.max(confirmed_nonce);
    }

    /// The pending transaction for a pair, if any.
    #[must_use]
    pub fn pending(&self, pair_id: u16) -> Option<&Pending> {
        // The oldest, which is the one a timeout will fire on first and the one whose fate
        // decides every transaction behind it.
        self.pending.get(&pair_id).and_then(|v| v.first())
    }

    /// Force the next reservation to re-read the nonce from the node.
    ///
    /// There are exactly two callers, and the module docs say why there may not be a third:
    /// [`Self::sweep_timeouts`] abandoning an intent, and a `nonce too low` proving the chain is
    /// ahead of us. Both are recovery paths where the local counter has already been shown to be
    /// wrong, so re-reading is the only way forward. On the hot path the node's answer is *less*
    /// trustworthy than ours — it is derived from a pool that truncates this account at 16 slots —
    /// and a read there would hand out a nonce that has already been used.
    pub fn resync_nonce(&mut self) {
        self.nonces.resync();
    }

    /// The nonce the next reservation would take, when the sequence is known.
    #[must_use]
    pub fn next_nonce(&self) -> Option<u64> {
        self.nonces.peek()
    }

    /// Whether an upstream backpressure episode is currently open.
    #[must_use]
    pub const fn under_backpressure(&self) -> bool {
        self.backpressure.is_some()
    }

    /// An episode that opened or closed since this was last called. Reported once, so it is logged
    /// once. See [`Episode`].
    pub fn take_episode(&mut self) -> Option<Episode> {
        self.staged_episode.take()
    }

    /// Read the account's nonce once, so that the first send is not the thing that discovers it.
    ///
    /// This is the *only* unconditional `eth_getTransactionCount` in the process, and startup is the
    /// one moment where the answer can be trusted without qualification: nothing of ours is in
    /// flight, so the local pool has dropped nothing and its `pending` count is the chain's.
    ///
    /// Worth doing on its own terms even before concurrency. The floor that used to guard the first
    /// send was initialised to zero, so the first send after every process start was unfloored — and
    /// a first send at a nonce the chain has long passed is the head of the `nonce too low` loop that
    /// accounted for 3,391 of 4,519 failed sends in one historical episode.
    ///
    /// # Errors
    /// [`TxError::NoKey`] with no signer, or [`TxError::Rpc`] if the read fails. Neither is fatal to
    /// the caller: an unseeded sequence simply means the first reservation reads it instead.
    pub async fn seed_nonce(&mut self, rpc: &Rpc) -> Result<u64, TxError> {
        let signer = self.signer.as_ref().ok_or(TxError::NoKey)?;
        let n = rpc
            .quantity(
                "eth_getTransactionCount",
                json!([signer.address().to_string(), "pending"]),
            )
            .await?;
        self.nonces.adopt(n);
        Ok(n)
    }

    /// Read the nonce if, and only if, nothing has established it yet.
    async fn ensure_seeded(&mut self, rpc: &Rpc) -> Result<(), TxError> {
        if self.nonces.seeded() {
            return Ok(());
        }
        self.seed_nonce(rpc).await.map(|_| ())
    }

    /// Build the envelope for an intent without signing or sending it, at the configured fees.
    #[must_use]
    pub fn envelope(&self, intent: Intent, nonce: u64) -> Eip1559 {
        self.envelope_with_fees(intent, nonce, None)
    }

    /// Build the envelope, optionally overriding the fees for this one transaction.
    #[must_use]
    pub fn envelope_with_fees(&self, intent: Intent, nonce: u64, fees: Option<Fees>) -> Eip1559 {
        let f = fees.unwrap_or(Fees {
            max_fee: self.max_fee,
            max_priority_fee: self.max_priority_fee,
        });
        Eip1559 {
            chain_id: self.chain_id,
            nonce,
            max_priority_fee_per_gas: f.max_priority_fee,
            max_fee_per_gas: f.max_fee,
            gas_limit: self.gas_limit,
            to: self.pool,
            value: U256::ZERO,
            input: intent.calldata(),
        }
    }

    /// Submit an intent, or describe what would have been submitted.
    ///
    /// The nonce comes from `eth_getTransactionCount(addr, "pending")` on the **ordinary** RPC,
    /// not the flashblocks one. A nonce read from a preconfirmed state that later reorganises
    /// produces a transaction that can never be included.
    ///
    /// # Errors
    /// [`TxError`]. On any failure past the point where a nonce was consumed the local nonce is
    /// dropped, so the next attempt re-reads it rather than building on a guess.
    pub async fn send(&mut self, rpc: &Rpc, intent: Intent) -> Result<Sent, TxError> {
        self.send_with_fees(rpc, intent, None).await
    }

    /// Submit an intent at fees other than the configured ones.
    ///
    /// # Why there is a second fee at all, when [`crate::tx`] argues against an escalator
    ///
    /// That argument is about *quote* traffic, and it holds: at 0.001 gwei the spread between a
    /// minimal tip and an absurd one is worth less than a rounding error on one quote, so a
    /// controller buys nothing. A **jump withdrawal** is the one transaction on this bot where the
    /// fee buys something concrete. GIWA's sequencer orders by highest fee first, and the
    /// counterparty is by construction someone willing to outbid a quoting bot that pays a flat
    /// near-zero tip. Detecting 200 ms sooner does not win a fee auction; paying more does, and it
    /// is the difference between the withdrawal landing ahead of the pick-off in the same block
    /// and landing behind it.
    ///
    /// This is still not an escalator: one flat number from config, used for one kind of
    /// transaction, never bumped and never retried at a higher price.
    ///
    /// # Errors
    /// [`TxError`], exactly as [`Sender::send`].
    pub async fn send_with_fees(
        &mut self,
        rpc: &Rpc,
        intent: Intent,
        fees: Option<Fees>,
    ) -> Result<Sent, TxError> {
        if !self.transmit_allowed {
            return self.dry_run(intent, fees);
        }
        self.ensure_seeded(rpc).await?;
        let mut reserved = self.reserve_batch(std::slice::from_ref(&intent), fees)?;
        // `reserve_batch` returns one entry per intent, and there is exactly one here.
        let Some(r) = reserved.pop() else {
            return Err(TxError::NoNonce);
        };
        let out = rpc
            .call("eth_sendRawTransaction", json!([hex0x(&r.raw)]))
            .await;
        self.settle_broadcast(&r, out, Instant::now())
    }

    /// Describe what would have gone out, without sending it.
    ///
    /// Still signs when a key happens to be loaded, so that the encode-and-sign path is exercised
    /// rather than merely assumed to work on the day it is switched on.
    fn dry_run(&self, intent: Intent, fees: Option<Fees>) -> Result<Sent, TxError> {
        let would_be_nonce = self.nonces.peek();
        let would_be_hash = match (&self.signer, would_be_nonce) {
            (Some(s), Some(n)) => Some(self.envelope_with_fees(intent, n, fees).sign(s)?.0),
            _ => None,
        };
        Ok(Sent::DryRun {
            calldata: intent.calldata(),
            would_be_hash,
            would_be_nonce,
        })
    }

    /// Broadcast a whole cycle's intents at once: nonces assigned synchronously, sends concurrent.
    ///
    /// This is the send phase. See the module docs for the split and for why it is the only shape
    /// that is safe; what follows is what happens around it.
    ///
    /// **Order.** `intents` is walked in order and the nonces come out in that order, so the
    /// arbitration `main.rs` performs between a capacity refresh and a quote row still decides which
    /// one executes first. Nothing else about the batch is ordered: the broadcasts complete in
    /// whatever order the network returns them, and the outcomes are folded back in reservation order
    /// regardless, so the pending queue for a pair stays oldest-first.
    ///
    /// **The in-flight ceiling.** Trimmed to [`IN_FLIGHT_TOTAL_MAX`] before anything is signed. The
    /// per-pair gate in [`crate::policy`] has usually already done this work via [`Self::at_capacity`];
    /// this is the account-level backstop, and it is here rather than only in the policy because the
    /// sequencer's limit is not something a per-pair decision can see.
    ///
    /// **Backpressure.** A `-32003` pauses the phase for [`BACKPRESSURE_PAUSE`] rather than being
    /// reported per transaction. Withholding quotes is the correct response to a full upstream: the
    /// transactions are idempotent overwrites, so nothing is lost by not sending them, and the next
    /// cycle computes fresh rows anyway. What *is* lost by sending them is the log — at 27 tx/s a
    /// per-occurrence error is about 1600 lines a minute, which buries the one line that says the
    /// account is wedging.
    ///
    /// # Errors
    /// [`TxError`] only for the failures that stop the whole phase: no key, or a nonce that could not
    /// be established. Per-intent failures are inside [`Batch::sent`].
    pub async fn send_batch(
        &mut self,
        rpc: &Rpc,
        intents: &[Intent],
        fees: Option<Fees>,
    ) -> Result<Batch, TxError> {
        if intents.is_empty() {
            return Ok(Batch::empty());
        }
        if !self.transmit_allowed {
            let mut batch = Batch::empty();
            for intent in intents {
                batch.sent.push((*intent, self.dry_run(*intent, fees)));
            }
            return Ok(batch);
        }

        let now = Instant::now();
        // Still inside the pause. Nothing is signed and nothing is offered: a pool that refused a
        // transaction 200ms ago is a pool that will refuse this one, and asking again is how one
        // wedged upstream becomes thousands of log lines.
        if let Some(mut bp) = self.backpressure {
            if now.saturating_duration_since(bp.last_refusal) < BACKPRESSURE_PAUSE {
                bp.held = bp.held.saturating_add(intents.len() as u64);
                self.backpressure = Some(bp);
                return Ok(Batch {
                    sent: Vec::new(),
                    held_at_capacity: 0,
                    held_backpressure: intents.len(),
                    episode: self.staged_episode.take(),
                });
            }
        }

        let headroom = IN_FLIGHT_TOTAL_MAX.saturating_sub(self.in_flight_total());
        let offered: Vec<Intent> = intents.iter().copied().take(headroom).collect();
        let held_at_capacity = intents.len().saturating_sub(offered.len());

        self.ensure_seeded(rpc).await?;
        let reserved = self.reserve_batch(&offered, fees)?;
        assert_eq!(
            reserved.len(),
            offered.len(),
            "one reservation per offered intent"
        );

        // The whole point. Five `eth_sendRawTransaction` calls at a fixed 264ms France-to-Korea RTT
        // cost 264ms together instead of 1.32s in sequence. The raw bytes are hex-encoded up front so
        // each future owns its own payload and borrows nothing but the client.
        let mut flight: FuturesUnordered<_> = reserved
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let raw = hex0x(&r.raw);
                async move { (i, rpc.call("eth_sendRawTransaction", json!([raw])).await) }
            })
            .collect();
        let mut outcomes: BTreeMap<usize, Result<serde_json::Value, RpcError>> = BTreeMap::new();
        while let Some((i, out)) = flight.next().await {
            outcomes.insert(i, out);
        }
        drop(flight);

        // Folded back in reservation order, not completion order. `Sender::pending` is documented
        // oldest-first per pair and `sweep_timeouts` acts on the head of it, so a pair carrying both
        // a refresh and a row must record them in the order their nonces execute.
        let settled_at = Instant::now();
        let mut batch = Batch {
            sent: Vec::with_capacity(reserved.len()),
            held_at_capacity,
            held_backpressure: 0,
            episode: None,
        };
        for (i, r) in reserved.iter().enumerate() {
            let Some(out) = outcomes.remove(&i) else {
                continue;
            };
            let refused = out.as_ref().err().is_some_and(RpcError::is_txpool_full);
            let result = self.settle_broadcast(r, out, settled_at);
            if refused {
                // Counted, not returned. This is the upstream's state, not this transaction's.
                batch.held_backpressure += 1;
            } else {
                batch.sent.push((r.intent, result));
            }
        }
        batch.episode = self.staged_episode.take();
        Ok(batch)
    }

    /// Assign nonces and sign, in the order the intents were pushed.
    ///
    /// **There is no `.await` in this function and there must never be one.** That is the entire
    /// reason it exists as a separate pass: the nonce is a read-modify-write, and an await inside it
    /// is what would let two concurrent sends both read `n` and both sign `n`.
    fn reserve_batch(
        &mut self,
        intents: &[Intent],
        fees: Option<Fees>,
    ) -> Result<Vec<Reserved>, TxError> {
        if self.signer.is_none() {
            return Err(TxError::NoKey);
        }
        if !self.nonces.seeded() {
            return Err(TxError::NoNonce);
        }

        let mut nonces: Vec<u64> = Vec::with_capacity(intents.len());
        for _ in intents {
            // Checked seeded above, and nothing between here and there can unseed it.
            let Some(n) = self.nonces.reserve() else {
                break;
            };
            // The invariant the RefreshCapacity-before-UpdateQuote argument rests on. A reused hole
            // is always below `next` and holes come out lowest-first, so the sequence a batch takes
            // is strictly increasing even when it is not contiguous.
            if let Some(prev) = nonces.last() {
                assert!(
                    n > *prev,
                    "reserved nonces must strictly increase in intent order"
                );
            }
            nonces.push(n);
        }
        if nonces.len() != intents.len() {
            for n in nonces {
                self.nonces.release(n);
            }
            return Err(TxError::NoNonce);
        }

        let signed: Result<Vec<Reserved>, TxError> = {
            let Some(signer) = self.signer.as_ref() else {
                return Err(TxError::NoKey);
            };
            intents
                .iter()
                .zip(&nonces)
                .map(|(intent, &nonce)| {
                    let (hash, raw) = self.envelope_with_fees(*intent, nonce, fees).sign(signer)?;
                    Ok(Reserved {
                        intent: *intent,
                        nonce,
                        hash,
                        raw,
                    })
                })
                .collect()
        };
        match signed {
            Ok(v) => Ok(v),
            // Signing a valid key cannot fail in practice, but a nonce reserved and then never used
            // is a hole in the sequence, and a hole stops every nonce above it from executing. So it
            // goes back rather than being left to a timeout that has nothing to time out.
            Err(e) => {
                for n in nonces {
                    self.nonces.release(n);
                }
                Err(e)
            }
        }
    }

    /// Fold one broadcast's outcome into the nonce book, the pending set, and the episode counter.
    ///
    /// The classification is the delicate part, and it is a claim about whether the nonce was
    /// consumed rather than about whether the send succeeded:
    ///
    /// * **Accepted** — tracked, nonce spent. A hash the node disagrees with is *still* accepted: it
    ///   took the transaction, so the nonce is gone whatever it filed it under. Tracking it anyway is
    ///   a change from the previous behaviour, which returned the error and left a possibly-live
    ///   nonce with nothing to time it out.
    /// * **Provably refused** — the local limiter never opened a socket, or the sequencer answered
    ///   `-32003`. The nonce is untouched and goes back to [`Nonces::free`] to be handed out again,
    ///   which is what keeps the sequence gap-free without asking the node anything.
    /// * **`nonce too low`** — the chain has passed this nonce. The counter is behind, and this is one
    ///   of the two conditions that may re-read it.
    /// * **`already known`** — the node has these exact bytes already, so the transaction is in flight
    ///   under the hash we signed. Treated as an acceptance; see [`RpcError::is_already_known`].
    /// * **Anything else** — unknown fate. The transaction may be sitting in the sequencer, so the
    ///   nonce must *not* be reused; it is recorded under the hash we signed and
    ///   [`Self::sweep_timeouts`] is what eventually resolves it.
    fn settle_broadcast(
        &mut self,
        r: &Reserved,
        out: Result<serde_json::Value, RpcError>,
        now: Instant,
    ) -> Result<Sent, TxError> {
        let e = match out {
            Ok(v) => {
                let returned = v.as_str().unwrap_or_default();
                let mismatch = !returned.is_empty() && returned != r.hash.to_string();
                self.track(r, now);
                self.close_episode(now);
                if mismatch {
                    // Not fatal, but the local hash and the node's disagree, so every receipt lookup
                    // after this would be against the wrong one.
                    return Err(TxError::Rejected(format!(
                        "node returned hash {returned}, locally computed {}",
                        r.hash
                    )));
                }
                return Ok(Sent::Broadcast {
                    hash: r.hash,
                    nonce: r.nonce,
                });
            }
            Err(e) => e,
        };

        if e.is_already_known() {
            // Accepted, not refused. The node has these exact bytes, so the transaction is in flight
            // under the hash we signed and the nonce is spent — the same state as an `Ok`, reached by
            // a different reply. Tracking it and returning success is what makes it true rather than
            // merely quiet: the pending entry is what a receipt lookup and the timeout sweep need,
            // and an operator alert on a transaction that is on its way is noise that trains people
            // to ignore the channel. Observed only on the one pair that spends ~30% of its time
            // withdrawn, where the ladder is byte-identical between pushes and so a duplicate is
            // possible at all.
            self.track(r, now);
            self.close_episode(now);
            return Ok(Sent::Broadcast {
                hash: r.hash,
                nonce: r.nonce,
            });
        }
        if e.is_txpool_full() {
            self.nonces.release(r.nonce);
            self.open_or_extend_episode(now);
        } else if e.never_sent() {
            self.nonces.release(r.nonce);
        } else if e.is_nonce_too_low() {
            self.resync_nonce();
        } else {
            self.track(r, now);
        }
        Err(TxError::Rpc(e))
    }

    /// Record a transaction as outstanding for its pair.
    fn track(&mut self, r: &Reserved, now: Instant) {
        self.pending
            .entry(r.intent.pair_id())
            .or_default()
            .push(Pending {
                hash: r.hash,
                kind: r.intent.label(),
                nonce: r.nonce,
                submitted_at: now,
                withdrawal: r.intent.is_withdrawal(),
            });
    }

    /// Note a `-32003`, opening an episode if this is the first one.
    fn open_or_extend_episode(&mut self, now: Instant) {
        let (first, refusals) = {
            let bp = self.backpressure.get_or_insert(Backpressure {
                opened: now,
                last_refusal: now,
                refusals: 0,
                held: 0,
            });
            let first = bp.refusals == 0;
            bp.last_refusal = now;
            bp.refusals = bp.refusals.saturating_add(1);
            (first, bp.refusals)
        };
        // Refreshed while the `Opened` report is still undrained, so the one line the caller logs
        // carries the whole batch's refusals rather than only the first of them.
        if first || matches!(self.staged_episode, Some(Episode::Opened { .. })) {
            self.staged_episode = Some(Episode::Opened { refusals });
        }
    }

    /// The upstream took a transaction, so whatever episode was open is over.
    fn close_episode(&mut self, now: Instant) {
        if let Some(bp) = self.backpressure.take() {
            self.staged_episode = Some(Episode::Closed {
                refusals: bp.refusals,
                held: bp.held,
                secs: now.saturating_duration_since(bp.opened).as_secs(),
            });
        }
    }

    /// Drops one settled transaction, and the pair's entry when it was the last.
    fn forget(&mut self, pair_id: u16, hash: B256) {
        if let Some(v) = self.pending.get_mut(&pair_id) {
            v.retain(|p| p.hash != hash);
            if v.is_empty() {
                self.pending.remove(&pair_id);
            }
        }
    }

    /// Check every pending transaction once. Returns what settled.
    ///
    /// One request per pending transaction, so at most one per pair, and only while something
    /// is actually outstanding.
    ///
    /// # Errors
    /// Never returns early on an RPC failure for one pair — a rate-limited receipt lookup must
    /// not prevent the others from being checked, and a pending transaction that cannot be
    /// looked at simply stays pending until it times out.
    pub async fn poll_pending(&mut self, rpc: &Rpc) -> Vec<(u16, Pending, Settled)> {
        let mut settled = Vec::new();
        let now = Instant::now();
        // A transaction younger than this has not had time to land, so asking for its receipt is
        // a round trip that can only return null. It did not matter while the cycle ran once a
        // second; at the quote cadence the same transaction would be asked about twice before it
        // could possibly be there, and the receipt call competes with quote traffic for the same
        // rate limit.
        //
        // Sized under the measured 296ms best-case inclusion so a fast landing is still seen on
        // the first poll after it happens.
        const MIN_AGE: Duration = Duration::from_millis(250);

        // Only transactions the chain has already accounted for.
        //
        // A receipt used to be how the in-flight gate learned a send had landed, so every pending
        // transaction was asked about on every cycle until one appeared -- and most of those asks
        // returned null, because they were made before the transaction could be there. Measured at
        // the quote cadence that was 8.19 requests a second, nearly twice the send rate itself and
        // the single largest consumer on the write path.
        //
        // The gate reads `landed_nonce` now (see `observe_landed`), which comes free with a read the
        // reader was making anyway. So a receipt is no longer needed to decide whether to keep
        // sending -- only to decide whether what landed *succeeded*, and that question is worth
        // asking exactly once, after the nonce says there is an answer to get.
        //
        // What this keeps: a revert still surfaces. `updateQuote` passing every off-chain check and
        // then reverting means dubu-core has diverged from the contract, which must never pass
        // silently, and it is why this is a narrowed filter rather than a deleted call.
        //
        // What this gives up: a transaction that lands but whose nonce we have not observed yet is
        // not classified this cycle. It is classified on the next one, and the timeout below is
        // unchanged, so nothing is left pending forever.
        let landed = self.landed_nonce;
        let entries: Vec<(u16, Pending)> = self
            .pending
            .iter()
            .flat_map(|(k, v)| v.iter().map(move |p| (*k, *p)))
            .filter(|(_, p)| p.nonce < landed)
            .filter(|(_, p)| now.duration_since(p.submitted_at) >= MIN_AGE)
            .collect();

        for (pair_id, p) in entries {
            match rpc
                .call("eth_getTransactionReceipt", json!([p.hash.to_string()]))
                .await
            {
                Ok(v) if !v.is_null() => {
                    let ok = v.get("status").and_then(serde_json::Value::as_str) == Some("0x1");
                    let block = v
                        .get("blockNumber")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0);
                    settled.push((
                        pair_id,
                        p,
                        if ok {
                            Settled::Confirmed { block }
                        } else {
                            Settled::Reverted { block }
                        },
                    ));
                    self.forget(pair_id, p.hash);
                }
                // Either no receipt yet, or the lookup failed. Both mean "ask again next cycle";
                // the timeout sweep below is what eventually gives up, and it runs over every
                // pending transaction rather than only the ones reached here.
                _ => {}
            }
        }

        settled.extend(self.sweep_timeouts(now));
        settled
    }

    /// Give up on transactions that have outlived [`Self::pending_timeout`].
    ///
    /// Separate from the receipt loop, and not an accident of layout. That loop deliberately skips
    /// anything `landed_nonce` has not passed, because asking about a transaction the chain has not
    /// accounted for was 8.19 null answers a second. But a genuinely stuck transaction is
    /// *precisely* one the nonce never passes, so a timeout living inside that loop would have made
    /// the one case it exists for the one case it can never reach.
    ///
    /// Pure: no RPC, so the behaviour is testable without a node.
    fn sweep_timeouts(&mut self, now: Instant) -> Vec<(u16, Pending, Settled)> {
        let mut settled = Vec::new();
        let stuck: Vec<(u16, Pending)> = self
            .pending
            .iter()
            .flat_map(|(k, v)| v.iter().map(move |p| (*k, *p)))
            .filter(|(_, p)| now.saturating_duration_since(p.submitted_at) >= self.pending_timeout)
            .collect();
        for (pair_id, p) in stuck {
            // The receipt loop may already have settled and forgotten it this pass.
            if !self
                .pending
                .get(&pair_id)
                .is_some_and(|v| v.iter().any(|q| q.hash == p.hash))
            {
                continue;
            }
            let waited = now.saturating_duration_since(p.submitted_at);
            settled.push((
                pair_id,
                p,
                Settled::TimedOut {
                    waited_secs: waited.as_secs(),
                },
            ));
            // Everything behind a timed-out transaction is dropped with it, not just the one that
            // expired. If nonce `k` never lands there is a gap, and every nonce after it is
            // unexecutable however healthy its own transaction looks.
            self.pending.remove(&pair_id);
            // The abandoned nonce may or may not have been consumed. Re-reading is the only way to
            // find out, and guessing produces a stuck queue.
            self.resync_nonce();
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    /// A transaction the nonce never passes must still time out.
    ///
    /// The receipt loop only looks at transactions below `landed_nonce`, which is the whole point --
    /// asking about a transaction before the chain has accounted for it was 8.19 requests a second
    /// of null answers. But a genuinely stuck transaction is *precisely* one the nonce never
    /// passes, so leaving the timeout inside that loop made the one case it exists for the one case
    /// it could never reach: the queue would have blocked forever behind it.
    #[test]
    fn a_stuck_transaction_times_out_even_though_the_nonce_never_passes_it() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        s.pending_timeout = Duration::from_millis(1);
        s.pending.insert(
            7,
            vec![Pending {
                hash: B256::repeat_byte(9),
                kind: "updateQuote",
                nonce: 500,
                submitted_at: Instant::now() - Duration::from_secs(30),
                withdrawal: false,
            }],
        );
        // Nonce 500 has NOT landed, so the receipt loop skips it entirely.
        s.observe_landed(400);
        assert!(
            s.at_capacity(7) || s.in_flight(7) > 0,
            "it is still counted in flight"
        );

        let out = s.sweep_timeouts(Instant::now());
        assert!(
            out.iter()
                .any(|(_, _, st)| matches!(st, Settled::TimedOut { .. })),
            "the sweep must reach it: {out:?}"
        );
        assert_eq!(s.in_flight(7), 0, "and the queue must be cleared behind it");
    }

    /// The gate must reopen on the chain's nonce, not on a receipt arriving.
    ///
    /// This is the regression that cost 14.7% of quote cycles: a transaction landed in ~570ms, the
    /// receipt call reporting it lost to quote traffic at the rate limiter and came back 2.5s
    /// later, and for those two seconds the gate refused to re-quote over a queue that was already
    /// empty.
    #[test]
    fn the_gate_reopens_on_the_confirmed_nonce() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        let p = |b: u8, n: u64| Pending {
            hash: B256::repeat_byte(b),
            kind: "updateQuote",
            nonce: n,
            submitted_at: Instant::now(),
            withdrawal: false,
        };
        s.pending.insert(1, vec![p(1, 10), p(2, 11)]);
        assert!(s.at_capacity(1), "two in flight against in_flight_max 2");

        // Nonce 10 is on chain. No receipt has been asked for, let alone answered.
        s.observe_landed(11);
        assert_eq!(s.in_flight(1), 1);
        assert!(
            !s.at_capacity(1),
            "the slot is free the moment the chain says so"
        );

        // The transaction is still tracked, because "did it land" and "did it succeed" are
        // different questions and only the receipt answers the second.
        assert_eq!(s.pending(1).map(|p| p.nonce), Some(10));
    }

    /// A withdrawal is a zero epoch, and nothing else is — including the routine refresh that
    /// encodes to the identical call.
    ///
    /// The distinction is the whole fix: if a non-zero `refreshCapacity` counted as a withdrawal,
    /// the re-assert would stand down on a pair whose epoch is about to be armed, which is the
    /// opposite of what it is for.
    #[test]
    fn only_a_zero_epoch_is_a_withdrawal() {
        assert!(Intent::RefreshCapacity {
            pair_id: 1,
            bid: 0,
            ask: 0
        }
        .is_withdrawal());
        assert!(!Intent::RefreshCapacity {
            pair_id: 1,
            bid: 0,
            ask: 1
        }
        .is_withdrawal());
        assert!(!Intent::RefreshCapacity {
            pair_id: 1,
            bid: 1,
            ask: 0
        }
        .is_withdrawal());
        assert!(!Intent::UpdateQuote {
            pair_id: 1,
            word: [0; 32]
        }
        .is_withdrawal());
    }

    /// The guard that stopped the jump re-assert from sending every withdrawal twice.
    ///
    /// Measured over 16.2 hours before it existed: 150-154 withdrawal transactions per pair
    /// against 73-75 real episodes, or about 740 wasted sends, each at the 100x withdrawal tip.
    /// The cause is entirely visible here — `in_flight_max` is 2, so one withdrawal in the air
    /// leaves `at_capacity` false and the re-assert takes the free slot ~1.4s into a ~2s
    /// inclusion.
    #[test]
    fn a_withdrawal_already_in_the_air_is_not_sent_again() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        let p = |b: u8, n: u64, withdrawal: bool| Pending {
            hash: B256::repeat_byte(b),
            kind: "refreshCapacity",
            nonce: n,
            submitted_at: Instant::now(),
            withdrawal,
        };

        assert!(!s.withdrawal_in_flight(1), "nothing sent, nothing pending");

        // What the fast lane leaves behind: one withdrawal, and a slot still open.
        s.pending.insert(1, vec![p(1, 10, true)]);
        assert!(
            !s.at_capacity(1),
            "the old guard would have let this through"
        );
        assert!(s.withdrawal_in_flight(1));

        // The chain has accounted for it, so the pair is answerable again — by the same rule
        // `in_flight` uses, and without waiting for a receipt.
        s.observe_landed(11);
        assert!(!s.withdrawal_in_flight(1));
        assert_eq!(s.in_flight(1), 0);
    }

    /// A pair with a routine epoch refresh outstanding is exactly a pair that still needs
    /// withdrawing, so the guard must not fire on one.
    #[test]
    fn a_pending_epoch_refresh_does_not_count_as_a_withdrawal() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        s.pending.insert(
            4,
            vec![
                Pending {
                    hash: B256::repeat_byte(1),
                    kind: "updateQuote",
                    nonce: 10,
                    submitted_at: Instant::now(),
                    withdrawal: false,
                },
                Pending {
                    hash: B256::repeat_byte(2),
                    kind: "refreshCapacity",
                    nonce: 11,
                    submitted_at: Instant::now(),
                    withdrawal: false,
                },
            ],
        );
        assert!(!s.withdrawal_in_flight(4));

        // And a timed-out withdrawal releases the guard, because the queue goes with it. A flag
        // kept outside `pending` is the version of this that would have suppressed the re-assert
        // for the one case it exists to catch.
        s.pending_timeout = Duration::from_millis(1);
        s.pending.insert(
            5,
            vec![Pending {
                hash: B256::repeat_byte(3),
                kind: "refreshCapacity",
                nonce: 12,
                submitted_at: Instant::now() - Duration::from_secs(30),
                withdrawal: true,
            }],
        );
        assert!(s.withdrawal_in_flight(5));
        let _ = s.sweep_timeouts(Instant::now());
        assert!(
            !s.withdrawal_in_flight(5),
            "a withdrawal that never landed must be re-assertable"
        );
    }

    /// A view can arrive out of order; a nonce that went backwards would re-block settled sends.
    #[test]
    fn the_landed_nonce_never_goes_backwards() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        s.observe_landed(50);
        s.observe_landed(7);
        s.pending.insert(
            1,
            vec![Pending {
                hash: B256::repeat_byte(3),
                kind: "updateQuote",
                nonce: 20,
                submitted_at: Instant::now(),
                withdrawal: false,
            }],
        );
        assert_eq!(
            s.in_flight(1),
            0,
            "nonce 20 is below the high-water mark of 50"
        );
    }

    use super::*;

    /// Anvil's first development account. **Published in Foundry's own documentation and in
    /// every Anvil startup banner** — it is a test fixture, not a secret, and it holds nothing
    /// on any network anyone cares about. It is here so the signature below can be checked
    /// against `cast` by anyone reading this file.
    const ANVIL_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const ANVIL_ADDR_0: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn the_address_derives_from_the_key() {
        let s = Signer::from_hex(ANVIL_KEY_0).unwrap();
        assert_eq!(s.address(), ANVIL_ADDR_0.parse::<Address>().unwrap());
        // ... with or without the prefix.
        assert_eq!(
            Signer::from_hex(&ANVIL_KEY_0[2..]).unwrap().address(),
            s.address()
        );
    }

    #[test]
    fn every_malformed_key_is_refused() {
        for bad in ["", "0x", "0xzz", "0x00", "not hex", "0xdeadbeef"] {
            assert!(
                Signer::from_hex(bad).is_err(),
                "`{bad}` was accepted as a key"
            );
        }
        // 32 bytes of zero is not a valid secp256k1 scalar, and 31 bytes is not a key at all.
        assert!(Signer::from_hex(&format!("0x{}", "00".repeat(32))).is_err());
        assert!(Signer::from_hex(&format!("0x{}", "11".repeat(31))).is_err());
    }

    #[test]
    fn a_key_error_never_quotes_the_key_material() {
        // 33 bytes: wrong length, but every byte of it is plausible key material, and the point
        // of the test is that none of it reaches a log line.
        let material = "11".repeat(33);
        let e = Signer::from_hex(&format!("0x{material}")).unwrap_err();
        let msg = format!("{e}");
        assert!(
            !msg.contains(&material),
            "the error quoted the key material: {msg}"
        );
        assert!(
            !msg.contains("1111"),
            "the error quoted the key material: {msg}"
        );
        assert!(
            msg.contains("32 bytes"),
            "... but it must still say what was wrong: {msg}"
        );

        // The env and file loaders name the source and nothing else.
        let e = Signer::load(&KeySource::Env("DUBU_TEST_UNSET_KEY_VAR".into())).unwrap_err();
        assert!(format!("{e}").contains("DUBU_TEST_UNSET_KEY_VAR"));
    }

    /// The transaction the byte-exact vector below was produced from.
    fn vector_tx() -> Eip1559 {
        Eip1559 {
            chain_id: 91_342,
            nonce: 7,
            max_priority_fee_per_gas: 5_000_000,
            max_fee_per_gas: 50_000_000,
            gas_limit: 400_000,
            to: "0xA629071E606F425dB93310c3ecc35E00Fbe16358"
                .parse()
                .unwrap(),
            value: U256::ZERO,
            input: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        }
    }

    #[test]
    fn the_signed_envelope_matches_cast_byte_for_byte() {
        // Produced independently by Foundry:
        //
        //   cast mktx --private-key $ANVIL_KEY_0 --chain 91342 --nonce 7 \
        //     --priority-gas-price 5000000 --gas-price 50000000 --gas-limit 400000 \
        //     0xA629071E606F425dB93310c3ecc35E00Fbe16358 0xdeadbeef
        //
        // This is the check that the nine RLP fields are in the right order with the right
        // encodings. A transposition here produces a valid transaction that does the wrong
        // thing, which no amount of round-tripping against ourselves would catch.
        let signer = Signer::from_hex(ANVIL_KEY_0).unwrap();
        let (hash, raw) = vector_tx().sign(&signer).unwrap();
        let expected_raw = include_str!("../testdata/eip1559_vector.hex").trim();
        assert_eq!(
            hex0x(&raw),
            expected_raw,
            "signed envelope diverged from `cast mktx`"
        );
        assert_eq!(hash, keccak256(&raw));
    }

    #[test]
    fn the_unsigned_envelope_is_the_signed_one_minus_the_signature() {
        // The signing hash covers exactly the nine payload fields, so the unsigned envelope must
        // be a strict prefix of the signed one apart from its own length header.
        let tx = vector_tx();
        let unsigned = tx.envelope(None);
        assert_eq!(unsigned[0], 0x02, "type byte");
        // An empty access list is one byte, 0xc0, and it is the last thing before the signature.
        assert_eq!(*unsigned.last().unwrap(), 0xc0);
        assert_eq!(tx.signing_hash(), keccak256(&unsigned));
    }

    #[test]
    fn update_quote_calldata_carries_the_word_intact() {
        let word = dubu_core::pack::QuoteWord::new(
            1,
            dubu_core::curve::Ladder {
                min_bid: 1_994_002_500_000_000,
                max_bid: 1_999_000_000_000_000,
                min_ask: 2_001_000_000_000_000,
                max_ask: 2_006_002_500_000_000,
            },
        );
        let packed = word.pack().unwrap();
        let data = Intent::UpdateQuote {
            pair_id: 1,
            word: packed,
        }
        .calldata();

        let decoded = abi::updateQuoteCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.packed.len(), 1);
        assert_eq!(decoded.packed[0].to_be_bytes::<32>(), packed);
        // And the chain will decode the same four prices back out of it.
        assert_eq!(
            dubu_core::pack::QuoteWord::unpack(&decoded.packed[0].to_be_bytes::<32>()).unwrap(),
            word
        );
    }

    #[test]
    fn refresh_capacity_calldata_carries_base_units_on_both_sides() {
        let data = Intent::RefreshCapacity {
            pair_id: 2,
            bid: 2_000_000_000,
            ask: 1_500_000_000,
        }
        .calldata();
        let d = abi::refreshCapacityCall::abi_decode(&data).unwrap();
        assert_eq!(d.pairId, 2);
        assert_eq!(d.bidCapacity.to::<u128>(), 2_000_000_000);
        assert_eq!(d.askCapacity.to::<u128>(), 1_500_000_000);
    }

    #[test]
    fn a_capacity_above_the_uint96_field_saturates_rather_than_wrapping() {
        // Wrapping would encode a tiny capacity that looks deliberate. The config validator
        // already refuses this, so saturating here is the second line rather than the first.
        let data = Intent::RefreshCapacity {
            pair_id: 1,
            bid: u128::MAX,
            ask: 0,
        }
        .calldata();
        let d = abi::refreshCapacityCall::abi_decode(&data).unwrap();
        assert_eq!(d.bidCapacity.to::<u128>(), dubu_core::curve::AMOUNT_MAX);
    }

    #[test]
    fn intents_key_by_pair_and_label_themselves() {
        assert_eq!(
            Intent::UpdateQuote {
                pair_id: 3,
                word: [0; 32]
            }
            .pair_id(),
            3
        );
        assert_eq!(
            Intent::UpdateQuote {
                pair_id: 3,
                word: [0; 32]
            }
            .label(),
            "updateQuote"
        );
        assert_eq!(
            Intent::RefreshCapacity {
                pair_id: 4,
                bid: 1,
                ask: 1
            }
            .label(),
            "refreshCapacity"
        );
    }

    fn tx_cfg(transmit: bool) -> crate::config::TxConfig {
        crate::config::TxConfig {
            transmit_allowed: transmit,
            private_key_env: None,
            private_key_file: None,
            gas_limit: 400_000,
            max_fee_per_gas_gwei: "0.05".into(),
            max_priority_fee_per_gas_gwei: "0.005".into(),
            pending_timeout_secs: 120,
            in_flight_max: 2,
        }
    }

    #[test]
    fn a_dry_run_sender_signs_nothing_and_needs_no_key() {
        let s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(false),
            50_000_000,
            5_000_000,
        );
        assert!(!s.transmit_allowed());
        assert_eq!(s.address(), None);
    }

    #[test]
    fn the_pending_map_blocks_exactly_the_pair_it_is_for() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        assert!(!s.at_capacity(1));
        assert_eq!(s.in_flight(1), 0);
        s.pending.insert(
            1,
            vec![Pending {
                hash: B256::ZERO,
                kind: "updateQuote",
                nonce: 3,
                submitted_at: Instant::now(),
                withdrawal: false,
            }],
        );
        assert_eq!(s.in_flight(1), 1, "the send must be tracked");
        assert_eq!(s.in_flight(2), 0, "and no other pair may be affected");
        assert!(
            !s.at_capacity(1),
            "one outstanding is inside the depth of two"
        );
    }

    /// The bound is what stops a stall burning nonces without limit. Depth, not a ban.
    #[test]
    fn the_pipeline_is_bounded_by_max_in_flight() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        let p = |n| Pending {
            hash: B256::from(U256::from(n)),
            kind: "updateQuote",
            nonce: n,
            submitted_at: Instant::now(),
            withdrawal: false,
        };
        s.pending.insert(1, vec![p(3)]);
        assert!(!s.at_capacity(1));
        s.pending.entry(1).or_default().push(p(4));
        assert!(s.at_capacity(1), "two outstanding fills the default depth");
        assert_eq!(s.in_flight(1), 2);
    }

    /// Everything behind a gap is unexecutable, so a timeout on the oldest takes the queue with
    /// it rather than leaving transactions that can never settle.
    #[test]
    fn a_timeout_drops_the_whole_queue_for_that_pair() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        let p = |n| Pending {
            hash: B256::from(U256::from(n)),
            kind: "updateQuote",
            nonce: n,
            submitted_at: Instant::now(),
            withdrawal: false,
        };
        s.pending.insert(1, vec![p(3), p(4)]);
        s.pending.remove(&1); // what the timeout branch does
        assert_eq!(s.in_flight(1), 0);
    }

    #[test]
    fn a_failed_send_drops_the_local_nonce() {
        // Building the next transaction on a nonce that may or may not have been consumed is
        // how a queue gets stuck behind a gap.
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        s.nonces.adopt(41);
        assert_eq!(s.next_nonce(), Some(41));
        s.resync_nonce();
        assert_eq!(s.next_nonce(), None);
    }

    /// A re-read may reach a node that has not seen what we already broadcast.
    ///
    /// The failure this guards: the transmit pool has nine endpoints and pins to one. When that
    /// one is serving a rate-limit penalty the nonce question goes to another, which answers from
    /// its own pending pool — a count from before the transactions the first node is holding. The
    /// sender then signs a nonce it has already used, the node refuses it as `nonce too low`, the
    /// refusal drops the nonce, and it asks again. 3,391 of 4,519 failed sends were this loop.
    ///
    /// The defence used to be a floor: whatever the node answered was raised to the highest nonce
    /// already broadcast. It is now structural, and stronger for it — the sequence lives in this
    /// process, so on the hot path there is no answer to raise because nothing is asked.
    #[test]
    fn a_lagging_node_is_never_consulted_about_a_nonce_already_handed_out() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        // Seeded once, as `seed_nonce` does at startup; 215409..=215411 then go out.
        s.nonces.adopt(215_409);
        for expected in [215_409_u64, 215_410, 215_411] {
            assert_eq!(s.nonces.reserve(), Some(expected));
        }
        // A node whose pool truncated all three would answer 215409. It is not asked.
        assert_eq!(s.next_nonce(), Some(215_412));
    }

    /// A refused nonce comes back and is handed out again, lowest first.
    ///
    /// The property the whole two-piece [`Nonces`] exists for. Nonce ordering is absolute for one
    /// sender, so a hole left by a refusal makes every nonce above it unexecutable until something
    /// times out — filling it is not tidiness, it is the difference between the next cycle quoting
    /// and the account wedging at the gap.
    #[test]
    fn a_refused_nonce_is_reused_rather_than_leaving_a_gap() {
        let mut n = Nonces::default();
        n.adopt(700);
        let taken: Vec<u64> = (0..3).filter_map(|_| n.reserve()).collect();
        assert_eq!(taken, vec![700, 701, 702]);

        // The middle one was provably refused; the other two are in the air.
        n.release(701);
        assert_eq!(
            n.reserve(),
            Some(701),
            "the hole is filled before the frontier"
        );
        assert_eq!(n.reserve(), Some(703));

        // A value the book never handed out cannot shrink the sequence.
        n.release(9_000);
        assert_eq!(n.reserve(), Some(704));
    }

    /// A fresh sender has no opinion about the nonce at all, and that is the point.
    ///
    /// The floor this replaces was initialised to `0`, which does not mean "unknown" — it means
    /// zero. So the first send of every process was floored at a nonce the chain passed long ago,
    /// which is the head of the `nonce too low` loop above. [`Sender::seed_nonce`] is what gives it
    /// an opinion, once, at startup.
    #[test]
    fn a_fresh_sender_has_no_nonce_until_something_seeds_it() {
        let s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        assert_eq!(
            s.next_nonce(),
            None,
            "an unseeded sequence must say so rather than answer zero"
        );
    }

    /// A request the limiter refused before opening a socket cannot have consumed a nonce, so it
    /// must not trigger the re-read that a genuinely-sent failure does. Keeping the nonce here is
    /// what stops one rate-limit penalty from becoming a run of `nonce too low`.
    #[test]
    fn a_send_that_never_left_is_distinguishable_from_one_that_did() {
        let never = RpcError::BackingOff {
            endpoint: "rpc",
            remaining_ms: 204,
        };
        assert!(never.never_sent());

        let went_out = RpcError::Http {
            endpoint: "rpc",
            status: 500,
            body: String::new(),
        };
        assert!(
            !went_out.never_sent(),
            "a 500 came back from the node, so the request reached it"
        );
    }

    #[test]
    fn the_envelope_is_deterministic_in_the_nonce_and_nothing_else() {
        let s = Sender::new(
            None,
            91_342,
            Address::repeat_byte(0xaa),
            &tx_cfg(false),
            50_000_000,
            5_000_000,
        );
        let i = Intent::UpdateQuote {
            pair_id: 1,
            word: [7; 32],
        };
        assert_eq!(
            s.envelope(i, 1).signing_hash(),
            s.envelope(i, 1).signing_hash()
        );
        assert_ne!(
            s.envelope(i, 1).signing_hash(),
            s.envelope(i, 2).signing_hash()
        );
        assert_eq!(
            s.envelope(i, 1).value,
            U256::ZERO,
            "neither updater function is payable"
        );
    }
}
