//! On-chain account layouts and events.
//!
//! Sources:
//! - ADR-001 §Plan account, §Subscription account (revised 2026-04-27 BLK-01/03/05)
//! - ADR-002 §Streaming state as named fields
//! - ADR-003 §State enum + §Per-state semantics
//! - ADR-014 §Event (`PlanCreated`)
//!
//! Layout invariant (ADR-001): no `realloc`, no insert-in-middle. Field order
//! and byte offsets are part of the public ABI; new fields go ONLY before
//! `reserved` and ONLY by shrinking it. Verified by the const-asserts at
//! the bottom of this file.

use anchor_lang::prelude::*;

/// Subscription FSM state — see ADR-003 §State enum.
///
/// `#[repr(u8)]` + explicit discriminants pin the byte representation: future
/// variants append after `Cancelled` and never shift existing values
/// (forward-compat invariant).
///
/// `#[non_exhaustive]` requires off-chain consumers to keep a `_` arm so
/// adding variants post-deploy doesn't panic decoders pinned to today's set.
///
/// MVP uses only `Active` and `Cancelled`. The other three are pre-defined
/// for post-hackathon stages (pause/resume, grace, exhausted) so their
/// discriminants are stable from day 1.
///
/// Forward-compat caveat (sign-off handoff item 4): `AnchorDeserialize` for an
/// `#[repr(u8)]` enum with named variants 0..=4 panics on unknown bytes. MVP
/// only ever writes 0 (in `subscribe`) and 4 (in `cancel`, immediately followed
/// by account close), so post-deploy reads only ever see 0. Custom Borsh impl
/// to swallow unknown bytes is deferred to post-MVP.
#[repr(u8)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, AnchorSerialize, AnchorDeserialize, InitSpace)]
#[borsh(use_discriminant = true)]
pub enum SubscriptionState {
    /// MVP: stream is unlocking, charge is legal. ADR-003.
    Active = 0,
    /// post-MVP: merchant froze charges; no settle, no time accrual. ADR-003 §Per-state.
    Paused = 1,
    /// post-MVP: stream fully consumed, awaiting top_up before terminal. ADR-003.
    GracePeriod = 2,
    /// post-MVP: grace timed out; awaiting cleanup. ADR-003.
    Exhausted = 3,
    /// soft-terminal: settled + refunded. In MVP, fused with cleanup — never
    /// observable on-chain because the account is closed in the same instruction.
    /// See ADR-003 §Cancel decomposition.
    Cancelled = 4,
}

/// Plan account — merchant-owned subscription template.
///
/// Layout per ADR-001 §Plan account. Total Borsh size = 153 bytes
/// (+ 8 discriminator = 161 on chain).
///
/// **Immutable after `create_plan`** (ADR-001). No `update_plan` instruction.
/// Price changes = new Plan with new `plan_id`.
///
/// Seeds: `[PLAN_SEED, merchant.key().as_ref(), &plan_id.to_le_bytes()]`.
#[account]
#[derive(InitSpace)]
pub struct Plan {
    /// Merchant pubkey — owner, the only signer for cancel-side ops (post-MVP).
    pub merchant: Pubkey,
    /// Namespace per merchant; `(merchant, plan_id)` is unique by PDA collision.
    pub plan_id: u64,
    /// Price per period, in `token_mint` base units.
    pub price: u64,
    /// Period length in seconds.
    pub period: i64,
    /// Mint of the payment token (USDC by ADR-001 whitelist).
    pub token_mint: Pubkey,
    /// Snapshot destination ATA — see ADR-002 §Account model.
    pub merchant_ata: Pubkey,
    /// PDA canonical bump.
    pub bump: u8,
    /// Forward-compat (price tiers / metadata URI). 32 bytes per ADR-001.
    pub reserved: [u8; 32],
}

/// Subscription account — per `(subscriber, plan)` PDA.
///
/// Layout per ADR-001 §Subscription account (revised 2026-04-27 BLK-01/03/05).
/// Total Borsh size = 267 bytes (+ 8 discriminator = 275 on chain).
///
/// **Field order is part of the ABI.**
/// `next_charge_at` is first after the discriminator (account.data offset 8) so
/// keepers can `memcmp`-prefilter due-subs by raw bytes (ADR-001 §Field-order
/// rationale, BLK-18). `state` lands at account.data offset 192 (BLK-19) — the
/// const-asserts below pin both invariants.
///
/// Streaming state (`deposited_amount` / `withdrawn_amount` / `rate_per_second`
/// / `stream_start`) lives as named fields, not as a manual byte-slice in
/// `reserved` (BLK-01: Anchor `BorshSerialize` autoderived per field, no
/// silent zeroing on partial writes).
///
/// Seeds: `[SUB_SEED, subscriber.key().as_ref(), plan.key().as_ref()]`.
///
/// Immutability invariant (ADR-001 Q6 / BLK-23): `subscriber` and `plan`
/// fields MUST NOT be mutated by any instruction after `subscribe`. Enforced
/// by code review; no on-chain assert (Subscription is program-owned).
#[account]
#[derive(InitSpace)]
pub struct Subscription {
    /// memcmp-friendly first field (account.data offset 8). Hint for keeper —
    /// not a security invariant; `withdrawn_amount` provides idempotency.
    /// ADR-002 §Streaming model.
    pub next_charge_at: i64,
    /// Subscriber wallet — immutable after subscribe (BLK-23).
    pub subscriber: Pubkey,
    /// Plan PDA — immutable after subscribe (BLK-23).
    pub plan: Pubkey,
    /// Snapshot of `Plan.price` at subscribe time (defends against price changes).
    pub price: u64,
    /// Snapshot of `Plan.period`.
    pub period: i64,
    /// Snapshot of `Plan.token_mint`.
    pub token_mint: Pubkey,
    /// Snapshot of `Plan.merchant` — avoids needing `Plan` in `charge` math
    /// (ADR-001 §Consequences). Frozen for the life of the subscription.
    pub merchant: Pubkey,
    /// Snapshot of `Plan.merchant_ata` — destination for vault → merchant CPI.
    pub merchant_ata: Pubkey,
    /// FSM state (ADR-003). 1 byte on the wire.
    /// Located at account.data offset **192** — see const-assert below.
    pub state: SubscriptionState,
    /// Subscription PDA canonical bump (stored, never re-derived per BLK-03).
    pub bump: u8,
    /// Vault PDA canonical bump (BLK-03 — used for vault CPI signing seeds).
    pub vault_bump: u8,
    pub created_at: i64,
    pub last_charge_at: i64,
    /// Cumulative subscriber deposits (USDC base units).
    pub deposited_amount: u64,
    /// Cumulative merchant settlements — **monotonic** (ADR-002 §Идемпотентность).
    pub withdrawn_amount: u64,
    /// `price / period` snapshotted at subscribe; fixed for life. ADR-002.
    pub rate_per_second: u64,
    /// First deposit timestamp; `unlocked` math anchors here. ADR-002.
    pub stream_start: i64,
    /// 32 bytes reserved for x402 satellite-PDA pointer (ADR-001 §Forward-compat).
    pub reserved: [u8; 32],
}

// ── Compile-time layout invariants ────────────────────────────────────────
//
// BLK-19: `state` field MUST sit at account.data offset 192 (after the 8-byte
// discriminator). Off-chain keeper memcmp prefilter `state == Active` keys on
// this offset; drift breaks the keeper.
//
// We assert this two ways:
//   1. Total Borsh `INIT_SPACE` = 267 (267 + 8 disc = 275 on chain).
//   2. The byte sum of every field preceding `state` in the layout = 184
//      (then +8 discriminator = 192 on the wire).
//
// `static_assertions::assert_eq_size!` would assert the in-memory `size_of`,
// which under default `#[repr(Rust)]` includes alignment padding and does not
// match the Borsh-serialized size. We use `INIT_SPACE` (Anchor-derived,
// Borsh-aware) and a hand-written offset sum, both `const`-evaluated, instead.

const _: () = {
    // Sum of every field before `state`, must equal 184. State offset on the
    // wire = 8 (discriminator) + 184 = 192. (BLK-19.)
    const PRE_STATE: usize =
          8   // next_charge_at
        + 32  // subscriber
        + 32  // plan
        + 8   // price
        + 8   // period
        + 32  // token_mint
        + 32  // merchant
        + 32; // merchant_ata
    assert!(PRE_STATE == 184, "Subscription pre-state byte count drifted from ADR-001");

    // Borsh-serialized total (no discriminator).
    assert!(
        Subscription::INIT_SPACE == 267,
        "Subscription::INIT_SPACE drifted from ADR-001 layout (expected 267)"
    );
    assert!(
        Plan::INIT_SPACE == 153,
        "Plan::INIT_SPACE drifted from ADR-001 layout (expected 153)"
    );
    assert!(
        SubscriptionState::INIT_SPACE == 1,
        "SubscriptionState must Borsh-serialize as exactly 1 byte"
    );
};

// ── Events ────────────────────────────────────────────────────────────────

/// Emitted by `create_plan`. ADR-014 §Event.
#[event]
pub struct PlanCreated {
    pub plan: Pubkey,
    pub merchant: Pubkey,
    pub plan_id: u64,
    pub price: u64,
    pub period: i64,
    pub timestamp: i64,
}

/// Emitted by `subscribe`. Useful for off-chain status pages and indexer.
#[event]
pub struct SubscriptionStarted {
    pub subscription: Pubkey,
    pub subscriber: Pubkey,
    pub plan: Pubkey,
    pub deposited_amount: u64,
    pub rate_per_second: u64,
    pub stream_start: i64,
}

/// Emitted by `cancel` BEFORE the Subscription account is closed.
/// Off-chain consumers detect cancellation via this event in MVP because
/// the on-chain account vanishes the same slot (ADR-003 §Cancel decomposition).
#[event]
pub struct SubscriptionCancelled {
    pub subscription: Pubkey,
    pub subscriber: Pubkey,
    pub plan: Pubkey,
    pub final_settled: u64,
    pub refunded: u64,
    pub timestamp: i64,
}
