//! Phase 2 RED — adversarial / invariant tests for ADR-x402-001 lifecycle ix.
//!
//! Coverage:
//! - parent.state == Active boundary contract on open_session
//! - has_one = subscriber on parent (UnauthorizedOpenSession)
//! - reservation_cap > remaining escrow (ReservationCapExceedsEscrow)
//! - duplicate session_id fails (Anchor system error 0 / AccountAlreadyInUse)
//! - close from non-subscriber (UnauthorizedClose)
//! - close from Cancelled parent succeeds (ADR-x402-001 R1 — close not state-guarded)

mod common;

use common::{
    clock,
    error::{assert_nakama_err, assert_system_account_already_in_use, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer,
};

const T0: i64 = 1_700_000_000;

fn setup_active_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    periods: u8,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
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

    (plan_pk, sub_pk)
}

#[test]
fn open_session_fails_when_subscriber_is_not_signer() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");

    let facilitator = solana_keypair::Keypair::new();

    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::open_session_ix(
            &attacker.pubkey(), // wrong signer
            &sub_pk,
            1,
            &facilitator.pubkey(),
            100,
        )],
        &[&attacker],
    );

    // Anchor `has_one = subscriber @ UnauthorizedOpenSession` fires.
    assert_nakama_err::<()>(result, NakamaError::UnauthorizedOpenSession);
}

#[test]
fn open_session_fails_with_reservation_cap_exceeding_escrow() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    // periods=1 → deposited = 600. Reservation cap > 600 must fail.
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 1);

    let facilitator = solana_keypair::Keypair::new();

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            1,
            &facilitator.pubkey(),
            10_000, // way > 600 deposited
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::ReservationCapExceedsEscrow);
}

#[test]
fn open_session_with_duplicate_session_id_fails() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    let facilitator = solana_keypair::Keypair::new();

    // First open succeeds.
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            5,
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    )
    .expect("first open");

    // Second open with same session_id fails — Anchor `init` sees PDA alive.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            5, // same id
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    );

    // System program AccountAlreadyInUse surfaces as Custom(0).
    assert_system_account_already_in_use(result);
}

#[test]
fn close_session_fails_when_signer_is_not_subscriber() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    let facilitator = solana_keypair::Keypair::new();
    let session_id = 11u64;

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    )
    .expect("open");

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::close_session_ix(
            &attacker.pubkey(), // wrong signer
            &sub_pk,
            session_id,
        )],
        &[&attacker],
    );

    assert_nakama_err::<()>(result, NakamaError::UnauthorizedClose);
}

#[test]
fn close_session_succeeds_when_parent_is_cancelled() {
    // ADR-x402-001 R1 closure: close_session has NO parent.state guard,
    // subscriber must be able to release rent even after Subscription
    // is in Cancelled tombstone (ADR-013).
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    let facilitator = solana_keypair::Keypair::new();
    let session_id = 77u64;

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    )
    .expect("open");

    // Cancel the parent. State byte → Cancelled (=4).
    clock::set_clock(&mut env.svm, T0 + 30);
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
    .expect("cancel parent");

    // Now close_session should still work — R1 invariant.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::close_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
        )],
        &[&actors.subscriber],
    )
    .expect("close_session against Cancelled parent (R1)");
}
