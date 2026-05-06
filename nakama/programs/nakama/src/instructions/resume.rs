//! `resume` instruction — ADR-006 §"Resume handler".
//!
//! Merchant-only. Reads `paused_at` from satellite, computes
//! `pause_duration = now - paused_at`, shifts `stream_start +=
//! pause_duration` (time-frozen continuity invariant), closes satellite
//! (rent → merchant per §"Symmetry: merchant authority = merchant rent
//! custody"), flips state Paused → Active.
//!
//! Continuity proof (ADR-006 §6):
//!   Before pause: U_p = rate * (paused_at - stream_start)
//!   After resume: stream_start' = stream_start + (now - paused_at)
//!   Therefore U(now) = rate * (now - stream_start') = U_p ✓
//!
//! Hard guards:
//! - `subscription.state == Paused` (FSM — refuses double-resume)
//! - `has_one = merchant @ UnauthorizedResume`
//! - `paused_satellite.subscription == subscription.key()` (defense-in-depth
//!   above PDA seeds)

use anchor_lang::prelude::*;

use crate::constants::{PAUSED_SUB_SEED, SUB_SEED};
use crate::error::NakamaError;
use crate::state::{PausedSubscription, Subscription, SubscriptionResumed, SubscriptionState};

#[derive(Accounts)]
pub struct Resume<'info> {
    /// Subscription PDA — mut for state byte flip + stream_start shift.
    /// Declared BEFORE `paused_satellite` so the latter's
    /// `paused_satellite.subscription == subscription.key()` constraint
    /// resolves against an already-loaded account (Anchor forward-reference
    /// rule, learned ADR-009 cycle).
    #[account(
        mut,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
        has_one = merchant @ NakamaError::UnauthorizedResume,
    )]
    pub subscription: Account<'info, Subscription>,

    /// PausedSubscription satellite — closed, rent → merchant (who paid
    /// at pause). ADR-006 §"Symmetry" — merchant authority + merchant rent
    /// custody.
    #[account(
        mut,
        seeds = [PAUSED_SUB_SEED, subscription.key().as_ref()],
        bump = paused_satellite.bump,
        constraint = paused_satellite.subscription == subscription.key()
            @ NakamaError::IllegalStateForResume,
        close = merchant,
    )]
    pub paused_satellite: Account<'info, PausedSubscription>,

    #[account(mut)]
    pub merchant: Signer<'info>,
}

pub fn resume_handler(ctx: Context<Resume>) -> Result<()> {
    // FSM guard — only Paused is resumable.
    {
        let sub = &ctx.accounts.subscription;
        require!(
            sub.state == SubscriptionState::Paused,
            NakamaError::IllegalStateForResume
        );
    }

    let now = Clock::get()?.unix_timestamp;
    let paused_at = ctx.accounts.paused_satellite.paused_at;
    require!(now >= paused_at, NakamaError::ClockBackwards);

    let pause_duration = now
        .checked_sub(paused_at)
        .ok_or(NakamaError::MathOverflow)?;

    let new_stream_start;
    let sub_pubkey;
    {
        let sub = &mut ctx.accounts.subscription;
        sub.stream_start = sub
            .stream_start
            .checked_add(pause_duration)
            .ok_or(NakamaError::MathOverflow)?;
        sub.state = SubscriptionState::Active;
        new_stream_start = sub.stream_start;
        sub_pubkey = sub.key();
    }

    emit!(SubscriptionResumed {
        subscription: sub_pubkey,
        resumed_at: now,
        pause_duration,
        new_stream_start,
    });

    // Anchor `close = merchant` runs post-handler — rent → merchant.
    Ok(())
}
