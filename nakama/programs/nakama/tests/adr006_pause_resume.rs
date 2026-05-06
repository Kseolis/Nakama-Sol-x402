//! Phase 2 RED — pause + resume happy/invariant tests.
//!
//! Coverage:
//! - pause initializes PausedSubscription with paused_at = now,
//!   subscription back-ref, state → Paused
//! - resume reads paused_at, shifts stream_start += pause_duration,
//!   closes satellite (rent → merchant), state → Active
//! - pause from non-merchant → UnauthorizedPause
//! - pause from non-Active state → IllegalStateForPause
//! - resume from non-merchant → UnauthorizedResume
//! - resume from non-Paused → IllegalStateForResume
//! - Time-frozen invariant: charge during Paused fails IllegalStateForCharge
//! - Math continuity: unlocked(now) post-resume == unlocked(paused_at) pre-resume

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, paused_sub_pda, plan_pda, send_tx, setup, subscription_pda, token_program_id,
    vault_pda, Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;

fn read_subscription(
    svm: &litesvm::LiteSVM,
    sub_pk: &solana_pubkey::Pubkey,
) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

fn read_paused_satellite(
    svm: &litesvm::LiteSVM,
    pda: &solana_pubkey::Pubkey,
) -> nakama::state::PausedSubscription {
    let data = svm.get_account(pda).expect("alive").data;
    nakama::state::PausedSubscription::deserialize(&mut &data[8..]).expect("decode")
}

fn setup_active(env: &mut common::TestEnv, actors: &common::Actors) -> solana_pubkey::Pubkey {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            1200,
            120,
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

    sub_pk
}

#[test]
fn pause_initializes_satellite_and_flips_state() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);
    let (paused_pda, _) = paused_sub_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let sat = read_paused_satellite(&env.svm, &paused_pda);
    assert_eq!(sat.subscription, sub_pk, "back-ref");
    assert_eq!(sat.paused_at, T0 + 30, "paused_at = clock at pause");

    let sub = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(sub.data[STATE_OFFSET], 1, "state byte = Paused (=1)");
}

#[test]
fn resume_shifts_stream_start_and_closes_satellite() {
    // ADR-006 continuity proof: U_p = rate*(paused_at - stream_start)
    // After resume: stream_start' = stream_start + pause_duration
    //   ⇒ U(now) = rate*(now - stream_start') = U_p ✓
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);
    let (paused_pda, _) = paused_sub_pda(&sub_pk);

    let pre_stream_start = read_subscription(&env.svm, &sub_pk).stream_start;

    // Pause at T0+30
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let pre_merchant_lamports = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("alive")
        .lamports;

    // Resume at T0+200 (paused for 170s)
    clock::set_clock(&mut env.svm, T0 + 200);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::resume_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("resume");

    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        post.state,
        nakama::state::SubscriptionState::Active,
        "state → Active"
    );
    assert_eq!(
        post.stream_start,
        pre_stream_start + 170,
        "stream_start += pause_duration (170s)"
    );

    // Satellite closed
    let post_sat = env.svm.get_account(&paused_pda);
    assert!(
        post_sat.is_none() || post_sat.map(|a| a.lamports == 0).unwrap_or(true),
        "PausedSubscription closed"
    );

    // Rent → merchant (who paid at pause)
    let post_merchant_lamports = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("alive")
        .lamports;
    assert!(
        post_merchant_lamports > pre_merchant_lamports,
        "merchant rent reclaim on resume"
    );
}

#[test]
fn pause_from_non_merchant_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::pause_ix(&attacker.pubkey(), &sub_pk)],
        &[&attacker],
    );

    assert_nakama_err::<()>(result, NakamaError::UnauthorizedPause);
}

#[test]
fn re_pause_from_paused_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("first pause");

    // Try to pause again — handler should reject. Anchor `init` will
    // surface AccountAlreadyInUse; we accept either NakamaError or system 0.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    );

    // Anchor `init` on existing PDA returns System Program Custom(0)
    // BEFORE handler body runs — same as ADR-x402-001 duplicate-session
    // pattern. This is the documented behaviour.
    common::error::assert_system_account_already_in_use(result);
}

#[test]
fn resume_from_active_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);

    // Subscription is Active; PausedSubscription doesn't exist yet.
    // Anchor seeds constraint will fire AccountNotInitialized (3012)
    // before handler IllegalStateForResume guard runs.
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::resume_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    );

    common::error::assert_anchor_err(result, common::error::anchor_codes::ACCOUNT_NOT_INITIALIZED);
}

#[test]
fn resume_from_non_merchant_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);

    // Pause first.
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop");

    clock::set_clock(&mut env.svm, T0 + 100);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::resume_ix(&attacker.pubkey(), &sub_pk)],
        &[&attacker],
    );

    assert_nakama_err::<()>(result, NakamaError::UnauthorizedResume);
}

#[test]
fn charge_during_paused_blocked() {
    // ADR-006 §"Per-state semantics in Paused": keeper charge MUST fail
    // with IllegalStateForCharge (ADR-004 invariant: charge requires Active).
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    // Pause
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    // Keeper attempts charge while Paused.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");

    clock::set_clock(&mut env.svm, T0 + 100);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
            &token_program_id(),
            None,
        )],
        &[&keeper],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCharge);
}

#[test]
fn pause_resume_continuity_invariant() {
    // ADR-006 §6 continuity: post-resume merchant earnings should equal
    // what they would have earned with no pause at all.
    //
    // Setup: rate=10/s. Pause at T0+30 (unlocked=300). Resume at T0+200
    // (170s frozen). Charge at T0+250 (50s post-resume) — claimable
    // should be 50*10 = 500 (NOT 220*10 = 2200, because pause froze).
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active(&mut env, &actors);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    let pre_merchant_usdc = common::token_balance(&env.svm, &actors.merchant_ata);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    clock::set_clock(&mut env.svm, T0 + 200);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::resume_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("resume");

    // Charge at T0+250.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");

    clock::set_clock(&mut env.svm, T0 + 250);
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
            &token_program_id(),
            None,
        )],
        &[&keeper],
    )
    .expect("charge post-resume");

    // After resume, stream_start shifted by 170. At T0+250, effective
    // elapsed = 250 - (0 + 170) = 80. Unlocked = 80 * 10 = 800.
    // Merchant got 800 (no double-charge from frozen interval).
    let merchant_delta = common::token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc;
    assert_eq!(
        merchant_delta, 800,
        "merchant earned only effective time (80s × 10 rate), pause time NOT charged"
    );
}
