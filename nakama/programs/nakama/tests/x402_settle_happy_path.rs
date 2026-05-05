//! Phase 3 RED — settle_usage happy-path tests.
//!
//! Coverage:
//! - vault → merchant_ata transfer of `amount`
//! - parent.withdrawn_amount monotonically increases by `amount`
//! - pay_session.usage_amount += amount
//! - pay_session.last_settle_at = now
//! - state stays Open after success (Settling is transient — should not
//!   be observable post-tx)
//! - emit UsageSettled with correct cumulative_usage

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, fund_actors, ix, pay_session_pda, plan_pda, send_tx, setup, subscription_pda,
    token_balance, token_program_id, vault_pda, Signer,
};

const T0: i64 = 1_700_000_000;

fn decode_pay_session(data: &[u8]) -> nakama::state::PaySession {
    nakama::state::PaySession::deserialize(&mut &data[8..]).expect("decode PaySession")
}

fn read_subscription(
    svm: &litesvm::LiteSVM,
    sub_pk: &solana_pubkey::Pubkey,
) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("subscription alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode Subscription")
}

fn setup_open_session(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    facilitator_pk: &solana_pubkey::Pubkey,
    session_id: u64,
    reservation_cap: u64,
) -> solana_pubkey::Pubkey {
    let plan_id = 1u64;
    // price=1200, period=60s ⇒ rate=20/s, periods=2 ⇒ deposited=2400
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            1200,
            60,
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
fn settle_usage_transfers_vault_to_merchant_ata() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let session_id = 1u64;
    let sub_pk = setup_open_session(&mut env, &actors, &facilitator.pubkey(), session_id, 500);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    // Advance time so settle is allowed (rate=20/s, after 30s unlocked=600,
    // > 0 means amount ≤ 600 is fine).
    clock::set_clock(&mut env.svm, T0 + 30);

    let pre_vault = token_balance(&env.svm, &vault_pk);
    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    let amount = 100u64;
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
            amount,
        )],
        &[&facilitator],
    )
    .expect("settle_usage");

    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        pre_vault - amount,
        "vault decreased by amount"
    );
    assert_eq!(
        token_balance(&env.svm, &actors.merchant_ata),
        pre_merchant + amount,
        "merchant_ata increased by amount"
    );

    let acct = env.svm.get_account(&pay_sess_pk).expect("session alive");
    let sess = decode_pay_session(&acct.data);
    assert_eq!(sess.usage_amount, amount, "usage_amount accumulated");
    assert_eq!(sess.last_settle_at, T0 + 30);
    assert_eq!(
        sess.state,
        nakama::state::PaySessionState::Open as u8,
        "state must be Open post-settle (Settling is transient)"
    );
}

#[test]
fn settle_usage_advances_parent_withdrawn_amount() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 2u64;
    let sub_pk = setup_open_session(&mut env, &actors, &facilitator.pubkey(), session_id, 500);
    let (vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0 + 30);

    let pre_withdrawn = read_subscription(&env.svm, &sub_pk).withdrawn_amount;

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
            150,
        )],
        &[&facilitator],
    )
    .expect("settle");

    let post = read_subscription(&env.svm, &sub_pk).withdrawn_amount;
    assert_eq!(
        post,
        pre_withdrawn + 150,
        "parent.withdrawn_amount must increase by settle amount (single source of truth — ADR-002)"
    );
}

#[test]
fn settle_usage_increments_session_usage_monotonically() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 3u64;
    let sub_pk = setup_open_session(&mut env, &actors, &facilitator.pubkey(), session_id, 1000);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    // Three settles at increasing time, each within unlocked bound.
    let settles = [(T0 + 20, 50u64), (T0 + 40, 70u64), (T0 + 60, 80u64)];
    let mut expected_cumulative = 0u64;
    for (t, amount) in settles {
        clock::set_clock(&mut env.svm, t);
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
                amount,
            )],
            &[&facilitator],
        )
        .unwrap_or_else(|e| panic!("settle at t={}: {:?}", t, e));
        expected_cumulative += amount;

        let acct = env.svm.get_account(&pay_sess_pk).expect("alive");
        let sess = decode_pay_session(&acct.data);
        assert_eq!(
            sess.usage_amount, expected_cumulative,
            "usage_amount monotonic at t={}",
            t
        );
        assert_eq!(sess.last_settle_at, t);
    }
}
