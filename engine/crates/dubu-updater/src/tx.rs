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
//! Up to [`crate::config::TxConfig::max_in_flight`] unconfirmed transactions per pair, tracked in
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
//! # Why the envelope is encoded here
//!
//! `alloy-consensus` would supply `TxEip1559`, and it does not currently resolve against this
//! toolchain — see the workspace manifest for the exact version deadlock. EIP-1559's payload is
//! a fully specified nine-field RLP list; it is encoded below and pinned byte-for-byte against
//! `cast mktx` output in the tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
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
        f.debug_struct("Signer").field("address", &self.address).finish_non_exhaustive()
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
            return Err(TxError::Key(format!("key must be 32 bytes, got {}", raw.len())));
        }
        let key = SigningKey::from_slice(&raw).map_err(|_| TxError::Key("key is not a valid secp256k1 scalar".into()))?;
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
                let v = std::env::var(name)
                    .map_err(|_| TxError::Key(format!("environment variable `{name}` is not set")))?;
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
        Ok((rec.to_byte() & 1 == 1, U256::from_be_slice(&sig.r().to_bytes()), U256::from_be_slice(&sig.s().to_bytes())))
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
        alloy_rlp::Header { list: true, payload_length: 0 }.encode(&mut fields);
        if let Some((y, r, s)) = sig {
            u8::from(y).encode(&mut fields);
            r.encode(&mut fields);
            s.encode(&mut fields);
        }

        let mut out = Vec::with_capacity(fields.len() + 8);
        out.push(0x02);
        alloy_rlp::Header { list: true, payload_length: fields.len() }.encode(&mut out);
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

    /// ABI-encoded calldata.
    ///
    /// # Panics
    /// Never: both amount fields are checked against the `uint96` domain by the caller and
    /// saturate here rather than wrapping.
    #[must_use]
    pub fn calldata(self) -> Bytes {
        match self {
            Self::UpdateQuote { word, .. } => {
                abi::updateQuoteCall { packed: vec![U256::from_be_bytes(word)] }.abi_encode().into()
            }
            Self::RefreshCapacity { pair_id, bid, ask } => abi::refreshCapacityCall {
                pairId: pair_id,
                bidCapacity: alloy_primitives::aliases::U96::from(bid.min(dubu_core::curve::MAX_AMOUNT)),
                askCapacity: alloy_primitives::aliases::U96::from(ask.min(dubu_core::curve::MAX_AMOUNT)),
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
    nonce: Option<u64>,
    /// Outstanding transactions per pair, oldest first. A `Vec` rather than one slot because the
    /// send path pipelines — see [`Sender::at_capacity`].
    pending: BTreeMap<u16, Vec<Pending>>,
    pending_timeout: Duration,
    max_in_flight: usize,
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
            nonce: None,
            pending: BTreeMap::new(),
            pending_timeout: Duration::from_secs(cfg.pending_timeout_secs),
            max_in_flight: cfg.max_in_flight,
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
    pub fn at_capacity(&self, pair_id: u16) -> bool {
        self.in_flight(pair_id) >= self.max_in_flight
    }

    /// How many transactions this pair has outstanding.
    #[must_use]
    pub fn in_flight(&self, pair_id: u16) -> usize {
        self.pending.get(&pair_id).map_or(0, Vec::len)
    }

    /// The pending transaction for a pair, if any.
    #[must_use]
    pub fn pending(&self, pair_id: u16) -> Option<&Pending> {
        // The oldest, which is the one a timeout will fire on first and the one whose fate
        // decides every transaction behind it.
        self.pending.get(&pair_id).and_then(|v| v.first())
    }

    /// Force the next send to re-read the nonce from the node.
    pub fn resync_nonce(&mut self) {
        self.nonce = None;
    }

    /// Build the envelope for an intent without signing or sending it, at the configured fees.
    #[must_use]
    pub fn envelope(&self, intent: Intent, nonce: u64) -> Eip1559 {
        self.envelope_with_fees(intent, nonce, None)
    }

    /// Build the envelope, optionally overriding the fees for this one transaction.
    #[must_use]
    pub fn envelope_with_fees(&self, intent: Intent, nonce: u64, fees: Option<Fees>) -> Eip1559 {
        let f = fees.unwrap_or(Fees { max_fee: self.max_fee, max_priority_fee: self.max_priority_fee });
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
            // Dry run still signs when a key happens to be loaded, so that the encode-and-sign
            // path is exercised rather than merely assumed to work on the day it is switched on.
            let would_be = match (&self.signer, self.nonce) {
                (Some(s), Some(n)) => Some(self.envelope_with_fees(intent, n, fees).sign(s)?.0),
                _ => None,
            };
            return Ok(Sent::DryRun {
                calldata: intent.calldata(),
                would_be_hash: would_be,
                would_be_nonce: self.nonce,
            });
        }

        let signer = self.signer.as_ref().ok_or(TxError::NoKey)?;
        let nonce = match self.nonce {
            Some(n) => n,
            None => {
                let n = rpc
                    .quantity("eth_getTransactionCount", json!([signer.address().to_string(), "pending"]))
                    .await?;
                self.nonce = Some(n);
                n
            }
        };

        let tx = self.envelope_with_fees(intent, nonce, fees);
        let (hash, raw) = tx.sign(signer)?;

        match rpc.call("eth_sendRawTransaction", json!([hex0x(&raw)])).await {
            Ok(v) => {
                let returned = v.as_str().unwrap_or_default();
                if !returned.is_empty() && returned != hash.to_string() {
                    // Not fatal, but it means the local hash and the node's disagree, and every
                    // receipt lookup after this would be against the wrong one.
                    return Err(TxError::Rejected(format!(
                        "node returned hash {returned}, locally computed {hash}"
                    )));
                }
                self.nonce = Some(nonce + 1);
                self.pending
                    .entry(intent.pair_id())
                    .or_default()
                    .push(Pending { hash, kind: intent.label(), nonce, submitted_at: Instant::now() });
                Ok(Sent::Broadcast { hash, nonce })
            }
            Err(e) => {
                self.resync_nonce();
                Err(TxError::Rpc(e))
            }
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
        let entries: Vec<(u16, Pending)> =
            self.pending.iter().flat_map(|(k, v)| v.iter().map(move |p| (*k, *p))).collect();

        for (pair_id, p) in entries {
            match rpc.call("eth_getTransactionReceipt", json!([p.hash.to_string()])).await {
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
                        if ok { Settled::Confirmed { block } } else { Settled::Reverted { block } },
                    ));
                    self.forget(pair_id, p.hash);
                }
                // Either no receipt yet, or the lookup failed. Both mean "still pending", and
                // both are subject to the timeout below.
                _ => {
                    let waited = now.saturating_duration_since(p.submitted_at);
                    if waited >= self.pending_timeout {
                        settled.push((pair_id, p, Settled::TimedOut { waited_secs: waited.as_secs() }));
                        // Everything behind a timed-out transaction is dropped with it, not just
                        // the one that expired. If nonce `k` never lands there is a gap, and every
                        // nonce after it is unexecutable however healthy its own transaction looks
                        // — tracking those would be tracking transactions that cannot settle.
                        self.pending.remove(&pair_id);
                        // The abandoned nonce may or may not have been consumed. Re-reading is
                        // the only way to find out, and guessing produces a stuck queue.
                        self.resync_nonce();
                    }
                }
            }
        }
        settled
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(Signer::from_hex(&ANVIL_KEY_0[2..]).unwrap().address(), s.address());
    }

    #[test]
    fn every_malformed_key_is_refused() {
        for bad in ["", "0x", "0xzz", "0x00", "not hex", "0xdeadbeef"] {
            assert!(Signer::from_hex(bad).is_err(), "`{bad}` was accepted as a key");
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
        assert!(!msg.contains(&material), "the error quoted the key material: {msg}");
        assert!(!msg.contains("1111"), "the error quoted the key material: {msg}");
        assert!(msg.contains("32 bytes"), "... but it must still say what was wrong: {msg}");

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
            to: "0xA629071E606F425dB93310c3ecc35E00Fbe16358".parse().unwrap(),
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
        assert_eq!(hex0x(&raw), expected_raw, "signed envelope diverged from `cast mktx`");
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
        let data = Intent::UpdateQuote { pair_id: 1, word: packed }.calldata();

        let decoded = abi::updateQuoteCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.packed.len(), 1);
        assert_eq!(decoded.packed[0].to_be_bytes::<32>(), packed);
        // And the chain will decode the same four prices back out of it.
        assert_eq!(dubu_core::pack::QuoteWord::unpack(&decoded.packed[0].to_be_bytes::<32>()).unwrap(), word);
    }

    #[test]
    fn refresh_capacity_calldata_carries_base_units_on_both_sides() {
        let data = Intent::RefreshCapacity { pair_id: 2, bid: 2_000_000_000, ask: 1_500_000_000 }.calldata();
        let d = abi::refreshCapacityCall::abi_decode(&data).unwrap();
        assert_eq!(d.pairId, 2);
        assert_eq!(d.bidCapacity.to::<u128>(), 2_000_000_000);
        assert_eq!(d.askCapacity.to::<u128>(), 1_500_000_000);
    }

    #[test]
    fn a_capacity_above_the_uint96_field_saturates_rather_than_wrapping() {
        // Wrapping would encode a tiny capacity that looks deliberate. The config validator
        // already refuses this, so saturating here is the second line rather than the first.
        let data = Intent::RefreshCapacity { pair_id: 1, bid: u128::MAX, ask: 0 }.calldata();
        let d = abi::refreshCapacityCall::abi_decode(&data).unwrap();
        assert_eq!(d.bidCapacity.to::<u128>(), dubu_core::curve::MAX_AMOUNT);
    }

    #[test]
    fn intents_key_by_pair_and_label_themselves() {
        assert_eq!(Intent::UpdateQuote { pair_id: 3, word: [0; 32] }.pair_id(), 3);
        assert_eq!(Intent::UpdateQuote { pair_id: 3, word: [0; 32] }.label(), "updateQuote");
        assert_eq!(Intent::RefreshCapacity { pair_id: 4, bid: 1, ask: 1 }.label(), "refreshCapacity");
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
            max_in_flight: 2,
        }
    }

    #[test]
    fn a_dry_run_sender_signs_nothing_and_needs_no_key() {
        let s = Sender::new(None, 91_342, Address::ZERO, &tx_cfg(false), 50_000_000, 5_000_000);
        assert!(!s.transmit_allowed());
        assert_eq!(s.address(), None);
    }

    #[test]
    fn the_pending_map_blocks_exactly_the_pair_it_is_for() {
        let mut s = Sender::new(None, 91_342, Address::ZERO, &tx_cfg(true), 50_000_000, 5_000_000);
        assert!(!s.at_capacity(1));
        assert_eq!(s.in_flight(1), 0);
        s.pending.insert(
            1,
            vec![Pending { hash: B256::ZERO, kind: "updateQuote", nonce: 3, submitted_at: Instant::now() }],
        );
        assert_eq!(s.in_flight(1), 1, "the send must be tracked");
        assert_eq!(s.in_flight(2), 0, "and no other pair may be affected");
        assert!(!s.at_capacity(1), "one outstanding is inside the depth of two");
    }

    /// The bound is what stops a stall burning nonces without limit. Depth, not a ban.
    #[test]
    fn the_pipeline_is_bounded_by_max_in_flight() {
        let mut s = Sender::new(None, 91_342, Address::ZERO, &tx_cfg(true), 50_000_000, 5_000_000);
        let p = |n| Pending { hash: B256::from(U256::from(n)), kind: "updateQuote", nonce: n, submitted_at: Instant::now() };
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
        let mut s = Sender::new(None, 91_342, Address::ZERO, &tx_cfg(true), 50_000_000, 5_000_000);
        let p = |n| Pending { hash: B256::from(U256::from(n)), kind: "updateQuote", nonce: n, submitted_at: Instant::now() };
        s.pending.insert(1, vec![p(3), p(4)]);
        s.pending.remove(&1); // what the timeout branch does
        assert_eq!(s.in_flight(1), 0);
    }

    #[test]
    fn a_failed_send_drops_the_local_nonce() {
        // Building the next transaction on a nonce that may or may not have been consumed is
        // how a queue gets stuck behind a gap.
        let mut s = Sender::new(None, 91_342, Address::ZERO, &tx_cfg(true), 50_000_000, 5_000_000);
        s.nonce = Some(41);
        s.resync_nonce();
        assert_eq!(s.nonce, None);
    }

    #[test]
    fn the_envelope_is_deterministic_in_the_nonce_and_nothing_else() {
        let s = Sender::new(None, 91_342, Address::repeat_byte(0xaa), &tx_cfg(false), 50_000_000, 5_000_000);
        let i = Intent::UpdateQuote { pair_id: 1, word: [7; 32] };
        assert_eq!(s.envelope(i, 1).signing_hash(), s.envelope(i, 1).signing_hash());
        assert_ne!(s.envelope(i, 1).signing_hash(), s.envelope(i, 2).signing_hash());
        assert_eq!(s.envelope(i, 1).value, U256::ZERO, "neither updater function is payable");
    }
}
