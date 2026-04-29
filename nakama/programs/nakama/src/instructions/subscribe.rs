//! `subscribe` instruction — ADR-002 §subscribe (revised 2026-04-27).
//!
//! Subscriber-signed: init Subscription PDA + per-subscription vault, snapshot
//! Plan fields, prefund vault with `price * periods_to_prefund` via SPL CPI.
//!
//! Hard guards (sign-off blockers):
//! - BLK-02 `ZeroRatePerSecond`: `price/period` rounding to 0 → reject.
//! - BLK-07 `ZeroPeriodsToFund`: `periods_to_prefund == 0` → reject.
//! - BLK-09 `subscriber_ata` mint + authority constraints (declared below).
//! - BLK-13 `periods_to_prefund: u8` (1..=255 sufficient — 21 years of monthly).
//! - BLK-03 store `vault_bump` for cancel-time CPI signing.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{PLAN_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{Plan, Subscription, SubscriptionStarted, SubscriptionState};

/// Account validation per ADR-002 §subscribe Accounts struct sketch (BLK-09).
#[derive(Accounts)]
pub struct Subscribe<'info> {
    /// Subscriber pays rent + signs prefund.
    #[account(mut)]
    pub subscriber: Signer<'info>,

    /// Plan PDA — read-only. Seeds re-derivation guards against substitution.
    #[account(
        seeds = [
            PLAN_SEED,
            plan.merchant.as_ref(),
            &plan.plan_id.to_le_bytes(),
        ],
        bump = plan.bump,
    )]
    pub plan: Account<'info, Plan>,

    /// Token mint — must equal `plan.token_mint` snapshot (defends against mint
    /// substitution by the caller). Required as a typed account so anchor-spl's
    /// `init`+`token::mint` constraint on the vault can reference it directly.
    #[account(address = plan.token_mint)]
    pub token_mint: Account<'info, Mint>,

    /// Subscription PDA, init by subscriber.
    #[account(
        init,
        payer = subscriber,
        space = 8 + Subscription::INIT_SPACE,
        seeds = [SUB_SEED, subscriber.key().as_ref(), plan.key().as_ref()],
        bump,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Per-subscription vault — non-ATA TokenAccount with PDA authority.
    /// authority = `subscription` PDA so vault transfers must be CPI-signed
    /// with the Subscription PDA seeds (see ADR-002 §Authority CPI). Bump
    /// stored on Subscription (BLK-03).
    #[account(
        init,
        payer = subscriber,
        seeds = [VAULT_SEED, subscription.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = subscription,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// Source TokenAccount — owned by subscriber, mint = plan's token (BLK-09).
    #[account(
        mut,
        token::mint = token_mint,
        token::authority = subscriber,
    )]
    pub subscriber_ata: Account<'info, TokenAccount>,

    /// Classic SPL Token only.
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

/// ADR-002 §subscribe pseudocode (revised 2026-04-27).
pub fn subscribe_handler(ctx: Context<Subscribe>, periods_to_prefund: u8) -> Result<()> {
    // Step 1 — defence-in-depth: Plan.period > 0 (also enforced in create_plan).
    require!(ctx.accounts.plan.period > 0, NakamaError::ZeroPeriod);

    // Step 2 — BLK-07: reject zero-deposit subscriptions.
    require!(periods_to_prefund >= 1, NakamaError::ZeroPeriodsToFund);

    let plan = &ctx.accounts.plan;

    // Step 3 — derive rate. period > 0 enforced; cast safe.
    let period_u64 = plan.period as u64;
    let rate_per_second = plan
        .price
        .checked_div(period_u64)
        .ok_or(NakamaError::MathOverflow)?;

    // Step 4 — BLK-02: silent locked-stream guard.
    require!(rate_per_second >= 1, NakamaError::ZeroRatePerSecond);

    // Step 9 — total deposit = price * periods_to_prefund.
    let deposited_amount = plan
        .price
        .checked_mul(periods_to_prefund as u64)
        .ok_or(NakamaError::MathOverflow)?;

    let now = Clock::get()?.unix_timestamp;

    // Step 10 — CPI prefund: subscriber_ata → vault.
    // anchor-spl 1.0.1 Transfer accounts: from / to / authority.
    // CpiContext::new takes program_id (Pubkey) + accounts.
    // https://docs.rs/anchor-lang/1.0.1/anchor_lang/context/struct.CpiContext.html#method.new
    let cpi_accounts = Transfer {
        from: ctx.accounts.subscriber_ata.to_account_info(),
        to: ctx.accounts.vault.to_account_info(),
        authority: ctx.accounts.subscriber.to_account_info(),
    };
    let cpi_ctx = CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts);
    token::transfer(cpi_ctx, deposited_amount)?;

    // Steps 5–8, 11–15 — populate Subscription state.
    let sub = &mut ctx.accounts.subscription;
    sub.next_charge_at = now
        .checked_add(plan.period)
        .ok_or(NakamaError::MathOverflow)?;
    sub.subscriber = ctx.accounts.subscriber.key();
    sub.plan = plan.key();
    sub.price = plan.price;
    sub.period = plan.period;
    sub.token_mint = plan.token_mint;
    sub.merchant = plan.merchant;
    sub.merchant_ata = plan.merchant_ata;
    sub.state = SubscriptionState::Active;
    sub.bump = ctx.bumps.subscription;
    sub.vault_bump = ctx.bumps.vault; // BLK-03
    sub.created_at = now;
    sub.last_charge_at = 0;
    sub.deposited_amount = deposited_amount;
    sub.withdrawn_amount = 0;
    sub.rate_per_second = rate_per_second;
    sub.stream_start = now;
    sub.reserved = [0u8; 32];

    emit!(SubscriptionStarted {
        subscription: sub.key(),
        subscriber: sub.subscriber,
        plan: sub.plan,
        deposited_amount,
        rate_per_second,
        stream_start: now,
    });

    Ok(())
}
