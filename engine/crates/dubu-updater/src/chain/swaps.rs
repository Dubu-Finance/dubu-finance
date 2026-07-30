//! Reading our own fills off the chain. [`crate::markout`] is the accounting; this is its tap.
//!
//! Polled rather than subscribed: `eth_getLogs` over an explicit block range is replayable, so a
//! failed poll leaves the cursor put and the next poll re-reads the range, where a missed websocket
//! frame is a silently missing fill. Markout's horizons are anchored to the fill's *block*
//! timestamp, so a fill discovered a cycle late marks identically to one discovered instantly. The
//! cursor tracks the unsafe head, because waiting for finality would put fills minutes behind their
//! own sixty-second horizon; the reorg exposure that buys is bounded by [`OVERLAP_BLOCKS`] of
//! re-read range deduplicated on `(transactionHash, logIndex)`, and by
//! [`crate::markout::Score::is_actionable`] requiring both a fill count and a notional floor. Not
//! handled: a log removed after we counted it, since a reorg between two polls leaves no marker.
//!
//! Past [`MAX_LOOKBACK_BLOCKS`] behind the head the watcher jumps forward and counts a gap rather
//! than grinding through the backlog: those fills would be marked against reference history
//! `markout::REF_RETENTION_SECS` has already retired, and quoting must not stall behind a
//! measurement.

use std::collections::{HashSet, VecDeque};

use alloy_primitives::{Address, B256};
use alloy_sol_types::{sol, SolEvent};
use serde_json::json;

use super::{Rpc, RpcError};

sol! {
    #[derive(Debug)]
    event Swap(
        uint16 indexed pairId,
        address indexed sender,
        address indexed receiver,
        bool isBid,
        uint256 amountIn,
        uint256 amountOut,
        uint256 partnerId
    );
}

/// Blocks re-read on every poll, below the cursor.
///
/// Covers a log that arrives late or moves block after a sequencer reorg; the dedup ring discards
/// the repeats before `markout` sees them. GIWA blocks are one second, so this is two seconds of
/// overlap, kept small because a poll requests `cursor - OVERLAP ..= head` and so spends it out of
/// the [`GETLOGS_RANGE_CAP_MIN`] budget.
pub const OVERLAP_BLOCKS: u64 = 2;

/// Largest span requested in one `eth_getLogs` call.
///
/// Endpoints cap the range without advertising it and answer an over-wide request with an error
/// rather than a truncated result, so a wider poll is split into several calls. Ten is
/// [`GETLOGS_RANGE_CAP_MIN`]; anything above it fails only when the read pool rotates onto the
/// tightest endpoint, which reads as a scan that half-works.
pub const MAX_RANGE_BLOCKS: u64 = 10;

/// Chunks one poll will request before leaving the rest to the next one.
///
/// Ten-block chunks turn a six-hundred-block gap into sixty sequential round trips, stalling a
/// 330ms cycle for seconds. The cursor persists between polls, so a bounded budget drains the gap
/// over several cycles and the read pool sees a trickle rather than a burst it rate-limits.
pub const CHUNKS_PER_POLL_MAX: u32 = 8;

/// The tightest `eth_getLogs` range cap any endpoint in the read pool imposes.
///
/// Nodit's free tier, measured: eleven blocks returns `-32604`, ten does not. A fact about the
/// environment rather than a knob, so anything else batching a range read respects the same
/// ceiling.
pub const GETLOGS_RANGE_CAP_MIN: u64 = 10;

// Compile-time rather than a test: the two constants disagreeing is the whole defect, and it is
// invisible at runtime until the read pool rotates onto the tightest endpoint.
const _: () = assert!(
    MAX_RANGE_BLOCKS <= GETLOGS_RANGE_CAP_MIN,
    "a chunk wider than the tightest cap fails whenever the read pool rotates onto that endpoint"
);
const _: () = assert!(
    OVERLAP_BLOCKS < MAX_RANGE_BLOCKS,
    "the overlap must fit inside one chunk or no range is ever readable"
);

/// Where a poll starts reading, and what it gave up on to get there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePlan {
    /// First block this poll will request.
    pub first: u64,
    /// Blocks abandoned because the cursor had fallen further behind than [`MAX_LOOKBACK_BLOCKS`].
    pub skipped: u64,
}

/// Where to start reading, given the cursor and the head.
///
/// Split out of [`SwapWatch::poll`] so the range arithmetic is testable without a live [`Rpc`].
#[must_use]
pub const fn plan_ranges(cursor: u64, head: u64) -> RangePlan {
    let from = cursor.saturating_sub(OVERLAP_BLOCKS);
    if head.saturating_sub(from) > MAX_LOOKBACK_BLOCKS {
        let jump = head.saturating_sub(MAX_LOOKBACK_BLOCKS);
        return RangePlan {
            first: jump,
            skipped: jump.saturating_sub(from),
        };
    }
    RangePlan {
        first: from,
        skipped: 0,
    }
}

/// How far behind the head the cursor may fall before the watcher gives up and jumps forward.
///
/// At one-second blocks this is about ten minutes, well past the point where the reference history
/// needed to mark those fills has been retired.
pub const MAX_LOOKBACK_BLOCKS: u64 = 600;

/// Dedup ring capacity, in log identities.
const SEEN_CAPACITY: usize = 4096;

/// One `Swap`, decoded, with the chain coordinates that identify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapLog {
    /// Which market the fill was against.
    pub pair_id: u16,
    /// Who called `swap`. For routed flow this is the adapter, which is why it is not what
    /// `markout` scores.
    pub sender: Address,
    /// Who received the output. This is the counterparty `markout` scores.
    pub receiver: Address,
    /// True when the pool bought base and paid quote.
    pub is_bid: bool,
    /// What the taker paid, in the input token's own units.
    pub amount_in: u128,
    /// What the pool paid out, in the output token's own units.
    pub amount_out: u128,
    /// The routing-source tag the caller supplied about itself.
    pub partner_id: u128,
    /// Block the fill landed in.
    pub block_number: u64,
    /// That block's timestamp. Every markout horizon is measured from here, never from when the
    /// poll noticed the fill. Zero only between decoding and [`SwapWatch::resolve_timestamps`].
    pub at_secs: u64,
    /// Transaction that contained it.
    pub tx_hash: B256,
    /// Position within the block's logs. With `tx_hash`, this identifies the log.
    pub log_index: u64,
}

/// A poll's worth of fills, plus what the poll could not account for.
#[derive(Debug, Default)]
pub struct Polled {
    /// Fills not seen before, oldest first.
    pub fills: Vec<SwapLog>,
    /// Logs discarded as already seen. Expected and harmless — this is the overlap working.
    pub duplicates: u64,
    /// Logs the filter returned that did not decode, or whose amounts exceed the engine's
    /// 128-bit domain. Non-zero means the chain and the engine disagree about the event and
    /// wants investigating; it is never a rounding matter.
    pub undecodable: u64,
    /// Logs flagged `removed` by the node, from a range that reorged while we were reading it.
    pub removed: u64,
    /// Fills dropped because their block's timestamp could not be established. See
    /// [`SwapWatch::resolve_timestamps`].
    pub unresolved: u64,
    /// Chunks this poll gave up on. At most one: the poll stops at the first refusal so the fills
    /// it already holds survive, and the cursor is left pointing at the range that failed.
    pub unread_chunks: u64,
    /// Blocks still between the cursor and the head when the poll ran out of its chunk budget.
    ///
    /// Zero on a poll that caught up; non-zero is the scan draining a gap, which is normal after a
    /// restart and a fault if it does not fall. Reported because a cursor quietly behind reads
    /// exactly like a chain with nothing happening on it.
    pub behind_blocks: u64,
}

/// Follows `Swap` logs from one pool address.
#[derive(Debug)]
pub struct SwapWatch {
    pool: Address,
    /// Highest block scanned. `None` until the first poll picks a starting point.
    cursor: Option<u64>,
    seen: VecDeque<(B256, u64)>,
    seen_set: HashSet<(B256, u64)>,
    gaps: u64,
    skipped_blocks: u64,
    undecodable: u64,
}

/// Where a log query ends.
///
/// `Pending` is the flashblocks tag: preconfirmed logs, ~200ms after submission against a second
/// for a sealed block. Only ever used on the path that does not advance the cursor; see
/// [`SwapWatch::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockBound {
    Number(u64),
    Pending,
}

impl BlockBound {
    fn tag(self) -> serde_json::Value {
        match self {
            Self::Number(n) => serde_json::Value::String(format!("0x{n:x}")),
            Self::Pending => serde_json::Value::String("pending".into()),
        }
    }
}

impl SwapWatch {
    /// A watcher over `pool`, positioned nowhere until its first poll.
    pub fn new(pool: Address) -> Self {
        Self {
            pool,
            cursor: None,
            seen: VecDeque::with_capacity(SEEN_CAPACITY),
            seen_set: HashSet::with_capacity(SEEN_CAPACITY),
            gaps: 0,
            skipped_blocks: 0,
            undecodable: 0,
        }
    }

    /// Highest block scanned, or `None` before the first poll.
    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    /// How many times the watcher fell far enough behind to jump forward.
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Total blocks skipped over by those jumps — the size of the hole in the record.
    pub fn skipped_blocks(&self) -> u64 {
        self.skipped_blocks
    }

    /// Running total of logs that failed to decode.
    pub fn undecodable(&self) -> u64 {
        self.undecodable
    }

    /// Read every `Swap` between the cursor and `head`.
    ///
    /// The first call starts at `head`, not at the pool's deployment: history predating the process
    /// was quoted by a different configuration, so backfilling it would mix two experiments. The
    /// cursor only advances over ranges that were read, so a failed call is re-read rather than
    /// skipped.
    pub async fn poll(&mut self, rpc: &Rpc, head: u64) -> Result<Polled, RpcError> {
        let Some(cursor) = self.cursor else {
            self.cursor = Some(head);
            return Ok(Polled::default());
        };
        if head <= cursor.saturating_sub(OVERLAP_BLOCKS) {
            return Ok(Polled::default());
        }

        let plan = plan_ranges(cursor, head);
        if plan.skipped > 0 {
            self.gaps += 1;
            self.skipped_blocks += plan.skipped;
        }

        let mut from = plan.first;
        let mut out = Polled::default();
        let mut chunks = 0;
        while from <= head && chunks < CHUNKS_PER_POLL_MAX {
            let to = head.min(from + MAX_RANGE_BLOCKS - 1);

            // A failed chunk ends the poll and keeps the chunks before it: `absorb` has already
            // marked those fills seen, so returning `Err` would discard the only copy of them the
            // dedup ring will ever admit. Safe because the cursor stays put for the failed range.
            let Ok(logs) = self.fetch(rpc, from, BlockBound::Number(to)).await else {
                out.unread_chunks += 1;
                return Ok(out);
            };
            self.absorb(&logs, &mut out);
            // Only after the range is in hand, so a failure repeats it rather than skipping it.
            self.cursor = Some(to);
            from = to + 1;
            chunks += 1;
        }
        if from <= head {
            out.behind_blocks = head.saturating_sub(from).saturating_add(1);
        }

        // Preconfirmed fills, ahead of the cursor and deliberately not advancing it: a fill is the
        // only signal that an informed counterparty traded against us, and one per sealed block is
        // two or three quotes late at a 415ms cadence. The cursor stays on sealed blocks because a
        // preconfirmed block's contents are not final; the sealed pass re-covers this range and
        // `seen` absorbs the duplicate.
        let preconf = self
            .fetch(rpc, head.saturating_add(1), BlockBound::Pending)
            .await?;
        self.absorb(&preconf, &mut out);
        self.resolve_timestamps(rpc, &mut out).await?;
        out.fills.sort_by_key(|f| (f.block_number, f.log_index));
        Ok(out)
    }

    /// Gives every fill the timestamp of the block it landed in — the field every markout horizon
    /// is measured from.
    ///
    /// Free when the node supplies `blockTimestamp`; otherwise one `eth_getBlockByNumber` per
    /// distinct block that produced a *new* fill, so dedup already bounds the call count. Never
    /// extrapolated from the head's timestamp and a block delta: block time is a target rather than
    /// a guarantee, and drift would push fills past `reference_at`'s tolerance and mark them
    /// against the wrong price. A block whose timestamp cannot be established drops its fills and
    /// counts them. The one index is on the `Ok(i)` arm of a `binary_search_by_key` over the vector
    /// being indexed.
    #[allow(clippy::indexing_slicing)]
    async fn resolve_timestamps(&self, rpc: &Rpc, out: &mut Polled) -> Result<(), RpcError> {
        let mut wanted: Vec<u64> = out
            .fills
            .iter()
            .filter(|f| f.at_secs == 0)
            .map(|f| f.block_number)
            .collect();
        wanted.sort_unstable();
        wanted.dedup();

        let mut found: Vec<(u64, u64)> = Vec::with_capacity(wanted.len());
        for block in wanted {
            let raw = rpc
                .call(
                    "eth_getBlockByNumber",
                    json!([format!("0x{block:x}"), false]),
                )
                .await?;
            if let Some(ts) = raw
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .and_then(hex_u64)
            {
                found.push((block, ts));
            }
        }

        let before = out.fills.len();
        out.fills.retain_mut(|f| {
            if f.at_secs != 0 {
                return true;
            }
            match found.binary_search_by_key(&f.block_number, |&(b, _)| b) {
                Ok(i) => {
                    f.at_secs = found[i].1;
                    true
                }
                Err(_) => false,
            }
        });
        out.unresolved += (before - out.fills.len()) as u64;
        Ok(())
    }

    async fn fetch(
        &self,
        rpc: &Rpc,
        from: u64,
        to: BlockBound,
    ) -> Result<Vec<serde_json::Value>, RpcError> {
        let filter = json!({
            "fromBlock": format!("0x{from:x}"),
            "toBlock": to.tag(),
            "address": self.pool.to_string(),
            "topics": [format!("{:#x}", Swap::SIGNATURE_HASH)],
        });
        let raw = rpc.call("eth_getLogs", json!([filter])).await?;
        Ok(raw.as_array().cloned().unwrap_or_default())
    }

    fn absorb(&mut self, logs: &[serde_json::Value], out: &mut Polled) {
        for log in logs {
            if log
                .get("removed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                out.removed += 1;
                continue;
            }
            let Some(decoded) = decode(log) else {
                out.undecodable += 1;
                self.undecodable += 1;
                continue;
            };
            let key = (decoded.tx_hash, decoded.log_index);
            if !self.remember(key) {
                out.duplicates += 1;
                continue;
            }
            out.fills.push(decoded);
        }
    }

    /// Records a log identity. Returns `false` if it was already there.
    fn remember(&mut self, key: (B256, u64)) -> bool {
        if !self.seen_set.insert(key) {
            return false;
        }
        self.seen.push_back(key);
        while self.seen.len() > SEEN_CAPACITY {
            if let Some(old) = self.seen.pop_front() {
                self.seen_set.remove(&old);
            }
        }
        true
    }
}

/// Decodes one log object from an `eth_getLogs` response.
///
/// `None` covers both a malformed response and an event the engine cannot represent: amounts are
/// `uint256` on chain and `u128` here, and `PropCurve` narrows outputs to the same bound, so an
/// amount that does not fit is a chain/engine divergence rather than a value to truncate into a
/// plausible markout for a trade that did not happen.
fn decode(log: &serde_json::Value) -> Option<SwapLog> {
    let topics: Vec<B256> = log
        .get("topics")?
        .as_array()?
        .iter()
        .map(|t| t.as_str()?.parse().ok())
        .collect::<Option<_>>()?;
    let data = hex_bytes(
        log.get("data")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0x"),
    )?;

    let ev = Swap::decode_raw_log_validate(topics, &data).ok()?;

    Some(SwapLog {
        pair_id: ev.pairId,
        sender: ev.sender,
        receiver: ev.receiver,
        is_bid: ev.isBid,
        amount_in: u128::try_from(ev.amountIn).ok()?,
        amount_out: u128::try_from(ev.amountOut).ok()?,
        partner_id: u128::try_from(ev.partnerId).ok()?,
        block_number: hex_u64(log.get("blockNumber")?.as_str()?)?,
        // Absent on nodes that predate the field; `resolve_timestamps` fills those in. Zero is
        // the sentinel rather than an `Option` because no real block carries timestamp zero and
        // the resolved struct should have one shape, not two.
        at_secs: log
            .get("blockTimestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(hex_u64)
            .unwrap_or(0),
        tx_hash: log.get("transactionHash")?.as_str()?.parse().ok()?,
        log_index: hex_u64(log.get("logIndex")?.as_str()?)?,
    })
}

fn hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let body = s.strip_prefix("0x").unwrap_or(s);
    if body.len() % 2 != 0 {
        return None;
    }
    (0..body.len() / 2)
        .map(|i| u8::from_str_radix(&body[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, hex, U256};
    use alloy_sol_types::SolValue;

    /// The ranges one poll would request, walking the same loop `poll` does.
    fn chunks_of(cursor: u64, head: u64) -> Vec<(u64, u64)> {
        let mut from = plan_ranges(cursor, head).first;
        let mut out = Vec::new();
        while from <= head && u32::try_from(out.len()).unwrap_or(u32::MAX) < CHUNKS_PER_POLL_MAX {
            let to = head.min(from + MAX_RANGE_BLOCKS - 1);
            out.push((from, to));
            from = to + 1;
        }
        out
    }

    #[test]
    fn a_poll_never_requests_a_range_wider_than_one_chunk() {
        for (cursor, head) in [(100, 101), (100, 130), (100, 900), (0, 5)] {
            for (from, to) in chunks_of(cursor, head) {
                assert!(
                    to.saturating_sub(from) < MAX_RANGE_BLOCKS,
                    "range {from}..={to} spans more blocks than one chunk may request"
                );
            }
        }
    }

    /// A gap wider than one poll's budget must be drained in bounded bites: asking for the whole
    /// gap is refused every time, and the cursor only advances over ranges that were read.
    #[test]
    fn a_gap_too_large_for_one_poll_is_drained_rather_than_retried_whole() {
        let head = 31_978_540;
        let mut cursor = head - 792;

        let first = chunks_of(cursor, head);
        assert_eq!(
            first.len(),
            CHUNKS_PER_POLL_MAX as usize,
            "a large gap should spend the whole chunk budget"
        );

        // Advance as `poll` does, and confirm the gap actually closes.
        let mut polls = 0;
        while cursor < head {
            let taken = chunks_of(cursor, head);
            assert!(
                !taken.is_empty(),
                "a poll behind the head must read something"
            );
            cursor = taken.last().expect("non-empty").1;
            polls += 1;
            assert!(polls < 100, "the gap is not draining");
        }
        assert_eq!(cursor, head, "the scan catches up exactly");
    }

    /// Past the lookback limit the scan gives up on the tail rather than reading forever, and says
    /// how much it abandoned.
    #[test]
    fn a_gap_past_the_lookback_limit_jumps_forward_and_counts_what_it_skipped() {
        let head = 1_000_000;
        let plan = plan_ranges(head - 5_000, head);
        assert_eq!(plan.first, head - MAX_LOOKBACK_BLOCKS);
        assert_eq!(plan.skipped, 5_000 - MAX_LOOKBACK_BLOCKS + OVERLAP_BLOCKS);

        let near = plan_ranges(head - 5, head);
        assert_eq!(near.skipped, 0, "a small gap skips nothing");
        assert_eq!(near.first, head - 5 - OVERLAP_BLOCKS);
    }

    fn pool() -> Address {
        address!("00000000000000000000000000000000000000AA")
    }

    fn topic_addr(a: Address) -> String {
        format!("0x{:0>64}", hex::encode(a.as_slice()))
    }

    /// One `Swap` as an RPC would report it. Defaults are a plausible bid; each test overrides only
    /// the field it is about.
    struct Spec {
        pair_id: u16,
        sender: Address,
        receiver: Address,
        is_bid: bool,
        amount_in: U256,
        amount_out: U256,
        block: u64,
        tx: B256,
        index: u64,
    }

    impl Default for Spec {
        fn default() -> Self {
            Self {
                pair_id: 3,
                sender: address!("00000000000000000000000000000000000000B1"),
                receiver: address!("00000000000000000000000000000000000000C2"),
                is_bid: true,
                amount_in: U256::from(1_000u64),
                amount_out: U256::from(2_000u64),
                block: 42,
                tx: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
                index: 5,
            }
        }
    }

    fn raw_log(s: Spec) -> serde_json::Value {
        let data = (s.is_bid, s.amount_in, s.amount_out, U256::from(7u64)).abi_encode_sequence();
        json!({
            "removed": false,
            "topics": [
                format!("{:#x}", Swap::SIGNATURE_HASH),
                format!("0x{:0>64x}", s.pair_id),
                topic_addr(s.sender),
                topic_addr(s.receiver),
            ],
            "data": format!("0x{}", hex::encode(&data)),
            "blockNumber": format!("0x{:x}", s.block),
            "blockTimestamp": format!("0x{:x}", 1_700_000_000u64 + s.block),
            "transactionHash": format!("{:#x}", s.tx),
            "logIndex": format!("0x{:x}", s.index),
        })
    }

    fn one(block: u64, index: u64) -> serde_json::Value {
        raw_log(Spec {
            block,
            index,
            ..Spec::default()
        })
    }

    #[test]
    fn decodes_every_field_off_the_wire() {
        let f = decode(&one(42, 5)).expect("well-formed log");
        assert_eq!(f.pair_id, 3);
        assert_eq!(
            f.sender,
            address!("00000000000000000000000000000000000000B1")
        );
        assert_eq!(
            f.receiver,
            address!("00000000000000000000000000000000000000C2")
        );
        assert!(f.is_bid);
        assert_eq!(f.amount_in, 1_000);
        assert_eq!(f.amount_out, 2_000);
        assert_eq!(f.partner_id, 7);
        assert_eq!(f.block_number, 42);
        assert_eq!(f.log_index, 5);
        assert_eq!(f.at_secs, 1_700_000_042);
    }

    /// A node that omits `blockTimestamp` must leave the sentinel for `resolve_timestamps` rather
    /// than have the decoder invent one.
    #[test]
    fn a_missing_block_timestamp_is_left_for_resolution() {
        let mut log = one(42, 5);
        log.as_object_mut()
            .expect("object")
            .remove("blockTimestamp");
        assert_eq!(decode(&log).expect("still decodable").at_secs, 0);
    }

    /// The engine is 128-bit and the event 256-bit; a value that does not fit is a divergence
    /// rather than something to truncate into a plausible-looking fill.
    #[test]
    fn refuses_an_amount_outside_the_engine_domain() {
        let too_big = U256::from(u128::MAX) + U256::from(1u64);
        let log = raw_log(Spec {
            amount_in: too_big,
            ..Spec::default()
        });
        assert!(decode(&log).is_none());
    }

    /// The overlap re-reads blocks on purpose, so the dedup ring is what stops double-counted flow.
    #[test]
    fn the_overlap_never_yields_the_same_fill_twice() {
        let mut w = SwapWatch::new(pool());
        let logs = vec![one(10, 0), one(10, 1)];

        let mut first = Polled::default();
        w.absorb(&logs, &mut first);
        assert_eq!(first.fills.len(), 2);
        assert_eq!(first.duplicates, 0);

        let mut again = Polled::default();
        w.absorb(&logs, &mut again);
        assert!(again.fills.is_empty());
        assert_eq!(again.duplicates, 2);
    }

    /// Same transaction, different log index: two fills in one routed transaction are two fills.
    #[test]
    fn two_legs_of_one_transaction_are_two_fills() {
        let mut w = SwapWatch::new(pool());
        let mut out = Polled::default();
        w.absorb(&[one(10, 0), one(10, 1)], &mut out);
        assert_eq!(out.fills.len(), 2);
    }

    #[test]
    fn a_removed_log_is_counted_and_dropped() {
        let mut w = SwapWatch::new(pool());
        let mut log = one(10, 0);
        log["removed"] = json!(true);
        let mut out = Polled::default();
        w.absorb(&[log], &mut out);
        assert!(out.fills.is_empty());
        assert_eq!(out.removed, 1);
    }

    #[test]
    fn a_malformed_log_is_counted_not_silently_dropped() {
        let mut w = SwapWatch::new(pool());
        let mut log = one(10, 0);
        log["data"] = json!("0xzz");
        let mut out = Polled::default();
        w.absorb(&[log], &mut out);
        assert!(out.fills.is_empty());
        assert_eq!(out.undecodable, 1);
        assert_eq!(w.undecodable(), 1);
    }

    /// The ring is bounded, so a long-lived process cannot grow it without limit.
    #[test]
    fn the_dedup_ring_stays_bounded() {
        let mut w = SwapWatch::new(pool());
        for i in 0..(SEEN_CAPACITY as u64 + 100) {
            w.remember((B256::from(U256::from(i)), 0));
        }
        assert_eq!(w.seen.len(), SEEN_CAPACITY);
        assert_eq!(w.seen_set.len(), SEEN_CAPACITY);
    }

    /// A log captured verbatim from GIWA Sepolia, field names and casing included.
    ///
    /// Every other test builds its input with the same `sol!` types the decoder reads back, so all
    /// would still pass if the event ABI and the deployed contract had drifted apart. The bytes are
    /// from the *previous* `PropPool` (`0xA629...6358`, see `DEPLOYMENTS.md`) and deliberately not
    /// restamped: what they pin is that a real node produced them, and the `Swap` event is
    /// unchanged across the two deployments. Also the routed case — `sender` is the adapter,
    /// `receiver` the taker — which is why `markout` scores the receiver.
    fn captured_from_chain() -> serde_json::Value {
        json!({
            "blockNumber": "0x1e4c3d3",
            "blockTimestamp": "0x6a66b02f",
            "logIndex": "0x2d",
            "transactionHash": "0xca570d313b4436c024d1c02d7f32c8b93194c285ab4462fb65d28928abb24783",
            "removed": false,
            "topics": [
                "0x9cfe9d5c9c99284d3a07f72aeb4e1e2a5656e85926c7542e3d0c631e6751930d",
                "0x0000000000000000000000000000000000000000000000000000000000000001",
                "0x00000000000000000000000016c5a0df5ad0c8b0a450edaa67c56593b02d19e2",
                "0x0000000000000000000000002b10d0b50ca3a7c0c7ccabc969615b4db3fb9471",
            ],
            "data": "0x0000000000000000000000000000000000000000000000000000000000000000\
                      000000000000000000000000000000000000000000000000000000003b9aca00\
                      00000000000000000000000000000000000000000000000006ef77ce918afa2e\
                      0000000000000000000000000000000000000000000000000000000000000000",
        })
    }

    #[test]
    fn decodes_a_log_captured_from_the_live_chain() {
        let f = decode(&captured_from_chain()).expect("a real Swap log must decode");
        assert_eq!(f.pair_id, 1);
        assert_eq!(
            f.sender,
            address!("16C5A0DF5AD0C8B0A450EDAA67C56593B02D19E2")
        );
        assert_eq!(
            f.receiver,
            address!("2B10D0B50CA3A7C0C7CCABC969615B4DB3FB9471")
        );
        assert_ne!(
            f.sender, f.receiver,
            "the routed case: an adapter sent it, a taker received it"
        );
        assert!(!f.is_bid, "the pool sold base for 1e9 quote units");
        assert_eq!(f.amount_in, 1_000_000_000);
        assert_eq!(f.amount_out, 499_749_812_750_187_054);
        assert_eq!(f.partner_id, 0);
        assert_eq!(f.block_number, 31_769_555);
        assert_eq!(f.at_secs, 1_785_114_671); // 2026-07-27T01:11:11Z
        assert_eq!(f.log_index, 45);
    }

    /// Records which path the live chain takes: GIWA supplies `blockTimestamp` on every log, so
    /// `resolve_timestamps` is the fallback rather than the norm.
    #[test]
    fn the_live_chain_needs_no_timestamp_backfill() {
        assert_ne!(decode(&captured_from_chain()).expect("decodes").at_secs, 0);
    }

    /// A filter is only as good as its topic: an event shape change must fail here rather than
    /// leave the watcher silently returning nothing.
    #[test]
    fn the_signature_hash_matches_the_deployed_event() {
        assert_eq!(
            format!("{:#x}", Swap::SIGNATURE_HASH),
            "0x9cfe9d5c9c99284d3a07f72aeb4e1e2a5656e85926c7542e3d0c631e6751930d"
        );
    }
}
