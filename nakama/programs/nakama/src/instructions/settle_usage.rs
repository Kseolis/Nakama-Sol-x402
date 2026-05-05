//! `settle_usage` instruction — ADR-x402-001 §"settle_usage".
//!
//! Facilitator-signed CPI transfer `vault → merchant_ata` with shared
//! accounting on `parent.withdrawn_amount` (ADR-002 single source of truth).
//!
//! **Composability with `charge`**: both ix mutate the same
//! `parent.withdrawn_amount`. Streaming math from charge_handler §3 is
//! mirrored here verbatim — drift would create double-spend. Composability
//! suite (`tests/x402_settle_composability.rs`) pins the invariant
//! `parent.withdrawn_amount == Σ(charge claimable) + Σ(settle amounts)`.
//!
//! Hard guards:
//! - parent.state == Active (boundary contract from ADR-007)
//! - pay_session.state == Open (Settling stuck → R3 force_close, post-MVP)
//! - amount > 0 (IllegalAmountForSettle)
//! - facilitator constraint via Anchor: pay_session.facilitator == signer
//! - reservation_cap check (if cap > 0, usage_amount + amount <= cap)
//! - amount <= unlocked - withdrawn (InsufficientUnlockedFunds — same math
//!   as charge_handler §3)
//!
//! Side effects (in order):
//! 1. Transient lock: pay_session.state = Settling (defense-in-depth
//!    against nested CPI re-entrancy; should never persist post-tx)
//! 2. CPI Transfer vault → merchant_ata, signed by Subscription PDA
//!    (vault authority)
//! 3. parent.withdrawn_amount += amount (monotonic)
//! 4. pay_session.usage_amount += amount
//! 5. pay_session.last_settle_at = now
//! 6. pay_session.state = Open (unlock)
//! 7. Emit UsageSettled

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::constants::{PAY_SESSION_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{PaySession, PaySessionState, Subscription, SubscriptionState, UsageSettled};

/// Account validation per ADR-x402-001 §"settle_usage" Accounts struct.
///
/// Wire order (canonical IDL): parent, pay_session, vault, merchant_ata,
/// facilitator, token_program. Parent declared FIRST so pay_session's
/// `subscription == parent.key()` constraint resolves against an
/// already-loaded account (Anchor forward-reference rule).
#[derive(Accounts)]
pub struct SettleUsage<'info> {
    /// Parent Subscription. `mut` because handler mutates
    /// `withdrawn_amount`. Boundary contract `state == Active` enforced
    /// in handler.
    #[account(
        mut,
        seeds = [SUB_SEED, parent.subscriber.as_ref(), parent.plan.as_ref()],
        bump = parent.bump,
    )]
    pub parent: Account<'info, Subscription>,

    /// PaySession satellite. Authority delegated to `facilitator`
    /// (snapshotted at open_session); `pay_session.facilitator == signer`
    /// constraint enforces it. Cross-session attacks (Adversarial §9)
    /// blocked by per-PDA seed binding.
    #[account(
        mut,
        seeds = [PAY_SESSION_SEED, parent.key().as_ref(), &pay_session.session_id.to_le_bytes()],
        bump = pay_session.bump,
        constraint = pay_session.subscription == parent.key()
            @ NakamaError::PaySessionParentMismatch,
        constraint = pay_session.facilitator == facilitator.key()
            @ NakamaError::UnauthorizedFacilitator,
    )]
    pub pay_session: Account<'info, PaySession>,

    /// Vault — source of CPI transfer. Authority is the Subscription PDA
    /// (set in subscribe via `token::authority = subscription`); we sign
    /// CPI with subscription seeds.
    #[account(
        mut,
        seeds = [VAULT_SEED, parent.key().as_ref()],
        bump = parent.vault_bump,
        token::mint = parent.token_mint,
        token::authority = parent,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// Merchant settlement destination. Address pinned to PaySession
    /// snapshot (NOT parent.merchant_ata directly — they're equal at
    /// open_session, but using the snapshot lets future ADRs route per
    /// session if needed).
    #[account(
        mut,
        address = pay_session.merchant_ata @ NakamaError::PaySessionMerchantAtaMismatch,
        token::mint = parent.token_mint,
    )]
    pub merchant_ata: Box<Account<'info, TokenAccount>>,

    pub facilitator: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn settle_usage_handler(ctx: Context<SettleUsage>, amount: u64) -> Result<()> {
    // 1. Amount sanity.
    require!(amount > 0, NakamaError::IllegalAmountForSettle);

    // 2. Boundary contract — parent.state == Active.
    {
        let parent = &ctx.accounts.parent;
        require!(
            parent.state == SubscriptionState::Active,
            NakamaError::ParentNotActive
        );
    }

    // 3. PaySession state guard (Settling = stuck — refuse).
    {
        let s = &ctx.accounts.pay_session;
        require!(
            s.state == PaySessionState::Open as u8,
            NakamaError::IllegalStateForSettle
        );
    }

    // 4. Reservation cap (if set).
    {
        let s = &ctx.accounts.pay_session;
        if s.reservation_cap > 0 {
            let new_total = s
                .usage_amount
                .checked_add(amount)
                .ok_or(NakamaError::ArithmeticOverflow)?;
            require!(
                new_total <= s.reservation_cap,
                NakamaError::ReservationCapExceeded
            );
        }
    }

    // 5. Streaming math — mirrors charge_handler §3 (ADR-002). u128 intermediate
    //    avoids overflow on multi-year streams; min() caps at deposited.
    let now = Clock::get()?.unix_timestamp;
    let parent_view = &ctx.accounts.parent;
    require!(now >= parent_view.stream_start, NakamaError::ClockBackwards);
    let stream_start = parent_view.stream_start;
    let deposited_amount = parent_view.deposited_amount;
    let withdrawn_amount = parent_view.withdrawn_amount;
    let rate_per_second = parent_view.rate_per_second;
    let parent_pubkey = parent_view.key();
    let parent_bump = parent_view.bump;
    let subscriber_pubkey = parent_view.subscriber;
    let plan_pubkey = parent_view.plan;
    let token_mint = parent_view.token_mint;
    let _ = token_mint; // doc-anchor

    let elapsed = (now - stream_start) as u64;
    let unlocked_unbounded = (elapsed as u128)
        .checked_mul(rate_per_second as u128)
        .ok_or(NakamaError::ArithmeticOverflow)?;
    let unlocked = u128::min(unlocked_unbounded, deposited_amount as u128) as u64;
    let parent_remaining = unlocked
        .checked_sub(withdrawn_amount)
        .ok_or(NakamaError::ArithmeticOverflow)?;
    require!(
        amount <= parent_remaining,
        NakamaError::InsufficientUnlockedFunds
    );

    // 6. Transient lock — Settling. Defense-in-depth against nested CPI
    //    re-entrancy via Token Program callback (theoretically impossible,
    //    but the byte costs nothing and matches ADR §"Internal FSM").
    {
        let s = &mut ctx.accounts.pay_session;
        s.state = PaySessionState::Settling as u8;
    }

    // 7. CPI Transfer vault → merchant_ata. Authority = Subscription PDA
    //    (vault.authority); sign with subscription seeds (BLK-14 manual seeds).
    let sub_seeds: &[&[u8]] = &[
        SUB_SEED,
        subscriber_pubkey.as_ref(),
        plan_pubkey.as_ref(),
        &[parent_bump],
    ];
    let sub_signer_seeds: &[&[&[u8]]] = &[sub_seeds];

    let cpi_accounts = Transfer {
        from: ctx.accounts.vault.to_account_info(),
        to: ctx.accounts.merchant_ata.to_account_info(),
        authority: ctx.accounts.parent.to_account_info(),
    };
    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.key(),
        cpi_accounts,
        sub_signer_seeds,
    );
    token::transfer(cpi_ctx, amount)?;

    // 8. State updates — single source of truth on parent.withdrawn_amount.
    {
        let parent = &mut ctx.accounts.parent;
        parent.withdrawn_amount = parent
            .withdrawn_amount
            .checked_add(amount)
            .ok_or(NakamaError::ArithmeticOverflow)?;
    }

    let cumulative_usage;
    {
        let s = &mut ctx.accounts.pay_session;
        s.usage_amount = s
            .usage_amount
            .checked_add(amount)
            .ok_or(NakamaError::ArithmeticOverflow)?;
        s.last_settle_at = now;
        s.state = PaySessionState::Open as u8; // unlock
        cumulative_usage = s.usage_amount;
    }

    let pay_session_pubkey = ctx.accounts.pay_session.key();

    emit!(UsageSettled {
        pay_session: pay_session_pubkey,
        subscription: parent_pubkey,
        amount,
        cumulative_usage,
        timestamp: now,
    });

    Ok(())
}
