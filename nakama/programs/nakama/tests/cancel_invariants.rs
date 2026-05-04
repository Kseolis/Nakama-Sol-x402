//! Error-path tests for `cancel`.
//!
//! Coverage:
//! - BLK-08 / ADR-002 §cancel signer policy — `UnauthorizedCancel` /
//!   has_one mismatch when a non-subscriber signs.
//! - BLK-06 / ADR-002 §cancel step 3 — `ClockBackwards` when
//!   `now < stream_start`.
//! - ADR-003 Q8 / BLK-10 — second cancel after fused-MVP cancel must fail
//!   (account already closed → AccountNotInitialized, not IllegalStateForCancel).

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_anchor_err, assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer,
};

fn create_plan_and_subscribe(env: &mut common::TestEnv, actors: &common::Actors) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
    let plan_id = 1u64;
    let price = 600u64;
    let period = 60i64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            price,
            period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);

    clock::set_clock(&mut env.svm, 1_700_000_000);
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            2,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    (plan_pk, sub_pk)
}

/// Source: ADR-002 §cancel signer policy, BLK-08 — only subscriber may cancel.
///
/// Attacker submits cancel with their own keypair as `subscriber` signer.
/// Anchor's `has_one = subscriber` plus PDA seed mismatch should reject;
/// either `ConstraintHasOne`, a seed mismatch, or `UnauthorizedCancel`
/// fires before any funds move.
#[test]
fn unauthorized_cancel_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");
    let attacker_ata =
        common::install_funded_ata(&mut env.svm, &attacker.pubkey(), &common::usdc_mint(), 0);

    // Pass `attacker` where `subscriber` is expected. Subscription account is
    // unchanged (PDA derived from real subscriber), so `has_one = subscriber`
    // should fire.
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::cancel_ix(
            &attacker.pubkey(), // wrong signer
            &sub_pk,
            &actors.merchant_ata,
            &attacker_ata,
        )],
        &[&attacker],
    );

    // AMBIG-02 (closed): tightened from assert_any_err in
    // chore/cleanup-cycle-1-debt. Cycle-1 confirmed the handler-side
    // `require!` (BLK-08) fires before Anchor's `has_one`, so the code is
    // always 6009 = NakamaError::UnauthorizedCancel.
    assert_nakama_err::<()>(result, NakamaError::UnauthorizedCancel);
}

/// Source: ADR-002 §cancel step 3, BLK-06 — `now < stream_start` rejected with
/// `ClockBackwards`.
#[test]
fn cancel_with_clock_before_stream_start_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    // stream_start was 1_700_000_000. Push clock back below it.
    clock::set_clock(&mut env.svm, 1_699_999_000);

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::ClockBackwards);
}

/// Source: ADR-003 Q8 / BLK-10 — second `cancel` on the same subscription must
/// fail because MVP fuses cleanup into cancel; the Subscription account no
/// longer exists. Expected: Anchor `AccountNotInitialized` (3012), NOT
/// `IllegalStateForCancel`.
#[test]
fn double_cancel_hits_account_not_initialized() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    // First cancel succeeds.
    clock::set_clock(&mut env.svm, 1_700_000_030);
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
    .expect("first cancel");

    // Second cancel: subscription is gone → AccountNotInitialized.
    // Expire blockhash so the second tx isn't deduped as AlreadyProcessed.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    );

    // AMBIG-01 (closed): tightened from assert_any_err in
    // chore/cleanup-cycle-1-debt. Cycle-1 confirmed Anchor 1.0.1 surfaces
    // exactly AccountNotInitialized (3012) on the closed `Account<T>`.
    // ADR-003 Q8 / BLK-10: post-MVP (split cancel/cleanup) this expectation
    // flips to NakamaError::IllegalStateForCancel — re-tighten then.
    assert_anchor_err(result, anchor_codes::ACCOUNT_NOT_INITIALIZED);
}
