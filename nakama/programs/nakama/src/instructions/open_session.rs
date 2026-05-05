//! `open_session` instruction — ADR-x402-001 §"open_session".
//!
//! Subscriber initiates a new PaySession satellite under their Subscription.
//! Snapshots merchant routing data (merchant, merchant_ata) from parent so
//! later `settle_usage` calls don't re-traverse the parent for routing.
//!
//! Hard guards:
//! - `parent.state == Active` (boundary contract from ADR-007 — single check
//!   covers Paused/GracePeriod/Cancelled/Exhausted)
//! - `has_one = subscriber @ UnauthorizedOpenSession` — declarative signer policy
//! - `reservation_cap == 0 || reservation_cap <= parent.deposited - parent.withdrawn`
//!   — caps damage from compromised facilitator (ADR-x402-001 §Adversarial 3 / 8)
//!
//! Side effects:
//! - Init PaySession PDA, payer = subscriber, ~0.00198 SOL rent
//! - Snapshot subscription / merchant / merchant_ata
//! - state = Open, opened_at = now, last_settle_at = 0, usage_amount = 0
//! - Emit `PaySessionOpened`

use anchor_lang::prelude::*;

use crate::constants::{PAY_SESSION_SEED, SUB_SEED};
use crate::error::NakamaError;
use crate::state::{
    PaySession, PaySessionOpened, PaySessionState, Subscription, SubscriptionState,
};

#[derive(Accounts)]
#[instruction(session_id: u64)]
pub struct OpenSession<'info> {
    /// Parent Subscription. `has_one = subscriber` enforces the snapshotted
    /// subscriber matches the signer; non-Active arms rejected by handler
    /// (ADR-x402-001 §"Boundary contracts").
    #[account(
        seeds = [SUB_SEED, parent.subscriber.as_ref(), parent.plan.as_ref()],
        bump = parent.bump,
        has_one = subscriber @ NakamaError::UnauthorizedOpenSession,
    )]
    pub parent: Account<'info, Subscription>,

    /// PaySession satellite — initialized, payer = subscriber. Anchor space
    /// sourced from `INIT_SPACE` derive (202 bytes payload + 8 disc).
    #[account(
        init,
        payer = subscriber,
        space = 8 + PaySession::INIT_SPACE,
        seeds = [PAY_SESSION_SEED, parent.key().as_ref(), &session_id.to_le_bytes()],
        bump,
    )]
    pub pay_session: Account<'info, PaySession>,

    #[account(mut)]
    pub subscriber: Signer<'info>,

    pub system_program: Program<'info, System>,
}

pub fn open_session_handler(
    ctx: Context<OpenSession>,
    session_id: u64,
    facilitator: Pubkey,
    reservation_cap: u64,
) -> Result<()> {
    let parent = &ctx.accounts.parent;

    // Boundary contract — single guard covers Paused/Grace/Cancelled/Exhausted.
    require!(
        parent.state == SubscriptionState::Active,
        NakamaError::ParentNotActive
    );

    // reservation_cap sanity: 0 means "unlimited up to escrow"; otherwise it
    // must not exceed the remaining escrow window (deposited - withdrawn).
    let remaining = parent
        .deposited_amount
        .checked_sub(parent.withdrawn_amount)
        .ok_or(NakamaError::ArithmeticOverflow)?;
    require!(
        reservation_cap == 0 || reservation_cap <= remaining,
        NakamaError::ReservationCapExceedsEscrow
    );

    let now = Clock::get()?.unix_timestamp;
    let parent_key = parent.key();
    let parent_merchant = parent.merchant;
    let parent_merchant_ata = parent.merchant_ata;

    let s = &mut ctx.accounts.pay_session;
    s.subscription = parent_key;
    s.merchant = parent_merchant;
    s.merchant_ata = parent_merchant_ata;
    s.facilitator = facilitator;
    s.session_id = session_id;
    s.opened_at = now;
    s.last_settle_at = 0;
    s.usage_amount = 0;
    s.reservation_cap = reservation_cap;
    s.state = PaySessionState::Open as u8;
    s.bump = ctx.bumps.pay_session;
    // s.reserved is already zeroed by `init`

    emit!(PaySessionOpened {
        pay_session: s.key(),
        subscription: parent_key,
        facilitator,
        reservation_cap,
        timestamp: now,
    });
    Ok(())
}
