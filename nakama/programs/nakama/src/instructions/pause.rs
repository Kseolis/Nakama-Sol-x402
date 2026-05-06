//! `pause` instruction — ADR-006 §"Pause handler".
//!
//! Merchant-only. Initializes the `PausedSubscription` satellite at
//! `paused_at = now`, flips state Active → Paused. Streaming math freezes:
//! `unlocked(t) = unlocked(paused_at)` for all t ≥ paused_at while in
//! Paused (handler-side enforcement; charge handler refuses Paused via
//! its FSM guard).
//!
//! Hard guards:
//! - `subscription.state == Active` (FSM — refuses re-pause from Paused)
//! - `has_one = merchant @ UnauthorizedPause`
//! - Anchor `init` on satellite — duplicate seeds fail with System
//!   `AccountAlreadyInUse` before handler runs (defense-in-depth against
//!   re-pause race)

use anchor_lang::prelude::*;

use crate::constants::{PAUSED_SUB_SEED, SUB_SEED};
use crate::error::NakamaError;
use crate::state::{PausedSubscription, Subscription, SubscriptionPaused, SubscriptionState};

#[derive(Accounts)]
pub struct Pause<'info> {
    /// Subscription PDA — mut for state byte flip; `has_one = merchant`
    /// declaratively pins the signer policy (ADR-006 §1 authority model).
    #[account(
        mut,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
        has_one = merchant @ NakamaError::UnauthorizedPause,
    )]
    pub subscription: Account<'info, Subscription>,

    /// PausedSubscription satellite — initialized, payer = merchant.
    /// Anchor `init` on duplicate seeds returns AccountAlreadyInUse — the
    /// re-pause guard is enforced declaratively before handler body runs.
    #[account(
        init,
        payer = merchant,
        space = 8 + PausedSubscription::INIT_SPACE,
        seeds = [PAUSED_SUB_SEED, subscription.key().as_ref()],
        bump,
    )]
    pub paused_satellite: Account<'info, PausedSubscription>,

    #[account(mut)]
    pub merchant: Signer<'info>,

    pub system_program: Program<'info, System>,
}

pub fn pause_handler(ctx: Context<Pause>) -> Result<()> {
    // FSM guard — only Active is pausable.
    {
        let sub = &ctx.accounts.subscription;
        require!(
            sub.state == SubscriptionState::Active,
            NakamaError::IllegalStateForPause
        );
    }

    let now = Clock::get()?.unix_timestamp;
    let sub_view = &ctx.accounts.subscription;
    require!(now >= sub_view.stream_start, NakamaError::ClockBackwards);

    // Compute unlocked-at-pause for the analytics event (ADR-002 streaming
    // math, mirror of charge §3).
    let elapsed = (now - sub_view.stream_start) as u64;
    let unlocked_unbounded = (elapsed as u128)
        .checked_mul(sub_view.rate_per_second as u128)
        .ok_or(NakamaError::MathOverflow)?;
    let unlocked = u128::min(unlocked_unbounded, sub_view.deposited_amount as u128) as u64;

    let sub_pubkey = sub_view.key();

    // Init satellite snapshot.
    {
        let satellite = &mut ctx.accounts.paused_satellite;
        satellite.subscription = sub_pubkey;
        satellite.paused_at = now;
        satellite.bump = ctx.bumps.paused_satellite;
    }

    // Flip state byte. ADR-001 layout invariant: state at offset 192
    // unchanged.
    {
        let sub = &mut ctx.accounts.subscription;
        sub.state = SubscriptionState::Paused;
    }

    emit!(SubscriptionPaused {
        subscription: sub_pubkey,
        paused_at: now,
        unlocked_at_pause: unlocked,
    });

    Ok(())
}
