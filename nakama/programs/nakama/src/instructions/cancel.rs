//! `cancel` instruction — ADR-002 §cancel + ADR-013 §"Cancel handler" (cycle-3 split).
//!
//! Subscriber-only (BLK-08). Settle merchant fairly, refund subscriber pro-rata,
//! close vault TokenAccount via explicit SPL CPI (BLK-15). **Subscription account
//! is preserved as a tombstone** — `state == Cancelled` is observable on-chain
//! until subscriber calls `cleanup` (ADR-013 §Decision). Rent reclaim is the
//! subscriber's choice of timing, never the merchant's (ADR-013 §Q1, §Q4).
//!
//! Post-split rationale (vs cycle-2 fused-cancel MVP):
//! - Multi-party UX (ADR-009): merchant may extend `cancel` signer policy;
//!   `cleanup` stays subscriber-only.
//! - Audit trail: `getProgramAccounts` filter on `state == 4` lists pending
//!   tombstones independently of event-log retention.
//! - x402 forward-compat: tombstone serves as parent-state sentinel for
//!   PaySession satellites (ADR-013 §"x402 forward-compat", R1).
//!
//! Hard guards:
//! - BLK-08 `subscriber: Signer<'info>` + `has_one = subscriber` constraint.
//! - BLK-06 `ClockBackwards` against `stream_start`.
//! - BLK-14 manual `CpiContext::new_with_signer` with explicit seed slice.
//! - BLK-15 `spl_token::close_account` CPI for vault (Anchor `close` doesn't
//!   handle TokenAccount close cleanly).

use anchor_lang::prelude::*;
use anchor_spl::token::{self, CloseAccount, Token, TokenAccount, Transfer};

use crate::constants::{GRACE_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{GracedSubscription, Subscription, SubscriptionCancelled, SubscriptionState};

/// Account validation per ADR-002 §cancel signer policy (BLK-08).
#[derive(Accounts)]
pub struct Cancel<'info> {
    /// Subscriber — must match `subscription.subscriber` (BLK-08 / has_one).
    /// Receives vault refund + closed-account rent.
    #[account(mut)]
    pub subscriber: Signer<'info>,

    /// Subscription PDA — **preserved as tombstone** post-cancel (ADR-013).
    /// `has_one = subscriber` enforces that the signer matches the snapshotted
    /// subscriber, so an arbitrary account cannot cancel another's subscription.
    /// No `close` attribute: account closure is the responsibility of
    /// `cleanup` (ADR-013 §Q4).
    #[account(
        mut,
        has_one = subscriber @ NakamaError::UnauthorizedCancel,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Per-subscription vault. Closed via explicit SPL CPI (BLK-15) after
    /// final settle + refund. authority = subscription PDA, signer-seeds
    /// derived from stored `vault_bump` (BLK-03). Boxed to keep the
    /// `Cancel::try_accounts` stack frame under the sBPF 4 KiB cap; ADR-007
    /// added an `Option<Account<GracedSubscription>>` slot which pushed the
    /// frame to ~4224 B (anchor-lang-1.0.1/src/accounts/boxed.rs).
    #[account(
        mut,
        seeds = [VAULT_SEED, subscription.key().as_ref()],
        bump = subscription.vault_bump,
        token::mint = subscription.token_mint,
        token::authority = subscription,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// Merchant settlement destination. Mint match enforced; address match
    /// against the snapshotted merchant_ata prevents redirection attacks.
    /// Boxed for the same stack-frame reason as `vault`.
    #[account(
        mut,
        address = subscription.merchant_ata,
        token::mint = subscription.token_mint,
    )]
    pub merchant_ata: Box<Account<'info, TokenAccount>>,

    /// Subscriber refund destination. Owner must match the signer.
    /// Boxed for the same stack-frame reason as `vault`.
    #[account(
        mut,
        token::mint = subscription.token_mint,
        token::authority = subscriber,
    )]
    pub subscriber_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,

    /// Optional `GracedSubscription` satellite — required iff
    /// `subscription.state == GracePeriod` (handler enforces with
    /// `MissingGraceSatellite`). When `Some`, Anchor `close = subscriber`
    /// returns rent to the subscriber as part of cancel cleanup; when `None`
    /// (cancel from `Active`), the close constraint is a no-op
    /// (anchor-lang-1.0.1/src/accounts/option.rs:72-77).
    /// ADR-007 §"cancel from GracePeriod" + §I-CANCEL-2.
    #[account(
        mut,
        close = subscriber,
        seeds = [GRACE_SEED, subscription.key().as_ref()],
        bump,
    )]
    pub graced_subscription: Option<Account<'info, GracedSubscription>>,
}

/// ADR-002 §cancel pseudocode (revised 2026-04-27) + ADR-007 §"cancel from
/// GracePeriod" GracePeriod arm.
pub fn cancel_handler(ctx: Context<Cancel>) -> Result<()> {
    // Step 1 — FSM guard. Cycle-3 reachable: `{Active, GracePeriod}` (ADR-007
    // §I-CANCEL-4). `Paused` arm reachable post-ADR-006; deferred. The
    // `matches!` shape lights up additional arms automatically when ADR-006
    // / ADR-007-Exhausted ship.
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
    let plan_pubkey = sub_view.plan;
    let current_state = sub_view.state;

    // ADR-007 §"cancel from GracePeriod" — `effective_now` clamps the unlocked
    // calculation when cancelling from Grace. From `Active` it is just `now`;
    // from `GracePeriod` it is `min(now, grace_until)` so a cancel after
    // passive grace expiry (state still GracePeriod, but `now > grace_until`)
    // settles fairly at the grace boundary instead of letting the merchant
    // claim time the subscriber never funded. ADR-007 §I-CANCEL-1.
    //
    // From GracePeriod the satellite MUST be present — both for reading
    // `grace_until` and for the Anchor `close = subscriber` constraint to
    // actually run on the satellite (rent → subscriber).
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
        // FSM guard above pinned the set; any other variant is unreachable in
        // this branch.
        _ => return err!(NakamaError::IllegalStateForCancel),
    };

    // Defense in depth: `effective_now >= stream_start` holds by construction
    // (Active branch reuses `now` which passed the clock guard; GracePeriod
    // branch clamps to `grace_until`, set at charge-tail when the stream was
    // already running, so `grace_until >= entered_grace_at >= stream_start`).
    // Re-check explicitly so the `as u64` cast cannot underflow on a future
    // refactor that violates the invariant.
    require!(effective_now >= stream_start, NakamaError::ClockBackwards);

    // Step 4–6 — pro-rata math against `effective_now`.
    // u128 intermediate to dodge overflow on long-running streams (ADR-002 §Negative).
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

    // Subscription PDA signer seeds (BLK-14: explicit slice of slices, manual signing).
    // The vault TokenAccount's authority is the Subscription PDA (set in subscribe via
    // `token::authority = subscription`). SPL Token requires Transfer.authority ==
    // source.owner and that authority to sign — so all vault-sourced CPIs must be
    // signed with the Subscription PDA seeds, not the vault seeds.
    // https://docs.rs/anchor-lang/1.0.1/anchor_lang/context/struct.CpiContext.html
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

    // Step 11 — set state to Cancelled. Post-split (ADR-013) this byte
    // **persists** on-chain as a tombstone observable by indexer / x402
    // satellites until the subscriber calls `cleanup`.
    {
        let sub_mut = &mut ctx.accounts.subscription;
        sub_mut.state = SubscriptionState::Cancelled;
        sub_mut.last_charge_at = now;
        // withdrawn_amount tracks cumulative settlement for off-chain debug;
        // safe-add — final_claimable is bounded by deposited.
        sub_mut.withdrawn_amount = sub_mut
            .withdrawn_amount
            .checked_add(final_claimable)
            .ok_or(NakamaError::MathOverflow)?;
    }

    // Step 10 — close vault TokenAccount with explicit CPI (BLK-15).
    // Lamports → subscriber. Authority is the Subscription PDA, signed via the
    // subscription PDA seeds (vault's authority IS subscription).
    //
    // Per anchor-spl 1.0.1 token::close_account / spl-token close_account:
    // https://docs.rs/anchor-spl/1.0.1/anchor_spl/token/fn.close_account.html
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
        final_settled: final_claimable,
        refunded: refund,
        timestamp: now,
    });

    // Subscription account intentionally NOT closed — tombstone preservation
    // per ADR-013 §"Cancel handler". Subscriber reclaims rent via `cleanup`.
    Ok(())
}
