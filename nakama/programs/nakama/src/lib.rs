//! Nakama Protocol on-chain program.
//!
//! MVP day 1–7 surface: `create_plan` (ADR-014), `subscribe` (ADR-002),
//! `charge` (ADR-004), `cancel` (ADR-002).
//!
//! See `docs/architecture/adr-001-account-model.md` for layout invariants.

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

    /// ADR-002 + ADR-013 — subscriber-only cancel: settle pro-rata, refund,
    /// close vault. Subscription account preserved as tombstone (cycle-3 split).
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        instructions::cancel::cancel_handler(ctx)
    }

    /// ADR-013 — subscriber-only rent reclaim from {Cancelled, Exhausted}
    /// tombstone. Closes the Subscription account, lamports → subscriber.
    pub fn cleanup(ctx: Context<Cleanup>) -> Result<()> {
        instructions::cleanup::cleanup_handler(ctx)
    }
}
