//! Program-level error variants.
//!
//! Sources:
//! - ADR-002 §Implementation impact ("error.rs minimum" list)
//! - ADR-003 §FSM enforcement points (state-guard variants)
//! - ADR-014 §Errors (`ZeroPrice`)
//! - Sign-off blockers BLK-02, BLK-06, BLK-07, BLK-08
//!
//! Convention (onchain-conventions.md): every fallible handler path maps
//! to a variant here; no `unwrap()` / `expect()` in handlers.

use anchor_lang::prelude::*;

#[error_code]
pub enum NakamaError {
    /// `Plan.period == 0` would div-by-zero in `rate_per_second` math.
    /// Enforced in `create_plan` (ADR-014) and defensively in `subscribe`
    /// (ADR-002 §subscribe step 1).
    #[msg("Plan period must be greater than zero")]
    ZeroPeriod,

    /// `Plan.price == 0` plans are degenerate (no funds ever flow).
    /// Defence-in-depth in `create_plan` (ADR-014 §Errors).
    #[msg("Plan price must be greater than zero")]
    ZeroPrice,

    /// `periods_to_prefund == 0` would create a zero-deposit subscription
    /// that immediately fails every charge. BLK-07.
    #[msg("Periods to prefund must be at least 1")]
    ZeroPeriodsToFund,

    /// `rate_per_second = price / period` rounded to 0 (price < period_seconds).
    /// Without this guard, vault is funded but every charge fails forever
    /// — silent locked-stream DoS. BLK-02 / ADR-002 §subscribe step 4.
    #[msg("price/period yielded rate_per_second = 0; raise price or shorten period")]
    ZeroRatePerSecond,

    /// `claimable == 0` in `charge`: nothing has unlocked since last settle.
    /// ADR-002 §charge step 5.
    #[msg("Insufficient unlocked funds: claimable is zero")]
    InsufficientUnlockedFunds,

    /// Validator clock moved backwards relative to a stored timestamp.
    /// Without this guard, `(now - stream_start) as u64` wraps to a huge
    /// value and `min(deposited, huge) → deposited` — subscriber loses refund.
    /// BLK-06 / ADR-002 §cancel step 3.
    #[msg("Clock moved backwards relative to stored timestamp")]
    ClockBackwards,

    /// `checked_*` arithmetic overflowed.
    /// ADR-002 §Negative — math overflow risk on long-running streams.
    #[msg("Arithmetic overflow")]
    MathOverflow,

    /// FSM guard: `cancel` only legal from non-terminal states.
    /// In MVP that means `Active`; post-MVP also `Paused / GracePeriod / Exhausted`.
    /// ADR-003 §FSM enforcement points.
    #[msg("Subscription is not in a state that allows cancellation")]
    IllegalStateForCancel,

    /// FSM guard: `charge` legal only from `Active`. ADR-003 §FSM enforcement,
    /// ADR-004 §2.h. Post-ADR-013 split this guard is reachable: `cancel` no
    /// longer closes the Subscription account, so a `charge` against a
    /// Cancelled tombstone deserialises the state byte and fires this variant
    /// (was Anchor `AccountNotInitialized` in cycle-2 fused-cancel MVP).
    #[msg("Subscription is not Active; charge not allowed")]
    IllegalStateForCharge,

    /// `cancel` signer != `subscription.subscriber`.
    /// Defence in depth above the `has_one = subscriber` Anchor constraint.
    /// BLK-08 / ADR-002 §cancel signer policy.
    #[msg("Only the subscription's subscriber may cancel it")]
    UnauthorizedCancel,

    /// `subscriber_ata` and `vault` resolve to the same address.
    /// Defence-in-depth before `top_up` ships in ADR-005; SPL Token's
    /// `Transfer` is a documented no-op when source == destination, which
    /// would let a relaxed `top_up` constraint set persist `deposited_amount`
    /// against an empty vault. See `docs/impl-cycle-1-security-audit.md` §F-2.
    #[msg("subscriber_ata must not equal vault")]
    DuplicateAtaAndVault,

    /// `merchant_ata.key() != subscription.merchant_ata`. Wired to the
    /// `address = ...` constraint on `merchant_ata` in `Charge` (ADR-004 §9).
    /// Custom variant gives operators context the generic Anchor
    /// `ConstraintAddress` (2012) does not — we know precisely which ATA
    /// got swapped (ADR-004 §8).
    #[msg("merchant_ata does not match the subscription's snapshotted merchant ATA")]
    AtaMismatch,

    /// `vault.mint` or `merchant_ata.mint` != `subscription.token_mint`.
    /// Defence-in-depth on top of Anchor's `token::mint` constraint —
    /// see ADR-004 §8 / §9.
    #[msg("token mint mismatch against the subscription snapshot")]
    MintMismatch,

    /// `vault.owner` != Subscription PDA. Covered by Anchor `token::authority`
    /// already; the custom variant exists for explicit audit trail
    /// (ADR-004 §8).
    #[msg("vault authority is not the subscription PDA")]
    VaultOwnerMismatch,

    /// FSM guard: `cleanup` legal only from `Cancelled` or `Exhausted`.
    /// From {Active, Paused, GracePeriod} the caller must `cancel` first
    /// (fair settle + refund) — closes the rage-cleanup vector where a
    /// subscriber would reclaim rent without paying the merchant for
    /// already-streamed time. ADR-013 §"Per-state cleanup eligibility".
    #[msg("cleanup is only allowed in Cancelled or Exhausted states")]
    IllegalStateForCleanup,

    /// `cleanup` signer != `subscription.subscriber`. Defence-in-depth above
    /// the `has_one = subscriber` Anchor constraint. Forward-compat for
    /// ADR-009: merchant may extend `cancel` signer policy, but `cleanup`
    /// stays subscriber-only because rent is a subscriber asset.
    /// ADR-013 §Q1.
    #[msg("only the subscription owner can call cleanup")]
    UnauthorizedCleanup,
}
