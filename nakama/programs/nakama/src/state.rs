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

/// Subscription FSM state — see ADR-003 §State enum, ADR-001 §`#[non_exhaustive]`
/// scope clarification.
///
/// `#[repr(u8)]` + explicit discriminants pin the byte representation: future
/// variants append after `Cancelled` and never shift existing values
/// (forward-compat invariant).
///
/// MVP uses only `Active` and `Cancelled`. The other three are pre-defined for
/// post-hackathon stages (pause/resume, grace, exhausted) so their discriminants
/// are stable from day 1.
///
/// # `#[non_exhaustive]` covers Rust pattern-match exhaustiveness only
///
/// The attribute forces external Rust crate consumers (off-chain readers,
/// indexer, keeper) that `match` this enum to keep a `_` arm. Adding a future
/// variant in a redeploy will not break their compilation — the `_` arm
/// absorbs the unknown discriminant. This is a **compile-time** guarantee for
/// downstream crates.
///
/// # Borsh-decode panic on unknown discriminant is a separate concern
///
/// `AnchorDeserialize` (Borsh-derived) for a `#[repr(u8)]` enum with named
/// variants 0..=4 **panics at runtime** when reading a byte ≥ 5 (e.g. after
/// a future redeploy adds a 6th variant and an old client reads a new
/// account). `#[non_exhaustive]` does **not** mitigate this — it only governs
/// pattern-match completeness, not deserialization.
///
/// MVP mitigation: only `state = 0` (written in `subscribe`) and `state = 4`
/// (written in `cancel`, immediately followed by `close_account` in the same
/// instruction) ever persist on-chain. `state = 4` is never observable
/// post-tx because the account is closed in the same slot — post-deploy
/// reads in MVP only ever see `state = 0`. A custom `BorshDeserialize` impl
/// that swallows unknown bytes (instead of panicking) is deferred to
/// post-MVP (sign-off handoff item 4 / security audit F-3).
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
    /// soft-terminal: settled + refunded; vault closed; Subscription account
    /// preserved as tombstone until `cleanup`. Post-ADR-013 split this byte
    /// **is observable on-chain** (cycle-2 MVP closed the account in the same
    /// ix; cycle-3 keeps it alive). See ADR-013 §"Cancel handler" and
    /// ADR-003 §"Cancel decomposition".
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
    const PRE_STATE: usize = 8   // next_charge_at
        + 32  // subscriber
        + 32  // plan
        + 8   // price
        + 8   // period
        + 32  // token_mint
        + 32  // merchant
        + 32; // merchant_ata
    assert!(
        PRE_STATE == 184,
        "Subscription pre-state byte count drifted from ADR-001"
    );

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

/// Satellite PDA, mirroring the `PausedSubscription` pattern from ADR-006.
/// Created when `charge` exhausts the stream (`withdrawn_amount ==
/// deposited_amount`) — flips parent Subscription state to `GracePeriod` and
/// snapshots the recovery window. Closed (rent → subscriber) on `top_up` from
/// `GracePeriod` or `cancel` from `GracePeriod`. ADR-007 §"Storage decision".
///
/// Layout: 32 (subscription) + 8 (entered_grace_at) + 8 (grace_until) = 48
/// Borsh; on-chain = 8 disc + 48 = 56 bytes (rent ~0.00040 SOL, recoverable on
/// close). `INIT_SPACE` const-asserted at 48 below.
///
/// Seeds: `[GRACE_SEED, subscription.key().as_ref()]`.
///
/// Passive expiry: state byte stays `GracePeriod` past `grace_until` if no
/// cancel/top_up trigger fires; off-chain `ComputedStatus::GraceExpired` is
/// derived from `(state, grace_until, now)` per ADR-007 boundary contract.
/// No on-chain `expire_grace` instruction (rejected alternative (h)).
#[account]
#[derive(InitSpace)]
pub struct GracedSubscription {
    /// Back-ref to the parent Subscription PDA — convenience for off-chain
    /// joins (indexer / x402 facilitator) so a single
    /// `getProgramAccounts` filter on this account type yields fully-resolved
    /// rows without a second lookup. ADR-007 §"Storage decision".
    pub subscription: Pubkey,
    /// Snapshot of `Clock::unix_timestamp` at the auto-transition in `charge`
    /// tail. ADR-007 §I-CHARGE-1.
    pub entered_grace_at: i64,
    /// `entered_grace_at + GRACE_DURATION`. Off-chain consumers compare
    /// against `now` to differentiate `InGrace` vs `GraceExpired`.
    /// ADR-007 §I-GRACE-2.
    pub grace_until: i64,
}

const _: () = {
    // 32 + 8 + 8 = 48. Pinned by ADR-007 §"Storage decision".
    assert!(
        GracedSubscription::INIT_SPACE == 48,
        "GracedSubscription::INIT_SPACE drifted from ADR-007 layout (expected 48)"
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

/// Emitted by `cancel`. Post-ADR-013 split, the Subscription account is
/// preserved as a tombstone (`state == Cancelled`) until subscriber calls
/// `cleanup` — off-chain consumers can read `state` directly via
/// `getProgramAccounts` filter on byte 192, in addition to listening for
/// this event. ADR-013 §"x402 forward-compat".
///
/// ADR-009 extension: `cancelled_by` records the polymorphic actor
/// (subscriber OR merchant) so off-chain analytics can split churn (subscriber)
/// from offboarding/compliance (merchant) without inferring from auxiliary
/// state. `had_graced_satellite` echoes whether a `GracedSubscription` was
/// closed as part of cancel — keeper accounting hint.
#[event]
pub struct SubscriptionCancelled {
    pub subscription: Pubkey,
    pub subscriber: Pubkey,
    pub plan: Pubkey,
    pub merchant: Pubkey,
    /// Polymorphic cancel actor — equal to either `subscriber` or `merchant`.
    /// ADR-009 §"Telemetry: event log".
    pub cancelled_by: Pubkey,
    pub final_settled: u64,
    pub refunded: u64,
    pub had_graced_satellite: bool,
    pub timestamp: i64,
}

/// Emitted by `charge` after a successful CPI transfer (ADR-004 §5).
/// `withdrawn_total` is the post-update cumulative settlement — single source
/// of truth for keepers (event log beats account-state read because of races
/// with parallel keepers; ADR-004 §7).
#[event]
pub struct SubscriptionCharged {
    pub subscription: Pubkey,
    pub amount: u64,
    pub withdrawn_total: u64,
    pub timestamp: i64,
}

/// Emitted by `cleanup` immediately before the Subscription account is closed
/// (ADR-013 §"Cleanup handler"). Off-chain consumers use this event to update
/// indexer rows from "pending-cleanup tombstone" to "closed".
#[event]
pub struct SubscriptionCleaned {
    pub subscription: Pubkey,
    pub rent_returned_to: Pubkey,
    pub timestamp: i64,
}

/// Emitted by the `charge` tail when stream exhaustion auto-transitions
/// `Active → GracePeriod` and the `GracedSubscription` satellite is created.
/// ADR-007 §"charge handler tail" + §I-CHARGE-1.
#[event]
pub struct GraceEntered {
    pub subscription: Pubkey,
    pub entered_grace_at: i64,
    pub grace_until: i64,
}

/// Emitted by `top_up` when the subscriber rescues a `GracePeriod`
/// subscription back to `Active`. Satellite is closed in the same ix
/// (rent → subscriber). ADR-007 §"top_up handler" + §I-TOPUP-6.
#[event]
pub struct GraceRecovered {
    pub subscription: Pubkey,
    pub top_up_amount: u64,
    pub new_deposited: u64,
}

// ── ADR-x402-001 — PaySession satellite layer ────────────────────────────
//
// Satellite-PDA pattern (blueprint from PausedSubscription / GracedSubscription):
// PaySession is a child account of Subscription, parented by the seed prefix
// `[b"pay_session", subscription.key, session_id_le]`. Subscription layout
// itself is untouched — x402 layer is zero-footprint on the parent
// (ADR-x402-001 §Decision).
//
// Layout: 202 bytes payload + 8-byte discriminator = 210 bytes total.
// const-asserted below to detect drift.

/// Internal FSM for PaySession — three states. **Independent** from
/// `SubscriptionState` (parent FSM stays unchanged per ADR-003 invariant).
///
/// Discriminants are wire-stable; never reorder, never reuse — append new
/// variants with `= 3, 4, ...` at the end. `#[non_exhaustive]` is hygiene
/// matching the SubscriptionState pattern (forward-compat without semver
/// break).
///
/// `Settling` is a **transient** state — observable on disk only if a settle
/// instruction crashed mid-CPI. Recovery via `force_close_session` (R3,
/// post-MVP).
#[repr(u8)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, AnchorSerialize, AnchorDeserialize)]
#[borsh(use_discriminant = true)]
pub enum PaySessionState {
    /// Active session, settle_usage allowed (subject to `parent.state == Active`).
    Open = 0,
    /// Transient lock, set immediately before settle CPI; cleared post-CPI.
    Settling = 1,
    /// Terminal — rent reclaimed, PDA closed.
    Closed = 2,
}

/// PaySession satellite — per-session ledger for x402 micropayment streams.
///
/// One Subscription → N concurrent PaySessions (ADR-x402-001 Q1, u64 nonce).
/// Snapshot pattern follows ADR-001 — `merchant`, `merchant_ata` copied from
/// parent at `open_session` time so settle CPI doesn't need parent traversal
/// for routing data.
///
/// Layout (ADR-x402-001 §"PaySession PDA Layout"):
///
/// | Offset | Size | Field            |
/// |--------|------|------------------|
/// | 0      | 32   | subscription     |
/// | 32     | 32   | merchant         |
/// | 64     | 32   | merchant_ata     |
/// | 96     | 32   | facilitator      |
/// | 128    | 8    | session_id       |
/// | 136    | 8    | opened_at        |
/// | 144    | 8    | last_settle_at   |
/// | 152    | 8    | usage_amount     |
/// | 160    | 8    | reservation_cap  |
/// | 168    | 1    | state            |
/// | 169    | 1    | bump             |
/// | 170    | 32   | reserved         |
///
/// Total: 202 bytes (+8 discriminator = 210 on the wire).
#[account]
#[derive(InitSpace)]
pub struct PaySession {
    /// Parent Subscription PDA (back-reference for off-chain joins).
    /// Defence-in-depth above the PDA seed constraint — handler verifies
    /// `pay_session.subscription == parent.key()`.
    pub subscription: Pubkey,
    /// Snapshot of `parent.merchant` at open_session — settlement destination
    /// owner identity, for off-chain analytics. Immutable post-init.
    pub merchant: Pubkey,
    /// Snapshot of `parent.merchant_ata` — settle CPI destination ATA.
    /// Immutable; CPI handler asserts `merchant_ata.key() == this`.
    pub merchant_ata: Pubkey,
    /// Subscriber-chosen authority for `settle_usage`. ADR-x402-001 Q5
    /// Option A — on-chain delegation. Rotation requires close + reopen.
    pub facilitator: Pubkey,
    /// u64 nonce from PDA seeds, mirrored on-chain for off-chain readability.
    /// Must match the seed `&session_id.to_le_bytes()` (defensive cross-check
    /// in handler — Anchor seed validation already guarantees uniqueness).
    pub session_id: u64,
    /// `Clock::get().unix_timestamp` at `open_session`.
    pub opened_at: i64,
    /// Last settle timestamp; `0` means "never settled".
    pub last_settle_at: i64,
    /// Cumulative settle volume across this session (monotonic non-decreasing).
    /// Per-session ledger — cross-session aggregates read from
    /// `parent.withdrawn_amount` (single source of truth, ADR-002).
    pub usage_amount: u64,
    /// Soft-cap on total session usage. `0` means "unlimited up to escrow".
    /// Enforced by handler (`usage_amount + amount <= reservation_cap`).
    /// ADR-x402-001 §Adversarial 3 / §8 — bounds compromised-key damage.
    pub reservation_cap: u64,
    /// `PaySessionState` discriminant. Open=0, Settling=1, Closed=2.
    pub state: u8,
    /// PDA bump cache (BLK-03 — never re-derive on subsequent calls).
    pub bump: u8,
    /// Forward-compat: variable rate, expiry, fee-split partner.
    /// 32 bytes per ADR-x402-001 §"Forward compat".
    pub reserved: [u8; 32],
}

// Compile-time invariant: PaySession payload must be exactly 202 bytes.
// Drift here surfaces as a build failure; the test
// `pay_session_init_space_is_202_bytes` provides a runtime mirror.
const _: () = {
    if PaySession::INIT_SPACE != 202 {
        panic!("ADR-x402-001 PaySession::INIT_SPACE drift — must be 202 bytes");
    }
};

/// Emitted by `open_session`. Off-chain indexers track session creation,
/// expected facilitator, and reservation cap.
#[event]
pub struct PaySessionOpened {
    pub pay_session: Pubkey,
    pub subscription: Pubkey,
    pub facilitator: Pubkey,
    pub reservation_cap: u64,
    pub timestamp: i64,
}

/// Emitted by `settle_usage` after CPI completes. `cumulative_usage` is the
/// post-update `pay_session.usage_amount` — single-source readout for
/// facilitator off-chain accounting.
#[event]
pub struct UsageSettled {
    pub pay_session: Pubkey,
    pub subscription: Pubkey,
    pub amount: u64,
    pub cumulative_usage: u64,
    pub timestamp: i64,
}

/// Emitted by `close_session` immediately before Anchor's `close = subscriber`
/// constraint deallocates the PDA. `final_usage` mirrors the last
/// `cumulative_usage` for audit trail closure.
#[event]
pub struct PaySessionClosed {
    pub pay_session: Pubkey,
    pub subscription: Pubkey,
    pub final_usage: u64,
    pub rent_returned_to: Pubkey,
    pub timestamp: i64,
}
