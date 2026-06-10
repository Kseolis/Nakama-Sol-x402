//! ADR-015 §F1 — `Option<Account<T>> + init` permissionless-pre-init grief
//! pattern (security-audit-patterns.md §P1). Expansion of the smoke regression
//! in `adr015_security_remediation.rs`.
//!
//! These tests pin the **order** of guards in `charge_handler`: the FSM state
//! guard (ADR-004 §2.h, `IllegalStateForCharge`) runs BEFORE the F1 satellite
//! defence. Therefore a `Some(grace_pda)` payload on a non-Active subscription
//! must bounce with `IllegalStateForCharge` — never `UnexpectedGraceSatellite`.
//!
//! Additional adversarial cases:
//! * **Wrong seed**: caller derives the grace PDA from a different seed
//!   constant — Anchor `ConstraintSeeds` (2006) before the handler body.
//! * **Multi-tx grief**: a failed grief tx-1 must leave NO satellite behind so
//!   that the next legitimate exhausting tx-2 (with the proper `Some(grace)`
//!   payload) can allocate it cleanly.
//!
//! Black-box: tests reach into raw account data only for the `state` byte at
//! `STATE_OFFSET` and grace satellite presence checks via `svm.get_account()`.
//! Discriminators/seed prefixes are pinned in `common::`.

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_anchor_err, assert_nakama_err, NakamaError},
    fund_actors, grace_pda, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
    STATE_OFFSET,
};
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

/// Helper: spin up a healthy Active subscription with `periods_to_prefund`
/// periods of escrow. Returns (plan_pk, sub_pk, vault_pk, grace_pk).
fn setup_active_sub(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    price: u64,
    period: i64,
    periods: u8,
) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            price,
            period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (grace_pk, _) = grace_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0);
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            periods,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    (plan_pk, sub_pk, vault_pk, grace_pk)
}

/// ADR-015 §F1 — post-cancel subscriptions are unreachable to `charge`
/// even before the F1 fix can fire because the vault TokenAccount is
/// closed inside cancel_handler (ADR-013 §Q6). Charge's Anchor account
/// validation hits `AccountNotInitialized` (3012) on the vault.
///
/// This pins the security property "no satellite grief possible on
/// Cancelled" through a different mechanism than IllegalStateForCharge:
/// the vault precondition fails first. The attacker cannot reach the
/// F1 check on a Cancelled subscription, by design.
#[test]
fn charge_with_grace_on_cancelled_unreachable_due_to_closed_vault() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        setup_active_sub(&mut env, &actors, plan_price, plan_period, 2);

    // Drive subscription into Cancelled.
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    )
    .expect("cancel into Cancelled tombstone");

    // Confirm tombstone state byte (Cancelled = variant index 4 per
    // src/state.rs SubscriptionState ordering — Active=0, Paused=1,
    // GracePeriod=2, Exhausted=3, Cancelled=4).
    let pre = env.svm.get_account(&sub_pk).expect("alive tombstone");
    assert_eq!(pre.data[STATE_OFFSET], 4, "subscription is Cancelled");

    // Attacker tries to grief the closed-out subscription with Some(grace).
    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(grace_pk),
        )],
        &[&attacker],
    );

    // Anchor 3012 fires on the closed vault before the handler body. The
    // grace-satellite attack surface is closed by this even-outer guard.
    assert_anchor_err(r, anchor_codes::ACCOUNT_NOT_INITIALIZED);

    // No satellite created — Anchor rolled back init on validation error.
    assert!(
        env.svm
            .get_account(&grace_pk)
            .map(|a| a.owner == Pubkey::default())
            .unwrap_or(true),
        "grace satellite must not exist post-rejection of Cancelled-state charge"
    );
}

/// Same order-of-guards property for `Paused`. Drive sub into Paused (ADR-006)
/// then attacker passes Some(grace_pda). Expected: IllegalStateForCharge first.
#[test]
fn charge_with_grace_on_paused_fails_with_state_guard() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        setup_active_sub(&mut env, &actors, plan_price, plan_period, 2);

    // Merchant pauses subscription (ADR-006). Pause sets state=Paused (=1).
    clock::set_clock(&mut env.svm, T0 + plan_period / 4);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let pre = env.svm.get_account(&sub_pk).expect("alive paused");
    assert_eq!(pre.data[STATE_OFFSET], 1, "subscription is Paused");

    // Attacker tries to plant grace satellite during Paused.
    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(grace_pk),
        )],
        &[&attacker],
    );

    assert_nakama_err::<()>(r, NakamaError::IllegalStateForCharge);
    assert!(
        env.svm
            .get_account(&grace_pk)
            .map(|a| a.owner == Pubkey::default())
            .unwrap_or(true),
        "grace satellite must not exist post-rejection of Paused-state charge"
    );
}

/// ADR-015 §F1 forward-compat. Caller computes the grace PDA using a wrong
/// seed prefix (`b"wrong_grace"` instead of `b"grace"`). The mismatched PDA
/// can never satisfy the Anchor `seeds = [GRACE_SEED, subscription]` check —
/// Anchor raises `ConstraintSeeds` (2006) at account-validation phase BEFORE
/// the handler body runs. Pins that an attacker cannot grief by passing an
/// arbitrary pubkey that "looks like" a satellite address.
#[test]
fn charge_with_wrong_seed_grace_pda_fails_constraint_seeds() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, _real_grace) =
        setup_active_sub(&mut env, &actors, plan_price, plan_period, 2);

    // Compute a satellite PDA under a deliberately-wrong seed constant.
    let (wrong_grace_pk, _) =
        Pubkey::find_program_address(&[b"wrong_grace", sub_pk.as_ref()], &common::program_id());

    // Mid-stream — healthy charge window.
    clock::set_clock(&mut env.svm, T0 + plan_period);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(wrong_grace_pk),
        )],
        &[&attacker],
    );

    // Anchor raises `ConstraintSeeds` (2006) at account-validation BEFORE the
    // handler body, so the test sees the framework error code, not our
    // custom `UnexpectedGraceSatellite`. Both equally close the attack
    // surface — pinning the wire code so a future codegen change is
    // observable.
    assert_anchor_err(r, anchor_codes::CONSTRAINT_SEEDS);
}

/// ADR-015 §F1 multi-tx safety. Two sequential txs:
/// * **tx-1 (grief)**: attacker passes `Some(grace_pk)` on a healthy mid-stream
///   sub. Must fail `UnexpectedGraceSatellite`. Critically, Anchor must roll
///   back the `init` allocation — no satellite left for the legit caller.
/// * **tx-2 (legit exhaustion)**: subscriber then advances clock to exact
///   exhaustion and the legit keeper calls `charge` with `Some(grace_pk)`.
///   Must succeed and flip state to `GracePeriod`. Pins that tx-1's failed
///   init did not leave residue.
#[test]
fn failed_grief_tx_then_legit_exhausting_charge_succeeds() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        setup_active_sub(&mut env, &actors, plan_price, plan_period, 2);

    // tx-1 — mid-stream grief attempt.
    clock::set_clock(&mut env.svm, T0 + plan_period);
    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(grace_pk),
        )],
        &[&attacker],
    );
    assert_nakama_err::<()>(r, NakamaError::UnexpectedGraceSatellite);

    // Satellite must NOT exist (Anchor rolls back the failed init).
    let post_grief = env.svm.get_account(&grace_pk);
    assert!(
        post_grief
            .map(|a| a.owner == Pubkey::default())
            .unwrap_or(true),
        "grief tx-1 must leave no satellite residue (Anchor init rollback)"
    );

    // tx-2 — legit exhaustion at exact period boundary. With 2 periods
    // prefund, full exhaustion is at T0 + 2*period.
    clock::set_clock(&mut env.svm, T0 + 2 * plan_period);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
            &common::token_program_id(),
            Some(grace_pk),
        )],
        &[&keeper],
    )
    .expect("legit exhausting charge after failed grief tx must succeed");

    // Post-state: GracePeriod (= 2), satellite allocated.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 2,
        "state flipped to GracePeriod on legit exhausting charge"
    );
    let grace_acct = env
        .svm
        .get_account(&grace_pk)
        .expect("grace satellite allocated by legit tx-2");
    assert_eq!(
        grace_acct.owner,
        common::program_id(),
        "satellite owned by Nakama program post-init"
    );
}

/// Sanity: even if attacker plants `Some(arbitrary_pubkey)` (not seed-derived
/// at all — a random keypair address), Anchor `ConstraintSeeds` rejects it.
/// This is the inverse of the canonical attack (which uses the correct PDA);
/// included to pin that "plant any 32 bytes" is also closed.
#[test]
fn charge_with_random_pubkey_in_grace_slot_fails_constraint_seeds() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, _real_grace) =
        setup_active_sub(&mut env, &actors, plan_price, plan_period, 2);

    clock::set_clock(&mut env.svm, T0 + plan_period);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let random_pubkey = solana_keypair::Keypair::new().pubkey();
    // Random keypair → essentially-zero probability of matching the
    // [GRACE_SEED, sub] PDA. ConstraintSeeds (2006) catches it.

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(random_pubkey),
        )],
        &[&attacker],
    );

    // Suppress unused warning.
    let _ = AccountMeta::new(random_pubkey, false);

    assert_anchor_err(r, anchor_codes::CONSTRAINT_SEEDS);
}
