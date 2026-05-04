//! `charge` instruction — ADR-004 §1–§5 (permissionless streaming withdrawal).
//!
//! Permissionless trigger: any pubkey may sign as `payer`. Security comes from
//! the math (monotonic `withdrawn_amount`) plus the FSM state guard, not from
//! the signer's identity (ADR-004 §1). Keeper-bots can drive this without
//! custodying user keys.
//!
//! Ordering (ADR-004 §2): Anchor declarative constraints → handler state guard
//! → clock guard → pure math → CPI transfer (vault → merchant_ata) → state
//! update + event. CPI before state mutation so a CPI failure leaves the
//! monotonic invariant intact (ADR-004 §4).
//!
//! Hard guards (sign-off blockers):
//! - BLK-03 vault `bump = subscription.vault_bump` (no per-charge re-derive).
//! - BLK-04 `plan` account kept for `has_one = plan` validation.
//! - BLK-14 manual `CpiContext::new_with_signer` — Anchor 1.0.1 does NOT
//!   auto-sign PDAs.
//!
//! Errors raised by the handler body:
//! - `IllegalStateForCharge`     — `state != Active`        (ADR-004 §2.h).
//! - `ClockBackwards`            — `now < stream_start`     (ADR-004 §2.j).
//! - `MathOverflow`              — `checked_*` saturation   (ADR-004 §3).
//! - `InsufficientUnlockedFunds` — `claimable == 0`         (ADR-004 §3 / §7).
//!
//! Errors raised by Anchor declarative constraints (ADR-004 §8 / §9):
//! - `AtaMismatch`               — wrong destination ATA (`address = ...`).
//! - `MintMismatch`              — destination mint mismatch (`constraint = ...`).
//! - generic `ConstraintTokenMint` / `ConstraintTokenOwner` for vault.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::constants::{SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{Plan, Subscription, SubscriptionCharged, SubscriptionState};

/// Account validation per ADR-004 §9 (revised 2026-04-27 for BLK-03 / BLK-04).
///
/// Constraint evaluation order matters for adversarial test pinning — Anchor
/// runs declarative constraints top-to-bottom. The order below is what the
/// black-box test matrix asserts against:
/// * subscription seeds + has_one    → `ConstraintHasOne` (2001) on plan swap
/// * vault    seeds + bump           → `ConstraintSeeds`  (2006) on vault swap
/// * merchant_ata `address = …`      → `AtaMismatch`      (custom)
/// * `Program<'info, Token>`         → `InvalidProgramId` (3008) on Token-2022
#[derive(Accounts)]
pub struct Charge<'info> {
    /// Subscription PDA — `mut` because the post-CPI step writes the new
    /// `withdrawn_amount` / `last_charge_at` / `next_charge_at`. `has_one =
    /// plan` validates `subscription.plan == plan.key()` declaratively
    /// (BLK-04). Bump from stored field — never re-derive (BLK-03).
    #[account(
        mut,
        seeds = [SUB_SEED, subscription.subscriber.as_ref(), subscription.plan.as_ref()],
        bump = subscription.bump,
        has_one = plan,
    )]
    pub subscription: Account<'info, Subscription>,

    /// Plan PDA — read-only. Kept in struct for `has_one = plan` (BLK-04).
    /// `Account<'info, Plan>` already verifies `owner == program_id` and the
    /// 8-byte discriminator (Anchor built-in), closing BLK-21.
    pub plan: Account<'info, Plan>,

    /// Per-subscription vault. Source of CPI transfer; `authority = subscription`
    /// PDA so all vault-sourced transfers must be CPI-signed with the
    /// subscription seeds. Anchor 1.0.1 does NOT auto-sign PDAs (sign-off
    /// anchor handoff item 1) — see manual seed construction below.
    /// Bump from stored `vault_bump` (BLK-03 — saves ~20k CU vs `find_program_address`).
    #[account(
        mut,
        seeds = [VAULT_SEED, subscription.key().as_ref()],
        bump = subscription.vault_bump,
        token::mint = subscription.token_mint,
        token::authority = subscription,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// Settlement destination — pinned to the subscription's snapshot, never
    /// re-derived. Plan is immutable so the snapshot can never go stale, but
    /// we use a custom `AtaMismatch` error for operational debug-UX
    /// (ADR-004 §8 — generic `ConstraintAddress` is context-poor).
    #[account(
        mut,
        address = subscription.merchant_ata @ NakamaError::AtaMismatch,
        constraint = merchant_ata.mint == subscription.token_mint
            @ NakamaError::MintMismatch,
    )]
    pub merchant_ata: Account<'info, TokenAccount>,

    /// Classic SPL Token only (ADR-004 §6). `Program<'info, Token>` rejects
    /// the Token-2022 program id with `InvalidProgramId` (3008) at account
    /// validation time — explicit deny, not accidental allow.
    pub token_program: Program<'info, Token>,

    /// Permissionless trigger: any pubkey may sign as `payer` (ADR-004 §1).
    /// No identity check — security comes from the math (monotonic
    /// `withdrawn_amount`) plus the state guard, not the signature.
    pub payer: Signer<'info>,
}

/// ADR-004 §1 → §2 → §3 → §4 → §5 ordering.
pub fn charge_handler(ctx: Context<Charge>) -> Result<()> {
    // §2.h — FSM state guard. MUST come before any state-mutation and before
    // touching the clock. ADR-003 §FSM enforcement.
    {
        let sub = &ctx.accounts.subscription;
        require!(
            sub.state == SubscriptionState::Active,
            NakamaError::IllegalStateForCharge
        );
    }

    // §2.i / §2.j — clock guard. `now >= stream_start` makes the unsigned
    // cast on the `elapsed` line below safe; also defends against fork
    // replay / validator clock drift (ADR-004 §2 rationale).
    let now = Clock::get()?.unix_timestamp;
    let sub_view = &ctx.accounts.subscription;
    require!(now >= sub_view.stream_start, NakamaError::ClockBackwards);

    // Snapshot math inputs. We hold `subscription` immutably through the
    // pure-math + CPI block; mutation happens only in the dedicated post-CPI
    // section (§5) so a CPI failure leaves the monotonic invariant intact
    // (ADR-004 §4 rationale 1).
    let stream_start = sub_view.stream_start;
    let deposited_amount = sub_view.deposited_amount;
    let withdrawn_amount = sub_view.withdrawn_amount;
    let rate_per_second = sub_view.rate_per_second;
    let period = sub_view.period;
    let subscription_bump = sub_view.bump;
    let subscription_pubkey = sub_view.key();
    let subscriber_pubkey = sub_view.subscriber;
    let plan_pubkey = sub_view.plan;

    // §3 — pure streaming math. u128 intermediate so `elapsed * rate` doesn't
    // wrap on multi-year streams (ADR-004 §3 — `release-profile overflow-checks`
    // would panic, but ADR wants explicit `MathOverflow`).
    let elapsed = (now - stream_start) as u64; // safe after §2.j
    let unlocked_unbounded = (elapsed as u128)
        .checked_mul(rate_per_second as u128)
        .ok_or(NakamaError::MathOverflow)?;
    // After `min`, the value is ≤ `deposited_amount` ≤ u64::MAX — cast safe.
    let unlocked = u128::min(unlocked_unbounded, deposited_amount as u128) as u64;
    let claimable = unlocked
        .checked_sub(withdrawn_amount)
        .ok_or(NakamaError::MathOverflow)?;
    require!(claimable > 0, NakamaError::InsufficientUnlockedFunds);

    // §4 — CPI vault → merchant_ata.
    //
    // Anchor 1.0.1 does NOT auto-sign PDAs (sign-off anchor handoff item 1).
    // We build the signer-seeds slice explicitly. The vault's authority is
    // the Subscription PDA (set in `subscribe` via `token::authority =
    // subscription`), so all vault-sourced transfers must be signed with
    // the subscription seeds, not the vault seeds.
    // https://docs.rs/anchor-lang/1.0.1/anchor_lang/context/struct.CpiContext.html
    let sub_seeds: &[&[u8]] = &[
        SUB_SEED,
        subscriber_pubkey.as_ref(),
        plan_pubkey.as_ref(),
        &[subscription_bump],
    ];
    let sub_signer_seeds: &[&[&[u8]]] = &[sub_seeds];

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
    token::transfer(cpi_ctx, claimable)?;

    // §5 — state update after CPI Ok. Order: monotonic `withdrawn_amount`
    // first (so any subsequent panic still leaves invariant intact), then
    // advisory hint fields. `next_charge_at` is a keeper hint, not a
    // security invariant (ADR-004 §5 closing paragraph).
    let sub_mut = &mut ctx.accounts.subscription;
    sub_mut.withdrawn_amount = sub_mut
        .withdrawn_amount
        .checked_add(claimable)
        .ok_or(NakamaError::MathOverflow)?;
    sub_mut.last_charge_at = now;
    // `rate_per_second >= 1` is guaranteed by the BLK-02 `ZeroRatePerSecond`
    // guard in `subscribe` — division is safe.
    let covered_seconds = i64::try_from(sub_mut.withdrawn_amount / rate_per_second)
        .map_err(|_| NakamaError::MathOverflow)?;
    sub_mut.next_charge_at = stream_start
        .checked_add(covered_seconds)
        .ok_or(NakamaError::MathOverflow)?
        .checked_add(period)
        .ok_or(NakamaError::MathOverflow)?;

    // TODO(ADR-007 — Top-up & Grace integration): when `top_up` ships,
    // un-comment the GracePeriod tail-transition here. Reserved per ADR-004
    // §5 — MUST stay commented in MVP. security-auditor: please verify this
    // hook is NOT active in MVP code.
    //
    //   if sub_mut.withdrawn_amount == sub_mut.deposited_amount {
    //       sub_mut.state = SubscriptionState::GracePeriod;
    //       sub_mut.grace_until = now + GRACE_DURATION;
    //   }

    emit!(SubscriptionCharged {
        subscription: subscription_pubkey,
        amount: claimable,
        withdrawn_total: sub_mut.withdrawn_amount,
        timestamp: now,
    });

    Ok(())
}
