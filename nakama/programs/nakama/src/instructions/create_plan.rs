//! `create_plan` instruction — ADR-014.
//!
//! Merchant-signed init of a `Plan` PDA. Closes BLK-12: every input that
//! could reach an attacker (signer / merchant_ata mint / merchant_ata owner)
//! is validated by Anchor constraints declaratively.

use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{PLAN_SEED, USDC_MINT};
use crate::error::NakamaError;
use crate::state::{Plan, PlanCreated};

/// Account validation matrix per ADR-014 §Accounts struct sketch.
///
/// - `merchant` is `Signer<'info>` → consent + rent payer (BLK-12).
/// - `plan` PDA seeds bind to `merchant` + `plan_id` → cross-merchant
///   collisions impossible.
/// - `merchant_ata` constraints validate mint = USDC and owner = merchant
///   (BLK-12). `Account<'info, TokenAccount>` itself enforces program
///   ownership = SPL Token (Anchor built-in).
/// - `Program<'info, Token>` rejects Token-2022 by program-ID equality
///   (anchor-spl built-in). Returns `InvalidProgramId` on mismatch.
#[derive(Accounts)]
#[instruction(plan_id: u64, _price: u64, _period: i64)]
pub struct CreatePlan<'info> {
    /// Merchant — signer + rent payer.
    #[account(mut)]
    pub merchant: Signer<'info>,

    /// `Plan` PDA, init by merchant.
    #[account(
        init,
        payer = merchant,
        space = 8 + Plan::INIT_SPACE,
        seeds = [PLAN_SEED, merchant.key().as_ref(), &plan_id.to_le_bytes()],
        bump,
    )]
    pub plan: Account<'info, Plan>,

    /// Snapshot destination ATA — must hold USDC and be owned by merchant.
    /// `address = USDC_MINT` constraint also locks the mint by pubkey.
    #[account(address = USDC_MINT)]
    pub token_mint: Account<'info, Mint>,

    /// `token::mint` and `token::authority` close BLK-12.
    #[account(
        token::mint = token_mint,
        token::authority = merchant,
    )]
    pub merchant_ata: Account<'info, TokenAccount>,

    /// Classic SPL Token only — Token-2022 program ID is rejected by Anchor.
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// ADR-014 §Instruction signature.
pub fn create_plan_handler(
    ctx: Context<CreatePlan>,
    plan_id: u64,
    price: u64,
    period: i64,
) -> Result<()> {
    // Defence-in-depth — `subscribe` re-checks `period > 0`, `create_plan`
    // catches it first to reject degenerate plans at create time.
    require!(period > 0, NakamaError::ZeroPeriod);
    require!(price > 0, NakamaError::ZeroPrice);

    let plan = &mut ctx.accounts.plan;
    plan.merchant = ctx.accounts.merchant.key();
    plan.plan_id = plan_id;
    plan.price = price;
    plan.period = period;
    plan.token_mint = ctx.accounts.token_mint.key();
    plan.merchant_ata = ctx.accounts.merchant_ata.key();
    plan.bump = ctx.bumps.plan;
    plan.reserved = [0u8; 32];

    emit!(PlanCreated {
        plan: plan.key(),
        merchant: plan.merchant,
        plan_id,
        price,
        period,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}
