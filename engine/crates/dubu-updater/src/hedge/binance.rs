//! The Binance USD-M futures leg: signed REST, on the testnet by default.
//!
//! # Both sides must be the same kind of money
//!
//! The pool trades mock tokens on a testnet. Hedging that on the **live** exchange would leave the
//! only real position in the system on the wrong side of a fake one: drain the testnet pool and
//! what remains is a genuine short with nothing behind it. Testnet on both sides is the only
//! arrangement that is coherent, and testnet USD-M futures track the live price closely enough to
//! prove the mechanism -- measured, ETHUSDT read 1,877.06 there against 1,877.07 live.
//!
//! What testnet does not prove is the economics: its book is thin, so fills there say nothing
//! about real slippage. The slippage curve is therefore measured against the **live** public
//! depth endpoint, which needs no key, and only the execution path runs on testnet.
//!
//! # The clock, for the third time today
//!
//! Every signed request carries a `timestamp` and the exchange rejects anything outside its
//! `recvWindow`. It does not compare against UTC; it compares against **its own** clock. So this
//! measures the offset once against `/fapi/v1/time` and applies it to every request, rather than
//! trusting the host. Getting that wrong is not a subtle failure -- it is
//! `-1021 Timestamp for this request was 1000ms ahead of the server's time`, on every call.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::{info, warn};

use super::Side;

/// Anything that can go wrong talking to the venue.
#[derive(Debug, thiserror::Error)]
pub enum VenueError {
    /// The request never completed.
    #[error("venue transport: {0}")]
    Transport(String),
    /// The venue answered with an error object. The code is Binance's own.
    #[error("venue rejected: {code} {message}")]
    Rejected {
        /// Binance error code, e.g. `-1021` for a clock outside `recvWindow`.
        code: i64,
        /// Binance's message.
        message: String,
    },
    /// The response did not look like what the endpoint documents.
    #[error("venue response: {0}")]
    Decode(String),
    /// No credentials were configured, so nothing could be signed.
    #[error("no venue credentials configured")]
    NoCredentials,
}

/// A filled order, as the venue reported it.
#[derive(Debug, Clone)]
pub struct Fill {
    /// The venue's order id.
    pub order_id: u64,
    /// What actually executed, in base units of the venue's contract.
    pub executed_qty: f64,
    /// Average fill price, or zero when the venue did not report one.
    pub avg_price: f64,
    /// The venue's own status string, e.g. `FILLED` or `PARTIALLY_FILLED`.
    pub status: String,
}

/// Credentials, held so they cannot be printed.
///
/// [`std::fmt::Debug`] is written by hand for the same reason as [`crate::tx::Signer`]: the derived
/// one would put the secret in every log line that formats the client.
pub struct Credentials {
    key: String,
    secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field(
                "key",
                &format!("{}…", &self.key.chars().take(6).collect::<String>()),
            )
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Read from two environment variables. The error names the variable, never the value.
    ///
    /// # Errors
    /// [`VenueError::NoCredentials`] when either is missing or empty.
    pub fn from_env(key_var: &str, secret_var: &str) -> Result<Self, VenueError> {
        let key = std::env::var(key_var).unwrap_or_default();
        let secret = std::env::var(secret_var).unwrap_or_default();
        if key.is_empty() || secret.is_empty() {
            return Err(VenueError::NoCredentials);
        }
        Ok(Self { key, secret })
    }
}

/// A signed client for one Binance USD-M futures endpoint.
#[derive(Debug)]
pub struct Venue {
    base: String,
    creds: Credentials,
    http: reqwest::Client,
    /// Server clock minus host clock, in milliseconds. See the module note.
    offset_ms: i64,
    /// When the offset was measured, so staleness is visible rather than assumed.
    offset_at: Option<Instant>,
    recv_window_ms: u64,
}

impl Venue {
    /// Build against `base`, e.g. `https://testnet.binancefuture.com`.
    ///
    /// # Errors
    /// [`VenueError::Transport`] if the HTTP client cannot be constructed.
    pub fn new(
        base: impl Into<String>,
        creds: Credentials,
        timeout: Duration,
    ) -> Result<Self, VenueError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        Ok(Self {
            base: base.into(),
            creds,
            http,
            offset_ms: 0,
            offset_at: None,
            recv_window_ms: 10_000,
        })
    }

    /// Measure the venue's clock against this host's and remember the difference.
    ///
    /// Call before the first signed request and periodically after: a host whose clock drifts will
    /// start failing every call at once, with an error that names the timestamp and not the cause.
    ///
    /// # Errors
    /// [`VenueError`] if the endpoint cannot be read.
    pub async fn sync_clock(&mut self) -> Result<i64, VenueError> {
        let url = format!("{}/fapi/v1/time", self.base);
        let before = Self::host_millis();
        let v: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| VenueError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|e| VenueError::Decode(e.to_string()))?;
        let after = Self::host_millis();
        let server = v
            .get("serverTime")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| VenueError::Decode("no serverTime".into()))?;

        // The server stamped its time somewhere inside the round trip, so the host's best estimate
        // of "now" at that instant is the midpoint. Using `before` alone would charge the whole
        // round trip to the offset.
        let midpoint = before + (after - before) / 2;
        self.offset_ms = server - midpoint;
        self.offset_at = Some(Instant::now());
        info!(
            target: "hedge", event = "clock_synced", offset_ms = self.offset_ms,
            round_trip_ms = after - before,
            "venue clock measured; every signed request carries this offset"
        );
        Ok(self.offset_ms)
    }

    /// How long since the offset was measured, or `None` if it never was.
    #[must_use]
    pub fn clock_age(&self, now: Instant) -> Option<Duration> {
        self.offset_at.map(|t| now.saturating_duration_since(t))
    }

    fn host_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }

    fn sign(&self, query: &str) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(self.creds.secret.as_bytes())
            .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
        mac.update(query.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    async fn signed(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value, VenueError> {
        let ts = Self::host_millis() + self.offset_ms;
        let mut query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(&format!(
            "recvWindow={}&timestamp={ts}",
            self.recv_window_ms
        ));
        let sig = self.sign(&query);
        let url = format!("{}{path}?{query}&signature={sig}", self.base);

        let resp = self
            .http
            .request(method, &url)
            .header("X-MBX-APIKEY", &self.creds.key)
            .send()
            .await
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| VenueError::Decode(e.to_string()))?;

        // Binance signals failure with a negative `code` alongside `msg`. A successful order also
        // has fields named `code` in some endpoints, so the sign is what discriminates.
        if let Some(code) = body.get("code").and_then(serde_json::Value::as_i64) {
            if code < 0 {
                let message = body
                    .get("msg")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(none)")
                    .to_string();
                if code == -1021 {
                    warn!(
                        target: "hedge", event = "clock_rejected", code, message = %message,
                        offset_ms = self.offset_ms,
                        "the venue rejected our timestamp; the offset needs re-measuring"
                    );
                }
                return Err(VenueError::Rejected { code, message });
            }
        }
        Ok(body)
    }

    /// The account's free USDT balance, as a liveness check that also proves the signature works.
    ///
    /// # Errors
    /// [`VenueError`].
    pub async fn available_usdt(&self) -> Result<f64, VenueError> {
        let v = self
            .signed(reqwest::Method::GET, "/fapi/v2/account", &[])
            .await?;
        v.get("availableBalance")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| VenueError::Decode("no availableBalance".into()))
    }

    /// Cross the spread for `qty` of `symbol`.
    ///
    /// A market order on purpose. The point of a hedge is to be flat, and a limit order that does
    /// not fill leaves the position open while reporting success — which is the failure mode this
    /// exists to prevent.
    ///
    /// Asked for with `newOrderRespType=RESULT`, so the response carries the execution rather than
    /// just an acknowledgement. The default acknowledges with `status: NEW` and `executedQty: 0`,
    /// which reads to [`super::Bands::settle`] as "nothing filled" -- the drift stays outstanding
    /// and the next evaluation crosses the same inventory again.
    ///
    /// # Errors
    /// [`VenueError`].
    pub async fn market(&self, symbol: &str, side: Side, qty: f64) -> Result<Fill, VenueError> {
        let v = self
            .signed(
                reqwest::Method::POST,
                "/fapi/v1/order",
                &[
                    ("symbol", symbol.to_string()),
                    ("side", side.as_str().to_string()),
                    ("type", "MARKET".to_string()),
                    ("quantity", format!("{qty}")),
                    // Without this the venue answers `ACK` -- `status: NEW`, `executedQty: 0` --
                    // and the fill is only visible to a later query. `Bands::settle` deducts what
                    // filled, so an unreported fill leaves the drift outstanding and the next
                    // evaluation crosses again: measured, one 0.04 ETH fill became a 0.08 ETH
                    // short before this was found. `RESULT` waits for the execution report.
                    ("newOrderRespType", "RESULT".to_string()),
                ],
            )
            .await?;

        let num = |k: &str| -> f64 {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0)
        };
        Ok(Fill {
            order_id: v
                .get("orderId")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            executed_qty: num("executedQty"),
            avg_price: num("avgPrice"),
            status: v
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_string(),
        })
    }
}

/// One point on a venue's depth curve: how far the marginal price is from mid at a cumulative size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthPoint {
    /// Cumulative base size.
    pub cumulative: f64,
    /// Marginal concession from mid, in hundredths of a basis point. Positive means worse.
    pub concession_bps_e2: u32,
}

/// Walk one side of an order book into a cumulative concession curve.
///
/// This is what the ladder's slope should be priced from. `width_bps` was chosen rather than
/// derived, and against the live book it is six times too steep at 1,000 ETH -- the book says
/// 3.8 bp where the ladder charges 26.7. Feeding the real curve in replaces the last unmeasured
/// number in the price schedule that flow data is not needed for.
///
/// `levels` is `(price, size)` best-first. Returns points in increasing cumulative size.
#[must_use]
pub fn depth_curve(levels: &[(f64, f64)], mid: f64, cumulative_max: f64) -> Vec<DepthPoint> {
    if mid <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cum = 0.0;
    for &(price, size) in levels {
        if size <= 0.0 {
            continue;
        }
        cum += size;
        let rel = ((mid - price) / mid).abs() * 1_000_000.0;
        out.push(DepthPoint {
            cumulative: cum,
            concession_bps_e2: rel as u32,
        });
        if cum >= cumulative_max {
            break;
        }
    }
    out
}

/// The concession the venue would charge to unwind `size`, in hundredths of a basis point.
///
/// Interpolates between levels rather than snapping to one, so a size that lands mid-level is not
/// charged the next level's price.
#[must_use]
pub fn concession_for(curve: &[DepthPoint], size: f64) -> Option<u32> {
    if size <= 0.0 {
        return curve.first().map(|p| p.concession_bps_e2);
    }
    let mut prev: Option<&DepthPoint> = None;
    for p in curve {
        if p.cumulative >= size {
            let Some(q) = prev else {
                return Some(p.concession_bps_e2);
            };
            let span = p.cumulative - q.cumulative;
            if span <= 0.0 {
                return Some(p.concession_bps_e2);
            }
            let t = (size - q.cumulative) / span;
            let lo = f64::from(q.concession_bps_e2);
            let hi = f64::from(p.concession_bps_e2);
            return Some((lo + (hi - lo) * t) as u32);
        }
        prev = Some(p);
    }
    // Past the end of the book: the venue cannot take it at any price we can quote from here.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live ETHUSDT book, walked. These are the shape the ladder should be priced against.
    fn book() -> Vec<(f64, f64)> {
        let mid = 1877.03;
        vec![
            (mid * (1.0 - 0.0000025), 120.0),
            (mid * (1.0 - 0.00002), 150.0),
            (mid * (1.0 - 0.00008), 230.0),
            (mid * (1.0 - 0.00021), 500.0),
            (mid * (1.0 - 0.00038), 1000.0),
            (mid * (1.0 - 0.00089), 2000.0),
        ]
    }

    #[test]
    fn the_curve_accumulates_size_and_worsens_monotonically() {
        let c = depth_curve(&book(), 1877.03, 10_000.0);
        assert_eq!(c.len(), 6);
        for w in c.windows(2) {
            assert!(w[1].cumulative > w[0].cumulative, "size accumulates");
            assert!(
                w[1].concession_bps_e2 >= w[0].concession_bps_e2,
                "and price only gets worse deeper in the book"
            );
        }
    }

    /// The measurement that started this: near the top the venue is effectively free, and that is
    /// what makes a 3.98 bp half-spread look like what it is.
    #[test]
    fn the_top_of_the_book_is_nearly_free() {
        let c = depth_curve(&book(), 1877.03, 10_000.0);
        let at_100 = concession_for(&c, 100.0).expect("inside the book");
        assert!(at_100 < 10, "under 0.1bp for 100 ETH, got {at_100}");
    }

    /// A size between two levels must not be charged the next level's price.
    #[test]
    fn a_size_between_levels_is_interpolated() {
        let c = depth_curve(&book(), 1877.03, 10_000.0);
        let lo = concession_for(&c, 120.0).expect("at a level");
        let hi = concession_for(&c, 270.0).expect("at the next");
        let mid = concession_for(&c, 195.0).expect("between them");
        assert!(mid > lo && mid < hi, "{lo} < {mid} < {hi}");
    }

    /// Past the end of the book there is no price, and saying so is the point. Returning the last
    /// level instead would quote a size the venue cannot actually absorb.
    #[test]
    fn beyond_the_book_there_is_no_answer() {
        let c = depth_curve(&book(), 1877.03, 10_000.0);
        assert_eq!(concession_for(&c, 50_000.0), None);
    }

    /// The secret must never reach a log line, however the client is formatted.
    #[test]
    fn debug_never_prints_the_secret() {
        let c = Credentials {
            key: "DXhLWDgCITCk1UW10bxhY7Mx".into(),
            secret: "QISmNNVTKCJDj5eEidWVRGWc".into(),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("QISmNNVTKCJD"), "secret leaked: {shown}");
        assert!(!shown.contains("W10bxhY7Mx"), "key leaked in full: {shown}");
    }
}
