//! `cleanup` instruction — ADR-013 §"Cleanup handler" (cycle-3 split).
//!
//! Terminal account-lifecycle action (NOT an FSM enum-variant target). After
//! `cleanup` the Subscription account does not exist; lamports → subscriber.
//!
//! Permissioned: subscriber-only signer. Forward-compat for ADR-009 — merchant
//! may `cancel` a subscription, but `cleanup` is always subscriber-owned
//! (rent reclaim is a subscriber asset; ADR-013 §Q1).
//!
//! Eligible source states: `Cancelled` (post-cancel tombstone) or `Exhausted`
//! (post-grace, ADR-007). The exhaustive `matches!` in the state guard is
//! deliberately written so that future variants from ADR-006/007 fall into
//! the implicit reject path until they are explicitly opted in (ADR-013 §"Per-state
//! cleanup eligibility" — Active/Paused/GracePeriod must `cancel` first).
//!
//! Hard guards:
//! - Subscription PDA seeds + stored bump (ADR-001 §Subscription account).
//! - `has_one = subscriber` declarative constraint, reinforced by an explicit
//!   custom error (`UnauthorizedCleanup`) for off-chain operator clarity.
//! - `state ∈ {Cancelled, Exhausted}` else `IllegalStateForCleanup`.
//! - Anchor `close = subscriber` constraint on Subscription handles tail-end
//!   account closure + lamport drain.

use anchor_lang::prelude::*;

use crate::constants::SUB_SEED;
use crate::error::NakamaError;
use crate::state::{Subscription, SubscriptionCleaned, SubscriptionState};

/// Account validation per ADR-013 §"Cleanup handler".
#[derive(Accounts)]
pub struct Cleanup<'info> {
    /// Subscription PDA — closed at end of handler, lamports → subscriber.
    /// `has_one = subscriber` enforces signer == snapshotted subscriber so
    /// an unrelated party cannot reclaim someone else's rent (ADR-013 §Q1).
    #[account(
        mut,
        close = subscriber,
        has_one = subscriber @ NakamaError::UnauthorizedCleanup,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Subscriber — explicit signer (in contrast to `charge` which is
    /// permissionless). Mutable because Anchor's `close` writes lamports
    /// back to this account.
    #[account(mut)]
    pub subscriber: Signer<'info>,
}

/// ADR-013 §"Cleanup handler" pseudocode.
///
/// Vault is already closed in `cancel` (ADR-013 §Q6 — vault balance is 0 at
/// `Cancelled` entry; SPL `close_account` CPI ran). Cleanup deals only with
/// the Subscription account itself.
pub fn cleanup_handler(ctx: Context<Cleanup>) -> Result<()> {
    let sub = &ctx.accounts.subscription;

    // State guard. `matches!` is exhaustive-by-design so future variants from
    // ADR-006 (Paused) / ADR-007 (GracePeriod) fall into the reject path
    // automatically — they must `cancel` first to enter Cancelled, or the
    // ADR-007 grace-expiry transition will land them in Exhausted.
    require!(
        matches!(
            sub.state,
            SubscriptionState::Cancelled | SubscriptionState::Exhausted
        ),
        NakamaError::IllegalStateForCleanup
    );

    let now = Clock::get()?.unix_timestamp;

    emit!(SubscriptionCleaned {
        subscription: sub.key(),
        rent_returned_to: ctx.accounts.subscriber.key(),
        timestamp: now,
    });

    // Account closure handled by Anchor `close = subscriber` constraint.
    Ok(())
}
