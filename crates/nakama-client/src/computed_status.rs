//! Off-chain ComputedStatus derivation — boundary contract from ADR-007
//! §"Off-chain ComputedStatus derive". Keeper, indexer, and x402 facilitator
//! MUST agree on this dispatch; anything richer (e.g. countdown banners) is a
//! UX concern layered on top of the variants below.
//!
//! Logic mirrors the ADR exactly:
//! * `state == Active`  → `Active` or `ActiveLowFunds` (utilization > 80% or
//!   runway < N days),
//! * `state == Paused`  → `Paused`,
//! * `state == GracePeriod` → `InGrace { grace_until, .. }` if `now <=
//!   grace_until`, else `GraceExpired { grace_until }`,
//! * `state == Cancelled` → `Cancelled`,
//! * `state == Exhausted` → `Exhausted`,
//! * unknown discriminant → `Corrupt`. Routing to `Active` would be unsafe
//!   (UI would show "fine"); routing to a terminal state would mask
//!   forward-compat redeploys. `Corrupt` is the worst-case-deny: callers
//!   refuse to settle / charge / top-up against an unknown state.
//!
//! Variant payload fields (`unlocked_pct`, `claimable`, `days_remaining`,
//! `seconds_remaining`) are off-chain enrichments computed from on-chain
//! source-of-truth fields per ADR-015 §F4 lazy-precise streaming math
//! (`unlocked = (elapsed * price) / period`). They DO NOT change the
//! dispatch — they're informational decoration for the HTTP API.
//! Cross-language byte-equivalent with TS `clients/ts/src/computedStatus.ts`
//! and on-chain `programs/nakama/src/instructions/{charge,cancel,settle_usage}.rs`.

use serde::Serialize;

use crate::accounts::{
    GracedSubscriptionView, PausedSubscriptionView, SubscriptionStateByte, SubscriptionView,
};

/// Threshold for `ActiveLowFunds` — runway in days at the streaming rate
/// derived inline from `(price, period)`. ADR-007 boundary contract uses
/// ratio (utilization > 0.8); we use BOTH gates (whichever fires first)
/// to give the UI a clean countdown ("3 days remaining"). 7 days mirrors
/// `GRACE_DURATION` so the banner appears at the moment the
/// post-exhaustion grace would start.
pub const ACTIVE_LOW_FUNDS_DAYS: u32 = 7;

/// 80% utilization threshold from ADR-007 pseudocode (line 256).
const ACTIVE_LOW_FUNDS_UTILIZATION_NUM: u128 = 80;
const ACTIVE_LOW_FUNDS_UTILIZATION_DEN: u128 = 100;

const SECONDS_PER_DAY: u128 = 86_400;

/// Sentinel for `days_remaining` when `price == 0` (defensive — subscribe
/// rejects zero-price plans). Matches TS `DAYS_REMAINING_SENTINEL =
/// 0xFFFF_FFFF` for cross-language byte-equivalence (ADR-015 §F4,
/// BLK-007-MAJ-2).
const DAYS_REMAINING_SENTINEL: u32 = u32::MAX;

/// Off-chain derived status for a subscription. JSON-serialized externally
/// tagged on `state` (`{"state": "Active", ...}` shape) for the x402
/// facilitator HTTP API. Internal to the off-chain workspace — not an
/// on-chain ABI.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state")]
pub enum ComputedStatus {
    #[serde(rename = "Active")]
    Active {
        /// 0..=100. `withdrawn_amount * 100 / deposited_amount`, saturating.
        unlocked_pct: u8,
        /// `unlocked - withdrawn` in token base units. What the merchant could
        /// settle right now.
        claimable: u64,
    },
    #[serde(rename = "ActiveLowFunds")]
    ActiveLowFunds {
        unlocked_pct: u8,
        claimable: u64,
        /// Estimated days of runway from `now` at the streaming rate derived
        /// inline from `(price, period)`. Saturates at `u32::MAX` for very
        /// large balances and on `price == 0` (sentinel; not a real value)
        /// for cross-language byte-equivalence with the TS mirror
        /// (ADR-015 §F4, BLK-007-MAJ-2).
        days_remaining: u32,
    },
    #[serde(rename = "Paused")]
    Paused {
        /// Placeholder for ADR-006 fields (resume_after, etc.). Empty until ADR-006 lands.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_after: Option<i64>,
    },
    #[serde(rename = "InGrace")]
    InGrace {
        grace_until: i64,
        /// `grace_until - now`. Strictly positive in this variant.
        seconds_remaining: i64,
    },
    #[serde(rename = "GraceExpired")]
    GraceExpired { grace_until: i64 },
    #[serde(rename = "Cancelled")]
    Cancelled,
    #[serde(rename = "Exhausted")]
    Exhausted,
    /// Off-chain only. Surfaces (a) unknown state byte from a future redeploy
    /// AND (b) `state == GracePeriod` with no satellite passed (per ADR-007
    /// pseudocode: `None => ComputedStatus::Corrupt`). Callers must log and
    /// refuse to charge / top-up.
    #[serde(rename = "Corrupt")]
    Corrupt {
        /// Raw state byte for operator triage.
        state_byte: u8,
        /// Human-readable cause.
        reason: &'static str,
    },
}

/// Boundary contract from ADR-007. Pure function; no RPC, no logging.
///
/// # Inputs
/// * `subscription` — required, decoded from on-chain Subscription account.
/// * `graced` — caller pre-fetched the `GracedSubscription` PDA. `None` is
///   the normal case for `state != GracePeriod`; for `state == GracePeriod`
///   a missing satellite signals corruption.
/// * `paused` — placeholder until ADR-006 ships; pass `None` today.
/// * `now` — wall clock (`unix_timestamp` from the same RPC `Clock` sysvar).
///
/// # Note on `unknown` state byte
/// Per `state.rs:50-69`, `SubscriptionState` is `#[non_exhaustive]` and
/// reserves discriminants for future variants. If we receive a byte ≥ 5
/// (e.g. after a redeploy adds a new state), we route to `Corrupt` rather
/// than a default-active interpretation — the UI / keeper / facilitator
/// then refuse to act, which is the safe choice (no spurious settle).
pub fn derive_status(
    subscription: &SubscriptionView,
    graced: Option<&GracedSubscriptionView>,
    paused: Option<&PausedSubscriptionView>,
    now: i64,
) -> ComputedStatus {
    let _ = paused; // ADR-006 not landed; arg reserved for future plumbing.

    match subscription.state_byte() {
        SubscriptionStateByte::Active => active_or_low_funds(subscription, now),
        SubscriptionStateByte::Paused => ComputedStatus::Paused { resume_after: None },
        SubscriptionStateByte::GracePeriod => match graced {
            Some(g) if now <= g.grace_until => ComputedStatus::InGrace {
                grace_until: g.grace_until,
                seconds_remaining: g.grace_until.saturating_sub(now),
            },
            Some(g) => ComputedStatus::GraceExpired {
                grace_until: g.grace_until,
            },
            None => ComputedStatus::Corrupt {
                state_byte: subscription.state,
                reason: "state==GracePeriod but no GracedSubscription satellite",
            },
        },
        SubscriptionStateByte::Cancelled => ComputedStatus::Cancelled,
        SubscriptionStateByte::Exhausted => ComputedStatus::Exhausted,
        SubscriptionStateByte::Unknown(b) => ComputedStatus::Corrupt {
            state_byte: b,
            reason: "unknown state discriminant — likely a forward-compat redeploy",
        },
    }
}

/// Stream-math helpers for the `Active` arm. Per ADR-015 §F4 (lazy precise
/// division — F4-mirror of on-chain `charge.rs` / `cancel.rs` /
/// `settle_usage.rs`):
///
/// ```text
/// unlocked  = min(deposited, (elapsed * price) / period)
/// claimable = unlocked - withdrawn
/// ```
///
/// The previous form `rate_per_second * elapsed` truncated the per-second
/// rate to integer base units, under-paying the merchant by up to
/// `(price mod period) / period` base units per second of accrual — up to
/// ~22% on plans where `price < period` (e.g. $10 USDC / 30d:
/// `10_000_000 / 2_592_000 = 3` truncated, vs precise rate of 3.858…).
/// Reading `(price, period)` directly avoids the snapshot rounding;
/// `rate_per_second` is now advisory and retained only for layout
/// compatibility.
///
/// Overflow window: `elapsed * price` overflows u128 only when
/// `elapsed > u128::MAX / price ≈ 1.7e29` sec for `price = u64::MAX` —
/// unreachable in practice. We still guard via `saturating_mul` /
/// `checked_div` so a corrupted satellite read cannot panic the derive.
fn active_or_low_funds(sub: &SubscriptionView, now: i64) -> ComputedStatus {
    let deposited = sub.deposited_amount as u128;
    let withdrawn = sub.withdrawn_amount as u128;
    let price = sub.price as u128;
    let period = sub.period as u128;

    // Clock-skew defence (ADR-002 §cancel step 3): if validator clock moved
    // backwards relative to stream_start, treat elapsed as 0.
    let elapsed = now.saturating_sub(sub.stream_start).max(0) as u128;

    // F4 lazy precise math. Guard `period == 0` defensively — on-chain
    // `InvalidPeriod` rejects this at subscribe, but a corrupt satellite
    // read shouldn't crash the off-chain derive.
    let accrued = elapsed
        .saturating_mul(price)
        .checked_div(period)
        .unwrap_or(0);
    let unlocked = accrued.min(deposited);
    let claimable = unlocked.saturating_sub(withdrawn);

    let unlocked_pct = withdrawn
        .saturating_mul(100)
        .checked_div(deposited)
        .map(|p| p.min(100) as u8)
        .unwrap_or(0);

    // Runway: remaining liquid balance / rate, in days. F4-mirror — derive
    // rate from `(price, period)` inline rather than reading the
    // now-advisory `rate_per_second` snapshot. Formula:
    //
    //   days_of_runway = (remaining_liquid * period) / (price * SECONDS_PER_DAY)
    //
    // Algebraically equivalent to `remaining_liquid / rate / SECONDS_PER_DAY`
    // with exact-arithmetic semantics (no rate truncation). `price == 0` is
    // impossible per the on-chain `InvalidPrice` guard but we surface the
    // sentinel `u32::MAX` to match the TS mirror byte-for-byte.
    let remaining_liquid = deposited.saturating_sub(withdrawn);
    let days_remaining: u32 = if price == 0 {
        DAYS_REMAINING_SENTINEL
    } else {
        let denom = price.saturating_mul(SECONDS_PER_DAY);
        remaining_liquid
            .saturating_mul(period)
            .checked_div(denom)
            .map(|days| days.min(u32::MAX as u128) as u32)
            .unwrap_or(DAYS_REMAINING_SENTINEL)
    };

    let claimable_u64 = claimable.min(u64::MAX as u128) as u64;

    let utilization_low = deposited > 0
        && withdrawn.saturating_mul(ACTIVE_LOW_FUNDS_UTILIZATION_DEN)
            > deposited.saturating_mul(ACTIVE_LOW_FUNDS_UTILIZATION_NUM);
    let runway_low = days_remaining < ACTIVE_LOW_FUNDS_DAYS;

    if utilization_low || runway_low {
        ComputedStatus::ActiveLowFunds {
            unlocked_pct,
            claimable: claimable_u64,
            days_remaining,
        }
    } else {
        ComputedStatus::Active {
            unlocked_pct,
            claimable: claimable_u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;

    /// Build a synthetic `SubscriptionView` with `price = rate * period` so
    /// existing tests written in `rate_per_second` terms remain valid
    /// canonically. F4 math reads `(price, period)`; pre-F4 tests that
    /// asserted on `rate * elapsed` continue to pass because we pick a
    /// period that makes `(elapsed * price) / period == rate * elapsed`
    /// exactly. `rate_per_second` is preserved in the layout but advisory.
    fn fresh_sub(state: u8, deposited: u64, withdrawn: u64, rate: u64) -> SubscriptionView {
        const PERIOD: i64 = 2_592_000; // 30 days
        let price = (rate as u128)
            .saturating_mul(PERIOD as u128)
            .min(u64::MAX as u128) as u64;
        let pk = Pubkey::new_from_array([1u8; 32]);
        SubscriptionView {
            next_charge_at: 0,
            subscriber: pk,
            plan: pk,
            price,
            period: PERIOD,
            token_mint: pk,
            merchant: pk,
            merchant_ata: pk,
            state,
            bump: 0,
            vault_bump: 0,
            created_at: 0,
            last_charge_at: 0,
            deposited_amount: deposited,
            withdrawn_amount: withdrawn,
            rate_per_second: rate,
            stream_start: 0,
            reserved: [0u8; 32],
        }
    }

    /// F4-aware builder: takes `(price, period)` directly. Used by the
    /// canonical $10 USDC / 30d vector and other tests where the
    /// rate-only synthesis would lose precision (e.g. when `price <
    /// period` and the integer rate truncates to 0 or 3).
    fn fresh_sub_pp(
        state: u8,
        deposited: u64,
        withdrawn: u64,
        price: u64,
        period: i64,
    ) -> SubscriptionView {
        let pk = Pubkey::new_from_array([1u8; 32]);
        SubscriptionView {
            next_charge_at: 0,
            subscriber: pk,
            plan: pk,
            price,
            period,
            token_mint: pk,
            merchant: pk,
            merchant_ata: pk,
            state,
            bump: 0,
            vault_bump: 0,
            created_at: 0,
            last_charge_at: 0,
            deposited_amount: deposited,
            withdrawn_amount: withdrawn,
            // Advisory after F4; populated with the pre-F4 truncated rate to
            // exercise the "ignored snapshot" forward-compat guarantee.
            rate_per_second: if period > 0 { price / period as u64 } else { 0 },
            stream_start: 0,
            reserved: [0u8; 32],
        }
    }

    fn fresh_grace(grace_until: i64) -> GracedSubscriptionView {
        GracedSubscriptionView {
            subscription: Pubkey::new_from_array([2u8; 32]),
            entered_grace_at: 0,
            grace_until,
        }
    }

    // ── State == Active ───────────────────────────────────────────────────

    #[test]
    fn active_full_runway_returns_active() {
        // 1_000_000 deposited, 1/sec rate, 0 withdrawn → 11.5 days runway.
        let sub = fresh_sub(0, 1_000_000, 0, 1);
        match derive_status(&sub, None, None, 100) {
            ComputedStatus::Active {
                unlocked_pct,
                claimable,
            } => {
                assert_eq!(unlocked_pct, 0);
                // 100s elapsed, 1/sec → 100 unlocked, 0 withdrawn → 100 claimable.
                assert_eq!(claimable, 100);
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn active_low_runway_routes_to_low_funds() {
        // 86400 deposited, 1/sec rate → 1 day runway. < 7 days → ActiveLowFunds.
        let sub = fresh_sub(0, 86_400, 0, 1);
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::ActiveLowFunds { days_remaining, .. } => {
                assert_eq!(days_remaining, 1);
            }
            other => panic!("expected ActiveLowFunds, got {other:?}"),
        }
    }

    #[test]
    fn active_high_utilization_routes_to_low_funds() {
        // 1_000_000 deposited, 850_000 withdrawn (85%) → ActiveLowFunds even
        // if days_remaining looks generous (rate 1, 150_000s = 1.7 days runway
        // is also < 7 → actually triggers BOTH gates; that's fine, the
        // contract is "either gate fires"). Test the utilization gate alone
        // by pumping rate down.
        let sub = fresh_sub(0, 100_000_000, 85_000_000, 1);
        // 15_000_000 / 86400 = 173.6 days runway → runway gate is FALSE.
        // utilization = 85% > 80% → utilization gate is TRUE.
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::ActiveLowFunds { unlocked_pct, .. } => {
                assert_eq!(unlocked_pct, 85);
            }
            other => panic!("expected ActiveLowFunds (utilization), got {other:?}"),
        }
    }

    // ── State == Paused ───────────────────────────────────────────────────

    #[test]
    fn paused_returns_paused_variant() {
        let sub = fresh_sub(1, 1_000, 0, 1);
        assert!(matches!(
            derive_status(&sub, None, None, 0),
            ComputedStatus::Paused { resume_after: None }
        ));
    }

    // ── State == GracePeriod ─────────────────────────────────────────────

    #[test]
    fn grace_with_satellite_in_window_returns_in_grace() {
        let sub = fresh_sub(2, 1_000, 1_000, 1);
        let grace = fresh_grace(1_000);
        match derive_status(&sub, Some(&grace), None, 500) {
            ComputedStatus::InGrace {
                grace_until,
                seconds_remaining,
            } => {
                assert_eq!(grace_until, 1_000);
                assert_eq!(seconds_remaining, 500);
            }
            other => panic!("expected InGrace, got {other:?}"),
        }
    }

    #[test]
    fn grace_with_satellite_at_boundary_returns_in_grace() {
        // now == grace_until → still InGrace (boundary inclusive per ADR-007).
        let sub = fresh_sub(2, 1_000, 1_000, 1);
        let grace = fresh_grace(1_000);
        assert!(matches!(
            derive_status(&sub, Some(&grace), None, 1_000),
            ComputedStatus::InGrace { .. }
        ));
    }

    #[test]
    fn grace_with_satellite_past_window_returns_grace_expired() {
        let sub = fresh_sub(2, 1_000, 1_000, 1);
        let grace = fresh_grace(1_000);
        match derive_status(&sub, Some(&grace), None, 1_001) {
            ComputedStatus::GraceExpired { grace_until } => {
                assert_eq!(grace_until, 1_000);
            }
            other => panic!("expected GraceExpired, got {other:?}"),
        }
    }

    #[test]
    fn grace_without_satellite_returns_corrupt() {
        let sub = fresh_sub(2, 1_000, 1_000, 1);
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::Corrupt { state_byte, .. } => assert_eq!(state_byte, 2),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ── State == Cancelled / Exhausted ────────────────────────────────────

    #[test]
    fn cancelled_returns_cancelled() {
        let sub = fresh_sub(4, 0, 0, 1);
        assert!(matches!(
            derive_status(&sub, None, None, 0),
            ComputedStatus::Cancelled
        ));
    }

    #[test]
    fn exhausted_returns_exhausted() {
        let sub = fresh_sub(3, 0, 0, 1);
        assert!(matches!(
            derive_status(&sub, None, None, 0),
            ComputedStatus::Exhausted
        ));
    }

    // ── Forward-compat: unknown state byte ────────────────────────────────

    #[test]
    fn unknown_state_byte_returns_corrupt() {
        let sub = fresh_sub(99, 1_000, 0, 1);
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::Corrupt { state_byte, .. } => assert_eq!(state_byte, 99),
            other => panic!("expected Corrupt for unknown state, got {other:?}"),
        }
    }

    // ── Edge cases ────────────────────────────────────────────────────────

    #[test]
    fn zero_deposited_does_not_panic() {
        let sub = fresh_sub(0, 0, 0, 1);
        match derive_status(&sub, None, None, 100) {
            // Zero deposited; utilization gate guarded by `deposited > 0`.
            // Runway gate: remaining = 0, days_remaining = 0 → < 7 → low funds.
            ComputedStatus::ActiveLowFunds {
                unlocked_pct,
                claimable,
                days_remaining,
            } => {
                assert_eq!(unlocked_pct, 0);
                assert_eq!(claimable, 0);
                assert_eq!(days_remaining, 0);
            }
            other => panic!("expected ActiveLowFunds (zero balance), got {other:?}"),
        }
    }

    // ── F4-mirror: lazy precise (price, period) math ─────────────────────
    //
    // Cross-language canonical vectors. Numbers and assertions match TS
    // `clients/ts/scripts/09-adr-015-smoke.ts` (BLK-007-MAJ-2 boundary
    // contract: byte-equivalent claimable across Rust/TS/on-chain).

    /// Canonical ADR-015 §F4 vector: $10 USDC over 30 days, full period
    /// elapsed. Pre-F4 (rate truncation: `rate = 10_000_000 / 2_592_000 =
    /// 3`) → `unlocked_old = 3 * 2_592_000 = 7_776_000` (~22% under-pay).
    /// F4 (lazy precise) → `unlocked = (2_592_000 * 10_000_000) /
    /// 2_592_000 = 10_000_000`.
    #[test]
    fn f4_canonical_usdc_monthly_full_period() {
        let sub = fresh_sub_pp(
            0,
            100_000_000, // 100 USDC deposited
            0,
            10_000_000, // 10 USDC price
            2_592_000,  // 30 days period
        );
        let claimable = match derive_status(&sub, None, None, 2_592_000) {
            ComputedStatus::Active { claimable, .. }
            | ComputedStatus::ActiveLowFunds { claimable, .. } => claimable,
            other => panic!("expected Active-class, got {other:?}"),
        };
        assert_eq!(
            claimable, 10_000_000,
            "F4 exact: full period claimable == 10_000_000 USDC base units"
        );
        // Demonstrate the regression that F4 fixes — pre-F4 would give:
        //   rate = 10_000_000 / 2_592_000 = 3 (truncated)
        //   unlocked = 3 * 2_592_000 = 7_776_000
        // The new derive MUST NOT produce 7_776_000.
        assert_ne!(
            claimable, 7_776_000,
            "must not regress to pre-F4 truncation"
        );
    }

    /// Half-period: unlocked should be price / 2 = 5_000_000.
    /// Pre-F4 with rate=3: `3 * 1_296_000 = 3_888_000`.
    #[test]
    fn f4_canonical_usdc_monthly_half_period() {
        let sub = fresh_sub_pp(0, 100_000_000, 0, 10_000_000, 2_592_000);
        let claimable = match derive_status(&sub, None, None, 1_296_000) {
            ComputedStatus::Active { claimable, .. }
            | ComputedStatus::ActiveLowFunds { claimable, .. } => claimable,
            other => panic!("expected Active-class, got {other:?}"),
        };
        assert_eq!(claimable, 5_000_000, "F4 exact: half-period claimable");
        assert_ne!(
            claimable, 3_888_000,
            "must not regress to pre-F4 truncation"
        );
    }

    /// `unlocked` clamped to `deposited` even when accrued exceeds it
    /// (subscriber deposited < one period). Mirrors TS `subOvershoot`.
    #[test]
    fn f4_overshoot_clamps_to_deposited() {
        let sub = fresh_sub_pp(
            0, 1_000_000, // 0.1 period worth deposited
            0, 10_000_000, 2_592_000,
        );
        let claimable = match derive_status(&sub, None, None, 10 * 2_592_000) {
            ComputedStatus::Active { claimable, .. }
            | ComputedStatus::ActiveLowFunds { claimable, .. } => claimable,
            other => panic!("expected Active-class, got {other:?}"),
        };
        assert_eq!(
            claimable, 1_000_000,
            "F4 overshoot: claimable clamps to deposited"
        );
    }

    /// Clock-skew defence: `now < stream_start` → elapsed = 0 → claimable = 0.
    #[test]
    fn f4_clock_backwards_yields_zero_claimable() {
        let mut sub = fresh_sub_pp(0, 100_000_000, 0, 10_000_000, 2_592_000);
        sub.stream_start = 1_000;
        let claimable = match derive_status(&sub, None, None, 500) {
            ComputedStatus::Active { claimable, .. }
            | ComputedStatus::ActiveLowFunds { claimable, .. } => claimable,
            other => panic!("expected Active-class, got {other:?}"),
        };
        assert_eq!(claimable, 0);
    }

    /// `rate_per_second` snapshot is now advisory — derive must read
    /// `(price, period)` and ignore a stale/corrupted snapshot. We poison
    /// the snapshot with 0 and confirm claimable is still F4-exact.
    #[test]
    fn f4_rate_per_second_is_advisory_only() {
        let mut sub = fresh_sub_pp(0, 100_000_000, 0, 10_000_000, 2_592_000);
        sub.rate_per_second = 0; // poison
        let claimable = match derive_status(&sub, None, None, 2_592_000) {
            ComputedStatus::Active { claimable, .. }
            | ComputedStatus::ActiveLowFunds { claimable, .. } => claimable,
            other => panic!("expected Active-class, got {other:?}"),
        };
        assert_eq!(claimable, 10_000_000);
    }

    /// `days_remaining` uses precise `(remaining * period) / (price *
    /// SECONDS_PER_DAY)`. 100 USDC at 10 USDC/30d → 300 days runway → far
    /// above the 7-day low-funds threshold → routed to `Active`, NOT
    /// `ActiveLowFunds`. Pre-F4 would have computed the same days via
    /// `remaining / rate / 86_400` with rate-truncation losing precision
    /// on plans where `price < period`; this test pins the post-F4 routing.
    #[test]
    fn f4_days_remaining_is_precise() {
        let sub = fresh_sub_pp(0, 100_000_000, 0, 10_000_000, 2_592_000);
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::Active {
                unlocked_pct,
                claimable,
            } => {
                assert_eq!(unlocked_pct, 0);
                assert_eq!(claimable, 0); // elapsed = 0.
            }
            other => panic!("expected Active (300d runway), got {other:?}"),
        }
    }

    /// `price == 0` is impossible post-subscribe but the off-chain derive
    /// must not panic on corrupted reads — it surfaces the sentinel
    /// `u32::MAX` matching the TS `DAYS_REMAINING_SENTINEL`. We force the
    /// `ActiveLowFunds` arm via high utilization so the sentinel is
    /// observable in the variant payload.
    #[test]
    fn f4_zero_price_yields_sentinel_days_remaining() {
        // 85% withdrawn → utilization gate fires regardless of runway.
        let sub = fresh_sub_pp(0, 100_000_000, 85_000_000, 0, 2_592_000);
        match derive_status(&sub, None, None, 0) {
            ComputedStatus::ActiveLowFunds {
                days_remaining,
                unlocked_pct,
                ..
            } => {
                assert_eq!(days_remaining, u32::MAX);
                assert_eq!(unlocked_pct, 85);
            }
            other => panic!("expected ActiveLowFunds(sentinel), got {other:?}"),
        }
    }

    #[test]
    fn json_serialization_uses_state_tag() {
        let s = ComputedStatus::Active {
            unlocked_pct: 10,
            claimable: 42,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""state":"Active""#));
        assert!(json.contains(r#""unlocked_pct":10"#));

        let g = ComputedStatus::InGrace {
            grace_until: 1234,
            seconds_remaining: 100,
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains(r#""state":"InGrace""#));
        assert!(json.contains(r#""grace_until":1234"#));
    }
}
