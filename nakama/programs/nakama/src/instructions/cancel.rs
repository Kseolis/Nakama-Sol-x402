//! `cancel` instruction — ADR-002 §cancel + ADR-013 §"Cancel handler" (cycle-3
//! split) + ADR-009 §"Decision" (polymorphic signer extension).
//!
//! Settle merchant fairly, refund subscriber pro-rata, close vault TokenAccount
//! via explicit SPL CPI (BLK-15). **Subscription account is preserved as a
//! tombstone** — `state == Cancelled` is observable on-chain until subscriber
//! calls `cleanup` (ADR-013 §Decision). Rent reclaim is the subscriber's
//! choice of timing, never the merchant's (ADR-013 §Q1, §Q4).
//!
//! ADR-009 widens the signer policy from `has_one = subscriber` to a
//! polymorphic guard: signer ∈ {subscription.subscriber, subscription.merchant}.
//! Settle math, CPI transfers, vault close, and Subscription tombstone state
//! are inherited unchanged from ADR-013. Rent flow is unchanged: vault rent
//! always returns to `subscription.subscriber` (passed as a separate
//! UncheckedAccount validated by handler), regardless of who signs.
//!
//! Hard guards:
//! - ADR-009 polymorphic-signer guard: explicit handler require! against
//!   `subscription.subscriber` and `subscription.merchant` snapshots.
//! - BLK-06 `ClockBackwards` against `stream_start`.
//! - BLK-14 manual `CpiContext::new_with_signer` with explicit seed slice.
//! - BLK-15 `spl_token::close_account` CPI for vault.
//! - ADR-009 `SubscriberAccountMismatch`: explicit `address = subscription.subscriber`
//!   constraint pins rent recipient to the snapshotted subscriber.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, CloseAccount, Token, TokenAccount, Transfer};

use crate::constants::{GRACE_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{GracedSubscription, Subscription, SubscriptionCancelled, SubscriptionState};

/// Account validation per ADR-013 §"Cancel handler" Accounts struct + ADR-009
/// §"Constraint shape" polymorphic signer split.
///
/// Wire order (canonical IDL): signer first, then `subscription` BEFORE
/// `subscriber` so the address constraint `subscriber.address =
/// subscription.subscriber` resolves against an already-loaded account
/// (Anchor evaluates fields in declaration order; back-references work,
/// forward-references surface as `AccountOwnedByWrongProgram` 3007).
///
///   signer, subscription, subscriber, vault, merchant_ata, subscriber_ata,
///   token_program, graced_subscription (Option, trailing).
#[derive(Accounts)]
pub struct Cancel<'info> {
    /// Polymorphic cancel actor. Validated by handler against
    /// `subscription.subscriber` OR `subscription.merchant`. ADR-009.
    pub signer: Signer<'info>,

    /// Subscription PDA — **preserved as tombstone** post-cancel (ADR-013).
    /// Seed-derived from `(subscriber, plan)` snapshots; signer-policy is
    /// enforced in handler (ADR-009 polymorphic guard) rather than declarative
    /// `has_one = subscriber`, which is incompatible with merchant-cancel.
    /// Declared before `subscriber` so the latter's `address` constraint can
    /// reference `subscription.subscriber`.
    #[account(
        mut,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Snapshotted subscriber wallet — rent recipient for vault close and
    /// (if Grace) `graced_subscription` close. Address-pinned to
    /// `subscription.subscriber` so a merchant-signer flow cannot redirect
    /// rent. ADR-009 §"Rent-flow invariant".
    /// CHECK: address-constraint enforces equality to the snapshotted pubkey.
    #[account(
        mut,
        address = subscription.subscriber @ NakamaError::SubscriberAccountMismatch,
    )]
    pub subscriber: UncheckedAccount<'info>,

    /// Per-subscription vault. Closed via explicit SPL CPI (BLK-15) after
    /// final settle + refund. Boxed to keep `Cancel::try_accounts` stack
    /// frame under the sBPF 4 KiB cap (ADR-007 added the `Option<Account<
    /// GracedSubscription>>` slot which pushed the frame to ~4224 B).
    #[account(
        mut,
        seeds = [VAULT_SEED, subscription.key().as_ref()],
        bump = subscription.vault_bump,
        token::mint = subscription.token_mint,
        token::authority = subscription,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// Merchant settlement destination. Address pinned to the snapshot
    /// (defense-in-depth against redirection). Boxed for stack-frame parity.
    #[account(
        mut,
        address = subscription.merchant_ata,
        token::mint = subscription.token_mint,
    )]
    pub merchant_ata: Box<Account<'info, TokenAccount>>,

    /// Subscriber refund destination. Authority pinned to the snapshotted
    /// subscriber (NOT the signer) — merchant-cancel still refunds the
    /// subscriber's ATA, never the merchant's. Boxed for stack-frame parity.
    #[account(
        mut,
        token::mint = subscription.token_mint,
        token::authority = subscription.subscriber,
    )]
    pub subscriber_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,

    /// Optional `GracedSubscription` satellite — required iff
    /// `subscription.state == GracePeriod` (handler enforces with
    /// `MissingGraceSatellite`). When `Some`, Anchor `close = subscriber`
    /// returns rent to the snapshotted subscriber (NOT the cancel actor).
    /// ADR-007 §"cancel from GracePeriod" + §I-CANCEL-2 + ADR-009 rent-flow.
    #[account(
        mut,
        close = subscriber,
        seeds = [GRACE_SEED, subscription.key().as_ref()],
        bump,
    )]
    pub graced_subscription: Option<Account<'info, GracedSubscription>>,
}

/// ADR-002 §cancel pseudocode (revised 2026-04-27) + ADR-007 §"cancel from
/// GracePeriod" + ADR-009 §"Decision" polymorphic signer guard.
pub fn cancel_handler(ctx: Context<Cancel>) -> Result<()> {
    // Step 0 — ADR-009 polymorphic signer guard. Single source of truth: the
    // snapshotted `subscriber` and `merchant` pubkeys on Subscription. Plan
    // account is intentionally NOT loaded — `Subscription.merchant` was
    // snapshotted at subscribe (ADR-001 §Consequences) precisely to avoid this
    // dependency in cancel/charge handlers.
    {
        let sub = &ctx.accounts.subscription;
        let signer_key = ctx.accounts.signer.key();
        require!(
            signer_key == sub.subscriber || signer_key == sub.merchant,
            NakamaError::NoCancelAuthority
        );
    }

    // Step 1 — FSM guard. Cycle-3 reachable: `{Active, GracePeriod}` (ADR-007
    // §I-CANCEL-4). `Paused` arm reachable post-ADR-006; deferred.
    {
        let sub = &ctx.accounts.subscription;
        require!(
            matches!(
                sub.state,
                SubscriptionState::Active | SubscriptionState::GracePeriod
            ),
            NakamaError::IllegalStateForCancel
        );
    }

    // Step 2/3 — clock guard (BLK-06).
    let now = Clock::get()?.unix_timestamp;
    let sub_view = &ctx.accounts.subscription;
    require!(now >= sub_view.stream_start, NakamaError::ClockBackwards);

    // Snapshot the math inputs before borrowing the account mutably.
    let stream_start = sub_view.stream_start;
    let deposited_amount = sub_view.deposited_amount;
    let withdrawn_amount = sub_view.withdrawn_amount;
    let rate_per_second = sub_view.rate_per_second;
    let subscription_bump = sub_view.bump;
    let subscription_pubkey = sub_view.key();
    let subscriber_pubkey = sub_view.subscriber;
    let merchant_pubkey = sub_view.merchant;
    let plan_pubkey = sub_view.plan;
    let current_state = sub_view.state;
    let cancelled_by = ctx.accounts.signer.key();

    // ADR-007 §"cancel from GracePeriod" — `effective_now` clamps the unlocked
    // calculation when cancelling from Grace.
    let effective_now = match current_state {
        SubscriptionState::Active => now,
        SubscriptionState::GracePeriod => {
            let grace = ctx
                .accounts
                .graced_subscription
                .as_ref()
                .ok_or(NakamaError::MissingGraceSatellite)?;
            core::cmp::min(now, grace.grace_until)
        }
        _ => return err!(NakamaError::IllegalStateForCancel),
    };

    require!(effective_now >= stream_start, NakamaError::ClockBackwards);

    // Step 4–6 — pro-rata math against `effective_now`.
    let elapsed = (effective_now - stream_start) as u64;
    let unlocked_u128 = (elapsed as u128)
        .checked_mul(rate_per_second as u128)
        .ok_or(NakamaError::MathOverflow)?;
    let cap_u128 = deposited_amount as u128;
    let unlocked = u128::min(unlocked_u128, cap_u128) as u64;

    let final_claimable = unlocked
        .checked_sub(withdrawn_amount)
        .ok_or(NakamaError::MathOverflow)?;
    let refund = deposited_amount
        .checked_sub(unlocked)
        .ok_or(NakamaError::MathOverflow)?;

    // Subscription PDA signer seeds (BLK-14: explicit slice of slices).
    let sub_seeds: &[&[u8]] = &[
        SUB_SEED,
        subscriber_pubkey.as_ref(),
        plan_pubkey.as_ref(),
        &[subscription_bump],
    ];
    let sub_signer_seeds: &[&[&[u8]]] = &[sub_seeds];

    // Step 7 — settle merchant.
    if final_claimable > 0 {
        let cpi_accounts = Transfer {
            from: ctx.accounts.vault.to_account_info(),
            to: ctx.accounts.merchant_ata.to_account_info(),
            authority: ctx.accounts.subscription.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            cpi_accounts,
            sub_signer_seeds,
        );
        token::transfer(cpi_ctx, final_claimable)?;
    }

    // Step 8–9 — refund subscriber.
    if refund > 0 {
        let cpi_accounts = Transfer {
            from: ctx.accounts.vault.to_account_info(),
            to: ctx.accounts.subscriber_ata.to_account_info(),
            authority: ctx.accounts.subscription.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            cpi_accounts,
            sub_signer_seeds,
        );
        token::transfer(cpi_ctx, refund)?;
    }

    let had_graced_satellite = ctx.accounts.graced_subscription.is_some();

    // Step 11 — set state to Cancelled.
    {
        let sub_mut = &mut ctx.accounts.subscription;
        sub_mut.state = SubscriptionState::Cancelled;
        sub_mut.last_charge_at = now;
        sub_mut.withdrawn_amount = sub_mut
            .withdrawn_amount
            .checked_add(final_claimable)
            .ok_or(NakamaError::MathOverflow)?;
    }

    // Step 10 — close vault TokenAccount with explicit CPI (BLK-15).
    // Lamports → snapshotted subscriber (NOT cancel actor) — ADR-009 invariant.
    let close_accounts = CloseAccount {
        account: ctx.accounts.vault.to_account_info(),
        destination: ctx.accounts.subscriber.to_account_info(),
        authority: ctx.accounts.subscription.to_account_info(),
    };
    let close_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.key(),
        close_accounts,
        sub_signer_seeds,
    );
    token::close_account(close_ctx)?;

    emit!(SubscriptionCancelled {
        subscription: subscription_pubkey,
        subscriber: subscriber_pubkey,
        plan: plan_pubkey,
        merchant: merchant_pubkey,
        cancelled_by,
        final_settled: final_claimable,
        refunded: refund,
        had_graced_satellite,
        timestamp: now,
    });

    Ok(())
}
