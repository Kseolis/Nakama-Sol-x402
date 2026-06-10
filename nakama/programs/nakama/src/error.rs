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

    /// FSM guard: `top_up` legal only from `{Active, Paused, GracePeriod}`.
    /// Reject from `Cancelled` / `Exhausted` (terminal). ADR-007 §"Per-state
    /// eligibility table" + §I-TOPUP-3.
    #[msg("Top-up not allowed in current subscription state")]
    IllegalStateForTopUp,

    /// `top_up(amount)` with `amount == 0` is rejected — the CPI would no-op
    /// while still emitting an event and (in absence of guard) consuming CU.
    /// ADR-007 §Adversarial 2.
    #[msg("Top-up amount must be greater than zero")]
    IllegalAmountForTopUp,

    /// State byte says `GracePeriod` but caller did not provide the
    /// `GracedSubscription` satellite account. Reachable from both `top_up`
    /// (recovery branch) and `cancel` (effective_now branch from grace).
    /// ADR-007 §"top_up handler" + §"cancel from GracePeriod".
    #[msg("GracePeriod state requires GracedSubscription account")]
    MissingGraceSatellite,

    /// `cancel` signer is neither `subscription.subscriber` nor
    /// `subscription.merchant`. Polymorphic dual-actor guard introduced by
    /// ADR-009; supersedes (functionally) the legacy `UnauthorizedCancel`
    /// variant which keyed off the `has_one = subscriber` constraint dropped
    /// when the signer policy was widened.
    #[msg("Signer is neither subscriber nor merchant of this subscription")]
    NoCancelAuthority,

    /// `cancel` was called with an explicit `subscriber` AccountInfo whose
    /// pubkey does not match `subscription.subscriber`. Defends the rent-flow
    /// invariant (vault rent → subscriber, not the cancel actor) when the
    /// merchant is the signer. ADR-009 §"Rent-flow invariant".
    #[msg("subscriber account does not match the snapshotted subscriber")]
    SubscriberAccountMismatch,

    // ── ADR-x402-001 (PaySession satellite layer) ──────────────────────────
    // Codes 6021..6032 — twelve variants per ADR-x402-001 §"Error variants added".
    // Order is wire-stable; never reorder, only append.
    /// `open_session` signer mismatch — subscriber must match `parent.subscriber`
    /// (declarative `has_one` triggers this). ADR-x402-001 §"open_session".
    #[msg("Only the subscription's subscriber may open a PaySession")]
    UnauthorizedOpenSession,

    /// Boundary contract violated — `parent.state != Active` for an x402
    /// instruction that requires Active parent (open_session, settle_usage).
    /// Single guard covers Paused / GracePeriod / Cancelled / Exhausted.
    /// ADR-x402-001 §"Boundary contracts" (inherited from ADR-007).
    #[msg("Parent Subscription is not Active; x402 ix not allowed")]
    ParentNotActive,

    /// `open_session` rejected — `reservation_cap` exceeds the parent's
    /// remaining escrow (`deposited - withdrawn`). Defence against opening
    /// a session that promises more than the parent escrow can fulfil.
    /// ADR-x402-001 §"open_session" + §Adversarial 3.
    #[msg("reservation_cap exceeds remaining parent escrow")]
    ReservationCapExceedsEscrow,

    /// `settle_usage(amount)` called with `amount == 0` — would no-op while
    /// still consuming CU and emitting a misleading event.
    /// ADR-x402-001 §"settle_usage".
    #[msg("Settle amount must be greater than zero")]
    IllegalAmountForSettle,

    /// `settle_usage` called when `pay_session.state != Open`. Reachable when
    /// (a) a previous settle crashed mid-CPI leaving `Settling` on disk, or
    /// (b) caller attempts settle on a closed session (Anchor would also
    /// surface AccountNotInitialized in case (b)). ADR-x402-001 §"Internal FSM".
    #[msg("PaySession is not Open; settle not allowed")]
    IllegalStateForSettle,

    /// `settle_usage(amount)` would push `pay_session.usage_amount` past
    /// `pay_session.reservation_cap`. Bounds the damage from a compromised
    /// or malicious facilitator key. ADR-x402-001 §"settle_usage" +
    /// §Adversarial 3 / §8.
    #[msg("settle_usage would exceed reservation_cap")]
    ReservationCapExceeded,

    /// `settle_usage` signer is not `pay_session.facilitator`. ADR-x402-001
    /// §"Facilitator authority model" (Q5 Option A — on-chain delegation).
    #[msg("signer is not the authorised facilitator for this PaySession")]
    UnauthorizedFacilitator,

    /// `pay_session.subscription` does not match the parent passed in the
    /// instruction context. Defence-in-depth above the PDA seed constraint.
    /// ADR-x402-001 §Adversarial 9 (cross-session replay).
    #[msg("PaySession parent reference does not match the supplied parent")]
    PaySessionParentMismatch,

    /// `close_session` rejected — `pay_session.state != Open`. The transient
    /// `Settling` byte should never persist after a successful settle; if it
    /// does, the recovery path is `force_close_session` (post-MVP, R3).
    /// ADR-x402-001 §"close_session" + §Adversarial 4.
    #[msg("PaySession is not Open; close not allowed (Settling stuck — see R3)")]
    IllegalStateForClose,

    /// `close_session` signer mismatch — only subscriber may close.
    /// Forward-injection from ADR-013 §Q1: rent recipient is always the
    /// subscriber, not the facilitator or any third party.
    /// ADR-x402-001 §"close_session".
    #[msg("Only the subscription's subscriber may close a PaySession")]
    UnauthorizedClose,

    /// PaySession-specific overflow detector. We re-introduce the variant
    /// (rather than reuse the existing `MathOverflow`) so the error message
    /// can carry the x402 context — keepers / facilitators can route
    /// log-grep on the specific code without tracing back to the generic
    /// arithmetic path. ADR-x402-001 §Adversarial 6.
    #[msg("Arithmetic overflow in x402 settlement math")]
    ArithmeticOverflow,

    /// `settle_usage` was passed a `merchant_ata` that does not match
    /// `pay_session.merchant_ata` snapshot. Distinct from the existing
    /// `AtaMismatch` (which keys off `Subscription.merchant_ata`) — the x402
    /// variant lets operators diagnose mis-routing per-session vs per-sub.
    /// ADR-x402-001 §"settle_usage".
    #[msg("merchant_ata does not match the PaySession's snapshotted ATA")]
    PaySessionMerchantAtaMismatch,

    // ── ADR-006 (Pause / Resume satellite layer) ───────────────────────────
    // Codes 6033..6036. Wire-stable; never reorder.
    /// `pause` signer != `subscription.merchant`. Authority is merchant-only
    /// per ADR-006 Q3 — subscriber's escape hatch is `cancel`, not pause.
    #[msg("Only the subscription's merchant may pause it")]
    UnauthorizedPause,

    /// FSM guard: `pause` legal only from `Active`. Re-pause from Paused
    /// rejected explicitly. ADR-006 §"Per-state eligibility table".
    #[msg("Subscription is not Active; pause not allowed")]
    IllegalStateForPause,

    /// `resume` signer != `subscription.merchant`. Same authority as pause —
    /// merchant created the pause, only merchant can lift it.
    #[msg("Only the subscription's merchant may resume it")]
    UnauthorizedResume,

    /// FSM guard: `resume` legal only from `Paused`. ADR-006 §"FSM transitions".
    #[msg("Subscription is not Paused; resume not allowed")]
    IllegalStateForResume,

    // ── ADR-015 (Impl-Cycle-2 Security Remediation) ────────────────────────
    // Codes 6037..6038. Wire-stable; never reorder.
    /// F1 (ADR-015) — `charge` from `Active` was invoked with a
    /// `GracedSubscription` satellite already present, but the post-CPI math
    /// did NOT exhaust the stream (`withdrawn != deposited`). Such a satellite
    /// could only have been allocated by a permissionless third-party caller
    /// before this charge ran — see ADR-015 §F1. Anchor codegen for
    /// `Option<Account<T>>` + `init` (anchor-syn-1.0.2/src/codegen/accounts/
    /// constraints.rs:29-50) wraps the init body in
    /// `if let Some(field) = field { ... } else { None }`, so init fires
    /// only when the caller plants a real PDA in the slot. Attackers exploit
    /// this to pre-allocate the grace satellite while the subscription is
    /// healthy, bricking the next honest exhausting charge with
    /// `AccountAlreadyInUse`. This guard rejects the attacker tx instead.
    #[msg("graced_subscription must be absent when charge does not exhaust the stream")]
    UnexpectedGraceSatellite,

    /// F4 (ADR-015) — defensive: `Subscription.period <= 0` after the snapshot,
    /// which should be impossible (subscribe enforces `Plan.period > 0` and
    /// the snapshot is immutable per ADR-001). Reached only on a corrupted
    /// account or hostile downgrade. Distinct from `ZeroPeriod` (which guards
    /// `Plan.period` at create_plan / subscribe) to disambiguate the failure
    /// site for operators reading logs.
    #[msg("Subscription period snapshot is non-positive (corrupted account)")]
    InvalidPeriod,
}
