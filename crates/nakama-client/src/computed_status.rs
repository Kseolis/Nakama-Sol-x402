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
//! source-of-truth fields per ADR-002 streaming math. They DO NOT change the
//! dispatch — they're informational decoration for the HTTP API.

use serde::Serialize;

use crate::accounts::{
    GracedSubscriptionView, PausedSubscriptionView, SubscriptionStateByte, SubscriptionView,
};

/// Threshold for `ActiveLowFunds` — runway in days at the snapshotted
/// `rate_per_second`. ADR-007 boundary contract uses ratio (utilization >
/// 0.8); we use BOTH gates (whichever fires first) to give the UI a clean
/// countdown ("3 days remaining"). 7 days mirrors `GRACE_DURATION` so the
/// banner appears at the moment the post-exhaustion grace would start.
pub const ACTIVE_LOW_FUNDS_DAYS: u32 = 7;

/// 80% utilization threshold from ADR-007 pseudocode (line 256).
const ACTIVE_LOW_FUNDS_UTILIZATION_NUM: u128 = 80;
const ACTIVE_LOW_FUNDS_UTILIZATION_DEN: u128 = 100;

const SECONDS_PER_DAY: i64 = 86_400;

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
        /// Estimated days of runway from `now` at `rate_per_second`. Saturates
        /// at `u32::MAX` for very large balances (sentinel; not a real value).
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

/// Stream-math helpers for the `Active` arm. Per ADR-002:
/// * `unlocked  = min(deposited, (now - stream_start) * rate_per_second)`
/// * `claimable = unlocked - withdrawn`
fn active_or_low_funds(sub: &SubscriptionView, now: i64) -> ComputedStatus {
    let deposited = sub.deposited_amount as u128;
    let withdrawn = sub.withdrawn_amount as u128;
    let rate = sub.rate_per_second as u128;

    let elapsed = now.saturating_sub(sub.stream_start).max(0) as u128;
    let unlocked_raw = elapsed.saturating_mul(rate);
    let unlocked = unlocked_raw.min(deposited);
    let claimable = unlocked.saturating_sub(withdrawn);

    let unlocked_pct = withdrawn
        .saturating_mul(100)
        .checked_div(deposited)
        .map(|p| p.min(100) as u8)
        .unwrap_or(0);

    // Runway: remaining liquid balance / rate, in days. Remaining liquid =
    // deposited - withdrawn (the merchant has not yet pulled). At
    // rate_per_second this is what the stream can still fund. ZeroRatePerSecond
    // is impossible per BLK-02 (rate_per_second >= 1 after subscribe), but we
    // guard anyway.
    let remaining_liquid = deposited.saturating_sub(withdrawn);
    let days_remaining: u32 = remaining_liquid
        .checked_div(rate)
        .and_then(|secs| secs.checked_div(SECONDS_PER_DAY as u128))
        .map(|days| days.min(u32::MAX as u128) as u32)
        .unwrap_or(u32::MAX);

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

    fn fresh_sub(state: u8, deposited: u64, withdrawn: u64, rate: u64) -> SubscriptionView {
        let pk = Pubkey::new_from_array([1u8; 32]);
        SubscriptionView {
            next_charge_at: 0,
            subscriber: pk,
            plan: pk,
            price: 0,
            period: 0,
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
