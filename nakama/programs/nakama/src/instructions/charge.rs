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
//! - `InvalidPeriod`             — `period <= 0` snapshot   (ADR-015 §F4).
//! - `UnexpectedGraceSatellite`  — pre-inited satellite on healthy charge (ADR-015 §F1).
//!
//! Errors raised by Anchor declarative constraints (ADR-004 §8 / §9):
//! - `AtaMismatch`               — wrong destination ATA (`address = ...`).
//! - `MintMismatch`              — destination mint mismatch (`constraint = ...`).
//! - generic `ConstraintTokenMint` / `ConstraintTokenOwner` for vault.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::constants::{GRACE_DURATION, GRACE_SEED, SUB_SEED, VAULT_SEED};
use crate::error::NakamaError;
use crate::state::{
    GraceEntered, GracedSubscription, Plan, Subscription, SubscriptionCharged, SubscriptionState,
};

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
    #[account(mut)]
    pub payer: Signer<'info>,

    /// ADR-007 §"charge handler tail" + §I-CHARGE-1 + ADR-015 §F1.
    ///
    /// # Off-chain caller contract (BLK-007-MAJ-1, revised post-ADR-015 §F1)
    ///
    /// **Account-meta slot is ALWAYS attached** (Anchor's positional account
    /// ordering requires the slot in every charge tx). What goes in the slot
    /// depends on whether the caller predicts this charge will exhaust the
    /// stream:
    ///
    /// 1. **Routine charge** (predicted post-CPI `withdrawn < deposited`):
    ///    place the program id (`crate::ID`) as a placeholder. Anchor 1.0.1
    ///    with the `allow-missing-optionals` cargo feature interprets this
    ///    as `Option::None`; the `init` constraint is skipped and the
    ///    handler's healthy-charge branch runs.
    ///
    /// 2. **Anticipated exhaustion** (predicted post-CPI
    ///    `withdrawn == deposited`): pre-derive the PDA
    ///    `[GRACE_SEED, subscription]` and attach it as `Some(pda)`. Anchor
    ///    fires `init` and the handler's grace-tail branch records the
    ///    Active → GracePeriod transition.
    ///
    /// Off-chain prediction is feasible because the unlock math is
    /// deterministic in `(now, stream_start, price, period, deposited,
    /// withdrawn)` — the caller computes `unlocked = (elapsed * price) /
    /// period` (clamped to `deposited`) for the slot it expects to land in,
    /// and attaches the real PDA iff that value reaches `deposited`.
    ///
    /// # F1 guard interaction (`UnexpectedGraceSatellite` = 6037)
    ///
    /// ADR-015 §F1 closes a permissionless pre-init grief vector: any signer
    /// can plant the satellite on a healthy subscription. The handler's
    /// `else` branch (non-exhausting charge) therefore raises
    /// `UnexpectedGraceSatellite` (see `error.rs`) when
    /// `graced_subscription.is_some()`. **Always sending the real PDA is
    /// NOT safe** — it bricks every non-exhausting charge with error 6037.
    /// Callers MUST use the predict-and-conditionally-attach protocol above.
    ///
    /// On the inverse mispredict — caller sent `None` but math exhausts —
    /// the handler raises `MissingGraceSatellite`; the keeper re-submits
    /// with the satellite attached. Mispredict cost is one wasted tx, not
    /// a permanent brick.
    ///
    /// The test helper `tests/common/ix.rs::charge_ix_full` mirrors this
    /// protocol: callers thread `Option<Pubkey>` end-to-end; the bytes-on-
    /// the-wire encoding (`program_id` placeholder vs real PDA) is the
    /// helper's responsibility.
    ///
    /// `Option<Account<T>>` + `init` — Anchor 1.0.1 codegen runs the `init`
    /// constraint ONLY when the caller passes a real account; on `None` (caller
    /// passes `program_id` placeholder, OR omits entirely with the
    /// `allow-missing-optionals` cargo feature) init is skipped (verified
    /// anchor-syn-1.0.1/src/codegen/accounts/constraints.rs:33-41 and
    /// anchor-lang-1.0.1/src/accounts/option.rs:20-55).
    ///
    /// Keeper protocol:
    /// - Routine charge (no exhaustion expected) → pass `program_id` placeholder
    ///   or omit. Handler does NOT enter the grace tail; if the post-CPI math
    ///   nonetheless exhausts the stream, handler errors with
    ///   `MissingGraceSatellite` and the keeper re-submits with the satellite.
    /// - Anticipated exhaustion → pre-derive PDA `[GRACE_SEED, subscription]`
    ///   and pass it. Init fires only on the charge that actually flips state
    ///   (subsequent charges from `GracePeriod` are blocked by the §2.h state
    ///   guard at the top of this handler — `IllegalStateForCharge`).
    ///
    /// `init` requires the account to NOT exist; second flip into Grace on the
    /// same Subscription is unreachable (a `GracePeriod`-state Subscription
    /// blocks `charge` entirely), so init is at-most-once per Subscription
    /// lifetime — exactly what ADR-007 §"Storage decision" requires.
    /// Rent payer = `payer` (the keeper); recoverable on `top_up` / `cancel`
    /// close (§"Authority decisions").
    #[account(
        init,
        payer = payer,
        space = 8 + GracedSubscription::INIT_SPACE,
        seeds = [GRACE_SEED, subscription.key().as_ref()],
        bump,
    )]
    pub graced_subscription: Option<Account<'info, GracedSubscription>>,

    /// Required by Anchor when any field has `init`. Anchor 1.0.1 raises a
    /// compile-time error if `system_program` is missing from a struct that
    /// inits a non-token account (anchor-syn parser/accounts/mod.rs:114-118).
    pub system_program: Program<'info, System>,
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
    // ADR-015 §F4 — unlock math no longer reads `rate_per_second` (truncated
    // by integer division at subscribe). Lazy precise math reads `price` and
    // `period` directly; one division at the end gives full precision.
    let price = sub_view.price;
    let period = sub_view.period;
    let subscription_bump = sub_view.bump;
    let subscription_pubkey = sub_view.key();
    let subscriber_pubkey = sub_view.subscriber;
    let plan_pubkey = sub_view.plan;

    // ADR-015 §F4 defence-in-depth — the snapshot is immutable post-subscribe
    // and subscribe enforces `Plan.period > 0`, so this should be unreachable.
    // The explicit guard keeps the division safe under hostile/corrupted state.
    require!(period > 0, NakamaError::InvalidPeriod);

    // §3 — pure streaming math (ADR-015 §F4 lazy precise division).
    //
    //     unlocked = min((elapsed * price) / period, deposited_amount)
    //
    // u128 intermediate avoids overflow on multi-year streams; one final
    // checked_div gives an exact result (no rate truncation; merchant
    // earns the precise per-second fraction). Replaces the previous
    // `rate_per_second * elapsed` form which under-paid the merchant by
    // up to `(price mod period) / period` base units/sec.
    //
    // Overflow window: `elapsed * price` overflows u128 only when
    // elapsed > u128::MAX / price ≈ 1.7e29 sec for price=u64::MAX —
    // unreachable. `checked_mul` is retained as defence-in-depth per
    // .claude/rules/onchain-conventions.md ("no wrapping_* for CU saving").
    let elapsed = (now - stream_start) as u64; // safe after §2.j
    let unlocked_u128 = (elapsed as u128)
        .checked_mul(price as u128)
        .ok_or(NakamaError::MathOverflow)?
        .checked_div(period as u128)
        .ok_or(NakamaError::MathOverflow)?;
    // After `min`, the value is ≤ `deposited_amount` ≤ u64::MAX — cast safe.
    let unlocked = u128::min(unlocked_u128, deposited_amount as u128) as u64;
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
    // ADR-015 §F4 — keeper hint `next_charge_at` is derived from the precise
    // math too. `covered_seconds = (withdrawn * period) / price` is the
    // inverse of the unlock formula above. `price > 0` and `period > 0`
    // (defensive snapshot guard at §3 + subscribe-time `ZeroPrice` /
    // `ZeroPeriod` enforcement). Division is safe.
    let covered_seconds_u128 = (sub_mut.withdrawn_amount as u128)
        .checked_mul(period as u128)
        .ok_or(NakamaError::MathOverflow)?
        .checked_div(price as u128)
        .ok_or(NakamaError::MathOverflow)?;
    let covered_seconds =
        i64::try_from(covered_seconds_u128).map_err(|_| NakamaError::MathOverflow)?;
    sub_mut.next_charge_at = stream_start
        .checked_add(covered_seconds)
        .ok_or(NakamaError::MathOverflow)?
        .checked_add(period)
        .ok_or(NakamaError::MathOverflow)?;

    // ADR-007 §"charge handler tail" + ADR-015 §F1 — auto-transition
    // Active → GracePeriod when the stream is fully consumed.
    // `Option<Account<GracedSubscription>>` with `init` constraint (see
    // Accounts struct above) makes the satellite present only when the caller
    // anticipates this branch; on `None` we raise `MissingGraceSatellite` so
    // the keeper re-submits with the pre-derived PDA. Init is at-most-once
    // per Subscription lifetime (subsequent charges from GracePeriod are
    // blocked by the §2.h state guard at the top of this handler).
    //
    // F1 defence: the `else` branch (healthy charge, no exhaustion) MUST
    // reject any caller-provided `graced_subscription`. Anchor codegen
    // (anchor-syn-1.0.2/src/codegen/accounts/constraints.rs:29-50) runs the
    // `init` constraint inside an `if let Some(...) = ident` wrapper — when
    // the caller plants a real PDA in the optional slot, init fires and
    // allocates the satellite. A permissionless attacker can exploit this on
    // a healthy subscription to pre-poison the satellite slot, bricking the
    // next honest exhausting charge with `AccountAlreadyInUse`. Rejecting at
    // handler-time keeps the attacker's ix from succeeding (so no satellite
    // is left behind for the next charge).
    if sub_mut.withdrawn_amount == sub_mut.deposited_amount {
        // i64 + 604_800 cannot overflow for any realistic clock value
        // (i64::MAX − GRACE_DURATION is in the year ~292 billion AD); checked
        // for purity per onchain-conventions.md mandate. ADR-007 §I-CHARGE-2.
        let grace_until = now
            .checked_add(GRACE_DURATION)
            .ok_or(NakamaError::MathOverflow)?;

        let graced = ctx
            .accounts
            .graced_subscription
            .as_mut()
            .ok_or(NakamaError::MissingGraceSatellite)?;
        graced.subscription = subscription_pubkey;
        graced.entered_grace_at = now;
        graced.grace_until = grace_until;

        sub_mut.state = SubscriptionState::GracePeriod;

        emit!(GraceEntered {
            subscription: subscription_pubkey,
            entered_grace_at: now,
            grace_until,
        });
    } else {
        // ADR-015 §F1 — healthy charge (post-CPI `withdrawn < deposited`).
        // Reject any caller-provided `GracedSubscription` to defeat the
        // permissionless-pre-init poison vector. The legitimate caller
        // protocol is "always attach" for `Option<Account<>>` with `init`
        // is satisfied because Anchor only fires the `init` body inside the
        // `Some` arm of the codegen wrapper (verified
        // anchor-syn-1.0.2/src/codegen/accounts/constraints.rs:29-50). So a
        // `Some` value here means the satellite was created in this tx —
        // which is wrong for a healthy charge. `program_id` placeholder
        // (Option::None) is the only valid shape for non-exhausting charges.
        require!(
            ctx.accounts.graced_subscription.is_none(),
            NakamaError::UnexpectedGraceSatellite
        );
    }

    emit!(SubscriptionCharged {
        subscription: subscription_pubkey,
        amount: claimable,
        withdrawn_total: sub_mut.withdrawn_amount,
        timestamp: now,
    });

    Ok(())
}
