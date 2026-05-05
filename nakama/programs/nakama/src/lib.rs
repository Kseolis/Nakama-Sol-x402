//! Nakama Protocol on-chain program.
//!
//! MVP day 1–7 surface: `create_plan` (ADR-014), `subscribe` (ADR-002),
//! `charge` (ADR-004), `cancel` (ADR-002), `cleanup` (ADR-013).
//! Cycle-4 addition: `top_up` (ADR-007).
//!
//! See `docs/architecture/adr-001-account-model.md` for layout invariants.
//!
//! # Anchor cargo features
//!
//! `anchor-lang/allow-missing-optionals` is enabled (see `Cargo.toml`) so
//! callers may OMIT trailing `Option<Account<T>>` accounts when the satellite
//! is not needed (e.g., `charge` from a stream that won't exhaust, or `cancel`
//! from `Active`). Without the feature, callers would have to pass `program_id`
//! as a placeholder pubkey for absent optional accounts — a UX papercut for the
//! TS SDK and keeper. Trade-off: NEVER add a required account after an optional
//! one in any `Accounts` struct, otherwise a present-but-unsigned trailing
//! required account would be silently absorbed as the optional's None case.
//! See ADR-007 §"Source-of-truth verification" Q9.

// Anchor 1.0.x `#[program]` macro expands to a `match` whose arms call
// `Result::Err`, which clippy 1.89 flags as `diverging_sub_expression`.
// Allow at crate root because the lint fires inside the macro expansion.
#![allow(clippy::diverging_sub_expression)]

pub mod constants;
pub mod error;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;

pub use constants::*;
pub use error::*;
pub use instructions::*;
pub use state::*;

declare_id!("HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm");

#[program]
pub mod nakama {
    use super::*;

    /// ADR-014 — merchant-signed Plan creation with USDC mint whitelist.
    pub fn create_plan(
        ctx: Context<CreatePlan>,
        plan_id: u64,
        price: u64,
        period: i64,
    ) -> Result<()> {
        instructions::create_plan::create_plan_handler(ctx, plan_id, price, period)
    }

    /// ADR-002 — subscriber inits Subscription + vault and prefunds N periods.
    pub fn subscribe(ctx: Context<Subscribe>, periods_to_prefund: u8) -> Result<()> {
        instructions::subscribe::subscribe_handler(ctx, periods_to_prefund)
    }

    /// ADR-004 — permissionless streaming withdrawal (vault → merchant_ata).
    pub fn charge(ctx: Context<Charge>) -> Result<()> {
        instructions::charge::charge_handler(ctx)
    }

    /// ADR-002 + ADR-013 + ADR-009 — polymorphic cancel (subscriber OR merchant):
    /// settle pro-rata, refund, close vault. Subscription account preserved as
    /// tombstone (cycle-3 split). Rent flow unchanged regardless of signer
    /// (vault rent → snapshotted subscriber).
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        instructions::cancel::cancel_handler(ctx)
    }

    /// ADR-013 — subscriber-only rent reclaim from {Cancelled, Exhausted}
    /// tombstone. Closes the Subscription account, lamports → subscriber.
    pub fn cleanup(ctx: Context<Cleanup>) -> Result<()> {
        instructions::cleanup::cleanup_handler(ctx)
    }

    /// ADR-007 — subscriber-signed top-up. Transfers USDC into the vault and
    /// (from `GracePeriod`) recovers the subscription back to `Active`,
    /// closing the `GracedSubscription` satellite (rent → subscriber).
    pub fn top_up(ctx: Context<TopUp>, amount: u64) -> Result<()> {
        instructions::top_up::top_up_handler(ctx, amount)
    }

    /// ADR-x402-001 — subscriber opens a PaySession satellite for x402
    /// per-request micropayments. Polymorphic facilitator delegation:
    /// settle authority is the `facilitator` pubkey passed at open, NOT
    /// the subscriber's signing key.
    pub fn open_session(
        ctx: Context<OpenSession>,
        session_id: u64,
        facilitator: Pubkey,
        reservation_cap: u64,
    ) -> Result<()> {
        instructions::open_session::open_session_handler(
            ctx,
            session_id,
            facilitator,
            reservation_cap,
        )
    }

    /// ADR-x402-001 — subscriber closes a PaySession. Anchor `close =
    /// subscriber` returns rent. NO `parent.state == Active` guard —
    /// must work from any parent state including Cancelled tombstone
    /// (R1 closure).
    pub fn close_session(ctx: Context<CloseSession>) -> Result<()> {
        instructions::close_session::close_session_handler(ctx)
    }
}
