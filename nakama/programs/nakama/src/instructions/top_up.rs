//! `top_up` instruction — ADR-007 §"top_up handler" + §"Per-state eligibility".
//!
//! Subscriber-signed CPI USDC transfer (`subscriber_ata → vault`). Allowed from
//! `{Active, Paused, GracePeriod}` (Paused reachable post-ADR-006). From
//! `GracePeriod` it ALSO transitions state back to `Active` and closes the
//! `GracedSubscription` satellite (rent → subscriber). From `Active`/`Paused`
//! the only side effect is `deposited_amount += amount`.
//!
//! Authority decisions (ADR-007 §"Authority decisions"):
//! - signer = subscriber, enforced via `has_one = subscriber` + `Signer<'info>`
//!   (defense in depth against third-party top-up griefing — Adversarial 1).
//! - GracedSubscription close beneficiary = subscriber (Anchor `close = subscriber`).
//!
//! CPI ordering (mirrors ADR-002 §subscribe prefund + ADR-004 §4): SPL
//! `token::transfer` runs BEFORE state mutation so a CPI failure leaves the
//! `deposited_amount` invariant intact. ADR-007 Adversarial 4.
//!
//! Errors (handler):
//! - `IllegalAmountForTopUp` — `amount == 0` (ADR-007 §I-TOPUP-2).
//! - `IllegalStateForTopUp`  — state ∉ {Active, Paused, GracePeriod} (ADR-007 §I-TOPUP-3).
//! - `MissingGraceSatellite` — state == GracePeriod but caller omitted the satellite (ADR-007 §"top_up handler").
//! - `MathOverflow`          — `deposited_amount + amount` overflowed u64 (ADR-007 §I-TOPUP-8).
//!
//! Errors (Anchor declarative):
//! - `ConstraintHasOne` (2001)   — signer != subscription.subscriber.
//! - `ConstraintSeeds`  (2006)   — wrong `GracedSubscription` PDA passed.
//! - `ConstraintTokenMint` / `ConstraintTokenOwner` — `subscriber_ata`/`vault` mismatch.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::constants::{GRACE_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{GraceRecovered, GracedSubscription, Subscription, SubscriptionState};

/// Account validation per ADR-007 §"top_up handler".
///
/// `graced_subscription` is `Option<...>` so the same instruction handles
/// `top_up` from `Active`/`Paused` (None expected) and from `GracePeriod`
/// (Some required). Anchor 1.0.1 codegen for `Option<Account<T>>` runs the
/// `close` constraint only when `Some` (verified
/// anchor-lang-1.0.1/src/accounts/option.rs:72-77 — `AccountsClose for
/// Option<T>` short-circuits to `Ok(())` when None).
///
/// Caller passes the satellite PDA when expecting it; otherwise passes
/// `program_id` as placeholder, OR omits the trailing account entirely if the
/// `allow-missing-optionals` cargo feature is enabled (it is — see Cargo.toml).
#[derive(Accounts)]
pub struct TopUp<'info> {
    /// Subscriber — must match `subscription.subscriber` via `has_one`.
    /// `mut` because Anchor `close = subscriber` deposits the satellite's
    /// reclaimed lamports here when state == GracePeriod.
    #[account(mut)]
    pub subscriber: Signer<'info>,

    /// Subscription PDA — `mut` because `deposited_amount` is incremented and,
    /// from `GracePeriod`, `state` flips to `Active`. `has_one = subscriber`
    /// enforces that the signer is the snapshotted subscriber (ADR-007 §I-TOPUP-1,
    /// §Adversarial 1: third-party top-up griefing). Anchor raises bare
    /// `ConstraintHasOne` (2001); kickoff §2.2 chose to reuse the built-in
    /// rather than introduce a separate `UnauthorizedTopUp` variant.
    #[account(
        mut,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
        has_one = subscriber,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Optional `GracedSubscription` satellite. Required iff
    /// `subscription.state == GracePeriod` (handler enforces with
    /// `MissingGraceSatellite`). On `Some`, `close = subscriber` returns rent
    /// to the subscriber; on `None`, the `close` constraint is a no-op
    /// (ADR-007 §"top_up handler").
    #[account(
        mut,
        close = subscriber,
        seeds = [GRACE_SEED, subscription.key().as_ref()],
        bump,
    )]
    pub graced_subscription: Option<Account<'info, GracedSubscription>>,

    /// Per-subscription vault — destination of the SPL transfer.
    /// `mut` for the CPI; `bump = subscription.vault_bump` per BLK-03.
    /// `token::mint` + `token::authority` constraints are defense in depth
    /// against vault swap (the seed/bump pair already binds it).
    #[account(
        mut,
        seeds = [VAULT_SEED, subscription.key().as_ref()],
        bump = subscription.vault_bump,
        token::mint = subscription.token_mint,
        token::authority = subscription,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// Subscriber's source ATA — owned by subscriber, matching mint enforced
    /// declaratively (ADR-002 BLK-09 pattern).
    #[account(
        mut,
        token::mint = subscription.token_mint,
        token::authority = subscriber,
    )]
    pub subscriber_ata: Account<'info, TokenAccount>,

    /// Classic SPL Token only (ADR-004 §6 — `Program<'info, Token>` rejects
    /// Token-2022 with `InvalidProgramId`).
    pub token_program: Program<'info, Token>,
}

/// ADR-007 §"top_up handler" pseudocode (CPI-then-mutate ordering).
pub fn top_up_handler(ctx: Context<TopUp>, amount: u64) -> Result<()> {
    // I-TOPUP-2 — non-zero amount guard. Reject before any state mutation
    // or CPI so the failure path is observable as a single ix-level error
    // (ADR-007 §Adversarial 2).
    require!(amount > 0, NakamaError::IllegalAmountForTopUp);

    // I-TOPUP-3 — FSM state guard. `Paused` is reachable post-ADR-006; the
    // exhaustive `matches!` lets that arm light up automatically once
    // ADR-006 lands without re-touching this handler.
    let current_state = ctx.accounts.subscription.state;
    require!(
        matches!(
            current_state,
            SubscriptionState::Active | SubscriptionState::Paused | SubscriptionState::GracePeriod
        ),
        NakamaError::IllegalStateForTopUp
    );

    // From GracePeriod the satellite MUST be passed — both for the recovery
    // event and for the Anchor `close` constraint to actually run.
    if matches!(current_state, SubscriptionState::GracePeriod) {
        require!(
            ctx.accounts.graced_subscription.is_some(),
            NakamaError::MissingGraceSatellite
        );
    }

    // CPI subscriber_ata → vault. Subscriber-signed (no PDA seeds), mirrors
    // the prefund CPI in ADR-002 §subscribe step 10.
    // https://docs.rs/anchor-lang/1.0.1/anchor_lang/context/struct.CpiContext.html#method.new
    let cpi_accounts = Transfer {
        from: ctx.accounts.subscriber_ata.to_account_info(),
        to: ctx.accounts.vault.to_account_info(),
        authority: ctx.accounts.subscriber.to_account_info(),
    };
    let cpi_ctx = CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts);
    token::transfer(cpi_ctx, amount)?;

    // Post-CPI state mutation. `checked_add` per onchain-conventions.md
    // mandate (no `wrapping_*` for CU saving — overflow-checks=true is the
    // safety net, this is the explicit error path). ADR-007 §I-TOPUP-8.
    let sub_mut = &mut ctx.accounts.subscription;
    sub_mut.deposited_amount = sub_mut
        .deposited_amount
        .checked_add(amount)
        .ok_or(NakamaError::MathOverflow)?;

    // Recovery branch: GracePeriod → Active. The Anchor `close = subscriber`
    // constraint on `graced_subscription` runs at `exit` time (post-handler)
    // for the `Some` case — no manual close CPI needed.
    if matches!(current_state, SubscriptionState::GracePeriod) {
        sub_mut.state = SubscriptionState::Active;
        emit!(GraceRecovered {
            subscription: sub_mut.key(),
            top_up_amount: amount,
            new_deposited: sub_mut.deposited_amount,
        });
    }

    Ok(())
}
