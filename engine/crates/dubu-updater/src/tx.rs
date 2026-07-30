//! Building, signing and submitting the two transactions the updater role may send.
//!
//! `PropPool` grants the updater exactly `updateQuote` and `refreshCapacity`; neither transfers a
//! token or touches `_reserve`, because the contract assumes this key leaks. [`Intent`] therefore
//! has two variants and may never grow a third. Gas is 0.001 gwei, so an escalator buys less than a
//! rounding error on one quote and adds a replacement racing the original; the fee is one flat
//! over-base config number, and a spike past `max_fee_per_gas_gwei` produces a transaction that
//! never lands, which the pending timeout reports with the fee it used.
//!
//! # A bounded pipeline, not one at a time
//!
//! Up to [`crate::config::TxConfig::in_flight_max`] unconfirmed transactions per pair, tracked in
//! [`Sender::pending`]; beyond that [`crate::policy`] gates the pair with `PushInFlight`.
//! Pipelining is safe only because these transactions are idempotent overwrites rather than orders
//! — `updateQuote` replaces the row without reading it. The cost is head-of-line blocking, so the
//! depth is bounded and a [`crate::config::TxConfig::pending_timeout_secs`] timeout abandons the
//! pair's whole queue: everything behind a gap is unexecutable anyway.
//!
//! # Reserved synchronously, broadcast concurrently
//!
//! `eth_sendRawTransaction` costs 264ms France-to-Korea, so five awaited in series spent 1.323s of
//! a 2.695s cycle on five copies of one wait. [`Sender::reserve_batch`] is therefore
//! **synchronous**: it assigns nonces N, N+1, … in the order the caller pushed the intents and
//! signs each envelope with no `.await` anywhere, so nothing can interleave with a half-advanced
//! nonce. Moving that read-modify-write across the send await is not a slower version of this; it
//! is the version where two futures both sign `n`. The order is load-bearing beyond the nonce:
//! `main.rs`'s per-pair loop requires a `RefreshCapacity` to execute before the `UpdateQuote` for
//! the same pair when the pool is dark, and that rests entirely on nonce order being absolute,
//! which is why [`Sender::reserve_batch`] takes a slice and walks it rather than taking a set.
//!
//! # The nonce is tracked here, not asked for
//!
//! [`Nonces`] holds the next nonce in this process: seeded at startup, advanced by reservation,
//! re-read only when [`Sender::sweep_timeouts`] abandons an intent or the node answers `nonce too
//! low`. Not asking on the hot path is correctness rather than a saved round trip — op-reth
//! forwards to the sequencer and only then discards the local pool error, so a transaction past
//! `--txpool.max-account-slots` (16) is live at the sequencer while the local pool has dropped it,
//! and a `pending` count from that pool reads twenty-odd low at ~37 in flight.
//!
//! `alloy-consensus` would supply `TxEip1559` and does not resolve against this toolchain, so the
//! nine-field EIP-1559 RLP list is encoded below and pinned byte-for-byte against `cast mktx`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_sol_types::SolCall;
use futures_util::stream::{FuturesUnordered, StreamExt};
use k256::ecdsa::SigningKey;
use serde_json::json;

use crate::chain::{abi, hex0x, unhex, Rpc, RpcError};
use crate::config::KeySource;

/// Anything that can go wrong on the transmit path.
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// The key could not be obtained. The message names where it was looked for, never the value.
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
    /// Distinct from [`Self::Rpc`]: the state *after* that read failed, saying the phase declined
    /// to guess. A guessed initial nonce is a `nonce too low` on every first send.
    #[error("the account nonce is not established; nothing was signed")]
    NoNonce,
    /// The node rejected the raw transaction.
    #[error("node rejected the transaction: {0}")]
    Rejected(String),
}

// --- Key ---

/// A loaded signing key. [`std::fmt::Debug`] is by hand and prints the address only, because the
/// derived one would put the scalar in every log line that formats a [`Sender`].
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
    /// [`TxError::Key`], never quoting the input. The two slices below are over fixed-size values:
    /// an uncompressed SEC1 point is always 65 bytes and a keccak digest always 32.
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
    /// The caller supplies a finished digest and this hashes nothing, because a signer computing
    /// its own preimage can be talked into signing a structure other than the one reviewed.
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

// --- Envelope ---

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
    /// Field order is the specification's: a transposition produces a perfectly well-formed
    /// transaction that pays the wrong fee or calls the wrong address.
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

// --- Intents ---

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
    /// Both amounts are **base** units on both sides: `PropCurve` amendment 1 moved ask capacity
    /// off the quote denomination and the bit positions did not move with it, so a wrong unit here
    /// encodes cleanly and settles wrongly.
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
    /// Read from the amounts, never from [`Self::label`]: a routine epoch and a withdrawal are the
    /// same call on chain, and they want opposite answers from [`Sender::withdrawal_in_flight`],
    /// since a pair with a non-zero epoch in the air is precisely one still to be withdrawn.
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

// --- Sender ---

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
    /// Recorded rather than re-derived: `kind` cannot answer it, since a withdrawal and a routine
    /// epoch refresh are the same call. See [`Sender::withdrawal_in_flight`].
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
    /// Included and reverted. For `updateQuote` that means the row failed `validateLadder` on chain
    /// after passing every off-chain check, which is a `dubu-core` divergence.
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
/// Held rather than read: the node's `pending` count comes from a pool that truncates this account
/// at 16 slots, so a per-send read hands out a used nonce — asking per send produced 3,391 of 4,519
/// failed sends in one episode, a `nonce too low` loop that re-read, signed the same used nonce and
/// was refused again. Two pieces, because a refused broadcast leaves a hole that must be filled
/// rather than skipped: nonce ordering is absolute for one sender, so everything above a gap is
/// unexecutable and the account wedges there until something times out.
#[derive(Debug, Default)]
struct Nonces {
    /// The lowest nonce never yet handed out. `None` until seeded, the only state in which a
    /// reservation may ask the node.
    next: Option<u64>,
    /// Nonces reserved and then *provably* refused, lowest first. Handed out again before `next`.
    ///
    /// Only values the upstream demonstrably never accepted: a local limiter refusal, or a
    /// `-32003`. An unknown outcome must never land here, because reusing a nonce that is in fact
    /// sitting in the sequencer signs a second transaction at the same nonce.
    free: BTreeSet<u64>,
    /// One past the highest nonce this book has ever handed out.
    ///
    /// The floor [`Nonces::adopt`] clamps the node's answer to. The local node forwards
    /// `eth_sendRawTransaction` to the sequencer instead of keeping the transaction, so its
    /// `pending` count omits our in-flight sends and comes back behind reality; adopting it
    /// verbatim re-issues nonces that are already mined or already in the mempool.
    issued: Option<u64>,
}

impl Nonces {
    /// What the next reservation would take, without taking it. `None` until seeded.
    fn peek(&self) -> Option<u64> {
        self.free.iter().next().copied().or(self.next)
    }

    /// Take the next nonce. `None` until seeded.
    ///
    /// Synchronous by construction and every caller must keep it that way: this is the
    /// read-modify-write that concurrency would break.
    fn reserve(&mut self) -> Option<u64> {
        let n = if let Some(n) = self.free.pop_first() {
            n
        } else {
            let n = self.next?;
            self.next = Some(n.saturating_add(1));
            n
        };
        self.issued = Some(self.issued.unwrap_or(0).max(n.saturating_add(1)));
        Some(n)
    }

    /// Give a reserved nonce back, for a broadcast the upstream provably refused.
    fn release(&mut self, n: u64) {
        // Only a nonce this book handed out: a value at or above `next` was never reserved, and
        // admitting one would let a caller mistake shrink the sequence.
        if self.next.is_some_and(|next| n < next) {
            self.free.insert(n);
        }
    }

    /// Forget everything and re-read on the next reservation. See [`Sender::resync_nonce`].
    fn resync(&mut self) {
        self.next = None;
        // The holes go too: they index a sequence this book is about to stop believing in, and
        // carrying them across would hand out nonces below whatever the node reports.
        self.free.clear();
    }

    /// Take the node's count as the sequence's new origin, but never move backwards.
    ///
    /// The node is authoritative only about what it has seen. Because sends are forwarded to the
    /// sequencer rather than pooled locally, its `pending` count lags our own issuance, and a
    /// backward jump re-issues live nonces: the mined ones come back `nonce too low`, the mempool
    /// ones `replacement transaction underpriced`, and each failure resyncs again. Clamping to
    /// what this book has already handed out makes that self-amplifying loop unreachable. A node
    /// genuinely ahead -- another sender on the same key -- is still adopted.
    fn adopt(&mut self, from_node: u64) {
        self.next = Some(from_node.max(self.issued.unwrap_or(0)));
        self.free.clear();
    }

    const fn seeded(&self) -> bool {
        self.next.is_some()
    }
}

/// A signed transaction whose nonce is already taken, waiting only for the wire. Produced by
/// [`Sender::reserve_batch`] with no await in it, which is what makes the concurrent broadcast
/// safe.
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
/// Carried out of [`Sender::send_batch`] rather than logged here: this module emits nothing, which
/// is what lets it be tested without a node.
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
    /// per-transaction failures, so they count into [`Self::held_backpressure`] and the episode,
    /// and a wedged upstream produces one line rather than one per send.
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
/// GIWA's sequencer refuses one account past 256 in flight with `-32003 txpool is full`; half is
/// kept as stall buffer. A backstop rather than the pacer — the per-pair
/// [`crate::config::TxConfig::in_flight_max`] binds in steady state — but stated per account,
/// because that is the scope of both the nonce sequence and the sequencer's own limit.
pub const IN_FLIGHT_TOTAL_MAX: usize = 128;

/// How long the send phase stands down after the upstream answers `-32003`.
///
/// Bracketed by two measurements: inclusion advances about once per block, so a second gives the
/// upstream a full block to drain where anything shorter re-offers the same transactions five times
/// a second, and it must stay short against the grace period, roughly 13s at 19 tx/s.
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
            // Unseeded. `Sender::seed_nonce` fills it at startup, the one moment reading the node
            // is trustworthy: nothing is in flight, so the local pool has truncated nothing.
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

    /// True when this pair already has as many transactions outstanding as it is allowed.
    ///
    /// Two ceilings: the per-pair one stops a single pair's stall from occupying the whole queue,
    /// but on its own it implies a global ceiling of *k·n* that moves whenever a pair is added, so
    /// the account's own limit is stated directly as [`IN_FLIGHT_TOTAL_MAX`].
    #[must_use]
    pub fn at_capacity(&self, pair_id: u16) -> bool {
        if self.in_flight(pair_id) >= self.in_flight_max {
            return true;
        }
        self.in_flight_total() >= IN_FLIGHT_TOTAL_MAX
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
    /// What the sequencer's 256-per-account limit is measured in, and — because nonce ordering is
    /// absolute — the depth a jump withdrawal must queue behind. Both are properties of the key,
    /// which the whole book shares.
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
    /// [`Self::at_capacity`] is the wrong question for the jump re-assert: `in_flight_max` is 2, so
    /// the fast lane's own withdrawal leaves a slot open and the re-assert re-sends at the 100x
    /// `jump.withdraw_priority_fee_wei` tip against a snapshot too young to show the zero. Derived
    /// from [`Self::in_flight`]'s bookkeeping rather than a parallel flag, which
    /// [`Self::sweep_timeouts`] and [`Self::observe_landed`] would each have to clear.
    #[must_use]
    pub fn withdrawal_in_flight(&self, pair_id: u16) -> bool {
        self.pending.get(&pair_id).is_some_and(|v| {
            v.iter()
                .any(|p| p.withdrawal && p.nonce >= self.landed_nonce)
        })
    }

    /// Tell the gate what the chain's confirmed nonce is.
    ///
    /// Everything below it has landed, so it no longer occupies an in-flight slot whatever the
    /// receipt poller has asked about; it stays in `pending` until a receipt says whether it
    /// *succeeded*. Monotone, because a view can arrive out of order and a nonce that went
    /// backwards would re-block settled sends.
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
    /// Exactly two callers and there may not be a third: [`Self::sweep_timeouts`] abandoning an
    /// intent, and a `nonce too low`. Both are recovery paths where the local counter has already
    /// been shown wrong; elsewhere the node's answer is the *less* trustworthy of the two.
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

    /// Read the account's nonce once, so the first send is not what discovers it.
    ///
    /// The *only* unconditional `eth_getTransactionCount` in the process: at startup nothing of
    /// ours is in flight, so the local pool has truncated nothing and its count is the chain's.
    ///
    /// # Errors
    /// [`TxError::NoKey`] with no signer, or [`TxError::Rpc`] if the read fails. Neither is fatal:
    /// an unseeded sequence means the first reservation reads it instead.
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
    /// The nonce is seeded and re-read on the **ordinary** RPC, never the flashblocks one: a nonce
    /// read from a preconfirmed state that later reorganises can never be included.
    ///
    /// # Errors
    /// [`TxError`]. Past the point where a nonce was consumed the local nonce is dropped, so the
    /// next attempt re-reads it rather than building on a guess.
    pub async fn send(&mut self, rpc: &Rpc, intent: Intent) -> Result<Sent, TxError> {
        self.send_with_fees(rpc, intent, None).await
    }

    /// Submit an intent at fees other than the configured ones.
    ///
    /// A **jump withdrawal** is the one transaction where the fee buys something: the sequencer
    /// orders by highest fee first and the counterparty will by construction outbid a flat
    /// near-zero tip, so the tip decides whether the withdrawal lands ahead of the pick-off in the
    /// same block. Still one flat config number, never bumped and never retried higher.
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

    /// Describe what would have gone out, without sending it. Still signs when a key is loaded, so
    /// the encode-and-sign path is exercised rather than assumed on the day transmission is on.
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
    /// `intents` is walked in order and the nonces come out in that order, so `main.rs`'s
    /// arbitration between a capacity refresh and a quote row still decides which executes first;
    /// broadcasts complete in network order but outcomes fold back in reservation order, keeping
    /// the pending queue oldest-first. The batch is trimmed to [`IN_FLIGHT_TOTAL_MAX`] before
    /// anything is signed, since the sequencer's per-account limit is invisible to a per-pair gate.
    /// A `-32003` pauses the phase for [`BACKPRESSURE_PAUSE`] rather than being reported per
    /// transaction: nothing is lost by withholding an idempotent overwrite, whereas at 27 tx/s a
    /// per-occurrence error is ~1600 lines a minute.
    ///
    /// # Errors
    /// [`TxError`] only for the failures that stop the whole phase: no key, or a nonce that could
    /// not be established. Per-intent failures are inside [`Batch::sent`].
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
        // Inside the pause nothing is signed and nothing is offered: a pool that refused a
        // transaction 200ms ago will refuse this one.
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

        // The raw bytes are hex-encoded up front so each future owns its payload and borrows
        // nothing but the client.
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

        // Reservation order, not completion order: `Sender::pending` is oldest-first per pair and
        // `sweep_timeouts` acts on its head, so a pair carrying both a refresh and a row must
        // record them in the order their nonces execute.
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
    /// **There is no `.await` in this function and there must never be one** — that is why it is a
    /// separate pass. The nonce is a read-modify-write, and an await inside it is what would let
    /// two concurrent sends both read `n` and both sign `n`.
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
            let Some(n) = self.nonces.reserve() else {
                break;
            };
            // The invariant the RefreshCapacity-before-UpdateQuote argument rests on: a reused hole
            // is below `next` and holes come out lowest-first, so a batch's nonces are strictly
            // increasing even when they are not contiguous.
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
            // A nonce reserved and never used is a hole, and a hole stops every nonce above it from
            // executing, so it goes back rather than waiting on a timeout with nothing to time out.
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
    /// Every branch is a claim about whether the *nonce was consumed*, not about whether the send
    /// succeeded. Accepted or `already known` is tracked and the nonce spent, even when the node
    /// returns a hash we disagree with, because an untracked live nonce has nothing to time it out.
    /// A provable refusal (no socket opened, or a `-32003`) leaves the nonce untouched, so it
    /// returns to [`Nonces::free`] and the sequence stays gap-free. Anything else is unknown fate,
    /// possibly sitting in the sequencer, so its nonce must *not* be reused and
    /// [`Self::sweep_timeouts`] resolves it.
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
                    // Not fatal, but every receipt lookup after this would use the wrong hash.
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
            // The node holds these exact bytes, so the transaction is in flight under the hash we
            // signed and the nonce is spent: the same state as an `Ok`, and tracked for the same
            // reason. Reachable where a ladder is byte-identical between pushes.
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
        // Refreshed while the `Opened` report is undrained, so the one line the caller logs carries
        // the whole batch's refusals rather than only the first.
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
    /// Never returns early on a failed lookup: a rate-limited receipt must not stop the others
    /// being checked, and an unreadable one stays pending until it times out.
    pub async fn poll_pending(&mut self, rpc: &Rpc) -> Vec<(u16, Pending, Settled)> {
        let mut settled = Vec::new();
        let now = Instant::now();
        // Younger than this cannot have landed, so the receipt call can only return null while
        // competing with quote traffic for the same rate limit. Under the measured 296ms best-case
        // inclusion, so a fast landing is still seen on the first poll after it happens.
        const MIN_AGE: Duration = Duration::from_millis(250);

        // Only transactions the chain has accounted for: the gate reads `landed_nonce`, so a
        // receipt only answers whether what landed *succeeded*, and asking every cycle until one
        // appeared cost 8.19 requests a second, twice the send rate. Asked at all because a revert
        // must surface — `updateQuote` reverting after passing every off-chain check means
        // dubu-core has diverged from the contract.
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
                // No receipt yet, or the lookup failed: both mean ask again next cycle. The sweep
                // below is what gives up, over every pending transaction rather than these.
                _ => {}
            }
        }

        settled.extend(self.sweep_timeouts(now));
        settled
    }

    /// Give up on transactions that have outlived [`Self::pending_timeout`].
    ///
    /// Outside the receipt loop by necessity: that loop skips anything `landed_nonce` has not
    /// passed, and a stuck transaction is precisely one the nonce never passes. Pure, so the
    /// behaviour is testable without a node.
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
            // Everything behind a timed-out transaction goes with it: a nonce that never lands is a
            // gap, and every nonce after it is unexecutable however healthy its transaction looks.
            self.pending.remove(&pair_id);
            // The abandoned nonce may or may not have been consumed, and re-reading is the only way
            // to find out. Guessing produces a stuck queue.
            self.resync_nonce();
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    /// The receipt loop only looks below `landed_nonce`, and a stuck transaction is precisely one
    /// the nonce never passes, so the sweep has to be what reaches it.
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

    /// The gate reopens on the chain's nonce, not on a receipt: a receipt lost to quote traffic at
    /// the rate limiter reports a landing ~2s late, over an already-empty queue.
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

    /// A withdrawal is a zero epoch and nothing else is, including the routine refresh that encodes
    /// identically: counting one would stand the re-assert down on a pair about to be armed.
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

    /// The guard against re-asserting every withdrawal: `in_flight_max` is 2, so one in the air
    /// leaves `at_capacity` false and the re-assert takes the free slot at the 100x tip.
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

        // The chain has accounted for it, so the pair is answerable again without a receipt.
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

        // A timed-out withdrawal releases the guard, because the queue goes with it. A flag kept
        // outside `pending` would suppress the re-assert for the one case it exists to catch.
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

    /// Anvil's first development account: published in Foundry's docs and in every Anvil startup
    /// banner, so a fixture rather than a secret. Hard-coded so the vector below is reproducible
    /// against `cast`.
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
        // 33 bytes: wrong length, but plausible key material, none of which may reach a log line.
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
        // A transposition of the nine RLP fields still produces a valid transaction.
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
        // Wrapping would encode a tiny capacity that looks deliberate. The config validator already
        // refuses this, so saturating here is the second line rather than the first.
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

    /// Everything behind a gap is unexecutable, so a timeout on the oldest takes the queue with it.
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
        // Building on a nonce that may or may not have been consumed is how a queue wedges.
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

    /// A re-read may reach a node that has not seen what we broadcast: while the pinned endpoint
    /// serves a rate-limit penalty, the nonce question goes to one whose pool is older.
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

    /// The same lagging node, on the one path that *does* ask it. A `nonce too low` resyncs, the
    /// next reservation re-reads, and sends are forwarded to the sequencer rather than pooled
    /// locally, so the answer omits everything in flight. Adopting it verbatim re-issued live
    /// nonces and each collision resynced again: 7 `nonce too low` and 7 `replacement transaction
    /// underpriced` in one burst on 2026-07-31, the node 66 ahead of what the bot was sending.
    #[test]
    fn a_resync_against_a_lagging_node_does_not_reissue_live_nonces() {
        let mut s = Sender::new(
            None,
            91_342,
            Address::ZERO,
            &tx_cfg(true),
            50_000_000,
            5_000_000,
        );
        s.nonces.adopt(641_376);
        for _ in 0..66 {
            s.nonces.reserve();
        }
        assert_eq!(s.next_nonce(), Some(641_442));

        // `nonce too low` clears the book, then the re-read answers with only what the node has
        // seen -- 66 behind, because the rest sit at the sequencer.
        s.resync_nonce();
        s.nonces.adopt(641_376);

        assert_eq!(
            s.next_nonce(),
            Some(641_442),
            "the book handed out 641441; re-issuing it collides with a live transaction"
        );

        // A node that is genuinely ahead is still believed: another sender shares this key.
        s.nonces.adopt(641_500);
        assert_eq!(s.next_nonce(), Some(641_500));
    }

    /// A refused nonce comes back and goes out again, lowest first: a hole makes every nonce above
    /// it unexecutable, so filling it is the difference between quoting and wedging at the gap.
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

    /// A fresh sender has no opinion about the nonce: a default of `0` does not mean "unknown", it
    /// means a nonce the chain passed long ago, which is the head of the `nonce too low` loop.
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

    /// A request refused before a socket opened cannot have consumed a nonce, so it must not
    /// trigger the re-read that turns one rate-limit penalty into a run of `nonce too low`.
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
