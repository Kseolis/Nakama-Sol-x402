//! Phase 3 RED — settle_usage adversarial / invariants.
//!
//! Coverage matrix (ADR-x402-001 §"settle_usage" + §Adversarial):
//! - amount == 0 → IllegalAmountForSettle
//! - parent.state != Active → ParentNotActive (boundary contract)
//! - signer != pay_session.facilitator → UnauthorizedFacilitator
//! - amount > parent_remaining (unlocked - withdrawn) → InsufficientUnlockedFunds
//! - reservation_cap exceeded → ReservationCapExceeded
//! - cross-session: facilitator-A signs on session-B → UnauthorizedFacilitator
//! - wrong merchant_ata → PaySessionMerchantAtaMismatch (or
//!   ConstraintAddress, accept either if Anchor declarative fires first)

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_program_id, vault_pda,
    Signer,
};

const T0: i64 = 1_700_000_000;

fn make_subscription_with_session(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    facilitator_pk: &solana_pubkey::Pubkey,
    session_id: u64,
    reservation_cap: u64,
) -> solana_pubkey::Pubkey {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            1200,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);

    clock::set_clock(&mut env.svm, T0);
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

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            facilitator_pk,
            reservation_cap,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    sub_pk
}

#[test]
fn settle_usage_fails_with_zero_amount() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 1u64;
    let sub_pk =
        make_subscription_with_session(&mut env, &actors, &facilitator.pubkey(), session_id, 500);
    let (vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0 + 30);

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            0,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalAmountForSettle);
}

#[test]
fn settle_usage_fails_when_signer_is_not_facilitator() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 1u64;
    let sub_pk =
        make_subscription_with_session(&mut env, &actors, &facilitator.pubkey(), session_id, 500);
    let (vault_pk, _) = vault_pda(&sub_pk);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    clock::set_clock(&mut env.svm, T0 + 30);

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::settle_usage_ix(
            &attacker.pubkey(), // wrong signer
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            50,
        )],
        &[&attacker],
    );

    assert_nakama_err::<()>(result, NakamaError::UnauthorizedFacilitator);
}

#[test]
fn settle_usage_fails_when_reservation_cap_exceeded() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 1u64;
    // cap=100; first settle 80 ok, second settle 30 would exceed (110 > 100)
    let sub_pk =
        make_subscription_with_session(&mut env, &actors, &facilitator.pubkey(), session_id, 100);
    let (vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0 + 30);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            80,
        )],
        &[&facilitator],
    )
    .expect("first settle 80 ok");

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            30,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::ReservationCapExceeded);
}

#[test]
fn settle_usage_fails_when_amount_exceeds_unlocked() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 1u64;
    // deposited = 2 × 1200 = 2400; use cap = 2000 (within escrow).
    let sub_pk =
        make_subscription_with_session(&mut env, &actors, &facilitator.pubkey(), session_id, 2_000);
    let (vault_pk, _) = vault_pda(&sub_pk);

    // After 5 seconds, unlocked = 5 * 20 = 100. Try settle 200.
    clock::set_clock(&mut env.svm, T0 + 5);

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            200,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::InsufficientUnlockedFunds);
}

#[test]
fn cross_session_facilitator_cannot_settle_other_session() {
    // ADR-x402-001 §Adversarial 9 — facilitator-A authorized for session-1,
    // attempts settle on session-2 (also subscriber's).
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator_a = solana_keypair::Keypair::new();
    let facilitator_b = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator_a.pubkey(), 5_000_000_000)
        .expect("airdrop a");
    env.svm
        .airdrop(&facilitator_b.pubkey(), 5_000_000_000)
        .expect("airdrop b");

    // session-1 → facilitator_a; session-2 → facilitator_b
    let session_a = 100u64;
    let session_b = 200u64;
    let sub_pk =
        make_subscription_with_session(&mut env, &actors, &facilitator_a.pubkey(), session_a, 500);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_b,
            &facilitator_b.pubkey(),
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("open session B");

    let (vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0 + 30);

    // facilitator_a tries to settle session_b — must fail.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator_a,
        &[ix::settle_usage_ix(
            &facilitator_a.pubkey(),
            &sub_pk,
            session_b, // session B
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            50,
        )],
        &[&facilitator_a],
    );

    assert_nakama_err::<()>(result, NakamaError::UnauthorizedFacilitator);
}
