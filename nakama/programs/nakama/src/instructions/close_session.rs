//! `close_session` instruction — ADR-x402-001 §"close_session".
//!
//! Subscriber-only closure of an open PaySession. Anchor `close = subscriber`
//! returns the PDA's rent to the subscriber.
//!
//! **Notably absent**: `parent.state == Active` guard. ADR-x402-001 R1
//! closure — close must work from any parent state including Cancelled
//! tombstone, otherwise satellites become orphaned rent waste after
//! cancel.
//!
//! Hard guards:
//! - parent `has_one = subscriber @ UnauthorizedClose` — only subscription
//!   owner may close
//! - `pay_session.subscription == parent.key()` — defence-in-depth above
//!   the PDA seeds (cross-session replay defense)
//! - `pay_session.state == Open` — Settling state is transient; persistent
//!   Settling means the previous settle crashed mid-CPI. Recovery via
//!   `force_close_session` (R3, post-MVP).
//!
//! Side effects:
//! - PDA closed (Anchor `close = subscriber`)
//! - lamports → subscriber
//! - emit `PaySessionClosed`

use anchor_lang::prelude::*;

use crate::constants::{PAY_SESSION_SEED, SUB_SEED};
use crate::error::NakamaError;
use crate::state::{PaySession, PaySessionClosed, PaySessionState, Subscription};

#[derive(Accounts)]
pub struct CloseSession<'info> {
    /// Parent Subscription — read-only here (seed-derivation source +
    /// has_one anchor for subscriber identity). Declared BEFORE
    /// `pay_session` so the latter's `pay_session.subscription == parent`
    /// constraint resolves against an already-loaded account (Anchor
    /// forward-reference rule, learned ADR-009 cycle).
    #[account(
        seeds = [SUB_SEED, parent.subscriber.as_ref(), parent.plan.as_ref()],
        bump = parent.bump,
        has_one = subscriber @ NakamaError::UnauthorizedClose,
    )]
    pub parent: Account<'info, Subscription>,

    /// PaySession satellite — closed, rent → subscriber.
    #[account(
        mut,
        seeds = [PAY_SESSION_SEED, parent.key().as_ref(), &pay_session.session_id.to_le_bytes()],
        bump = pay_session.bump,
        constraint = pay_session.subscription == parent.key()
            @ NakamaError::PaySessionParentMismatch,
        close = subscriber,
    )]
    pub pay_session: Account<'info, PaySession>,

    #[account(mut)]
    pub subscriber: Signer<'info>,
}

pub fn close_session_handler(ctx: Context<CloseSession>) -> Result<()> {
    let s = &ctx.accounts.pay_session;

    // Settling state is transient by design — observable on disk only if a
    // prior settle_usage crashed mid-CPI. We refuse to close from Settling
    // because we cannot tell if the settle accounting was applied or not.
    // Recovery path is R3 (post-MVP `force_close_session`).
    require!(
        s.state == PaySessionState::Open as u8,
        NakamaError::IllegalStateForClose
    );

    let now = Clock::get()?.unix_timestamp;

    emit!(PaySessionClosed {
        pay_session: s.key(),
        subscription: ctx.accounts.parent.key(),
        final_usage: s.usage_amount,
        rent_returned_to: ctx.accounts.subscriber.key(),
        timestamp: now,
    });

    // Anchor `close = subscriber` does the deallocation + lamport transfer
    // post-handler. No manual close call here.
    Ok(())
}
