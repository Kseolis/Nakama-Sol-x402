//! Phase 3 RED — KRITISCH: composability between settle_usage and charge.
//!
//! Это критический инвариант ADR-x402-001 §"Composability with charge":
//! `charge` (subscription stream) и `settle_usage` (pay-per-call) **оба**
//! пишут в `parent.withdrawn_amount` через ту же streaming math (ADR-002
//! single source of truth). Не должно быть double-spend; не должно быть
//! ghosted accounting.
//!
//! Этот suite проверяет:
//! - settle → charge не double-spends (charge видит уже-settle'd amount)
//! - charge → settle не double-spends
//! - interleaved settle/charge sequence — ровный пример из ADR §405-413
//! - Σ(charge claimable) + Σ(settle amounts) == parent.withdrawn_amount
//!   ↔ invariant
//!
//! Если этот suite падает — мы имеем escrow-double-spend bug, который ломает
//! всю экономику протокола. Phase 3 не merge'ится без 100% green.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance,
    token_program_id, vault_pda, Signer,
};

const T0: i64 = 1_700_000_000;

fn read_subscription(
    svm: &litesvm::LiteSVM,
    sub_pk: &solana_pubkey::Pubkey,
) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode Subscription")
}

fn setup_active_with_session(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    facilitator_pk: &solana_pubkey::Pubkey,
    session_id: u64,
) -> solana_pubkey::Pubkey {
    // price=100, period=100s ⇒ rate=1/s, periods=2 ⇒ deposited=200
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            100,
            100,
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
            150,
        )],
        &[&actors.subscriber],
    )
    .expect("open session");

    sub_pk
}

#[test]
fn settle_then_charge_does_not_double_spend() {
    // ADR-x402-001 §405-413 walkthrough scenario:
    // t=0: subscribe(deposited=200, rate=1/s, period=100s)
    // t=10: settle_usage(amount=5)  → parent.withdrawn = 5
    // t=20: charge()                → unlock=20; claimable=20-5=15; parent.withdrawn = 20
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 42u64;
    let sub_pk = setup_active_with_session(&mut env, &actors, &facilitator.pubkey(), session_id);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    // t=10: settle 5
    clock::set_clock(&mut env.svm, T0 + 10);
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
            5,
        )],
        &[&facilitator],
    )
    .expect("settle 5 at t=10");

    let after_settle = read_subscription(&env.svm, &sub_pk);
    assert_eq!(after_settle.withdrawn_amount, 5, "withdrawn=5 after settle");

    // t=20: charge → claimable = unlock(20) - withdrawn(5) = 15
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    clock::set_clock(&mut env.svm, T0 + 20);
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
    .expect("charge at t=20");

    let after_charge = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        after_charge.withdrawn_amount, 20,
        "withdrawn=20 after charge (5 prior + 15 new)"
    );

    // Merchant got 5 (settle) + 15 (charge) = 20 total
    assert_eq!(
        token_balance(&env.svm, &actors.merchant_ata) - pre_merchant,
        20,
        "merchant received exactly unlocked-to-t=20 amount, no double-spend"
    );
}

#[test]
fn charge_then_settle_does_not_double_spend() {
    // Reverse order:
    // t=0: subscribe(...)
    // t=20: charge()   → unlock=20, withdrawn=20
    // t=20: settle(5)  → would need unlocked-withdrawn = 0; FAILS InsufficientUnlockedFunds
    // t=30: settle(5)  → unlock=30, remaining=10; ok, withdrawn=25
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 1u64;
    let sub_pk = setup_active_with_session(&mut env, &actors, &facilitator.pubkey(), session_id);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    // t=20: charge captures full 20
    clock::set_clock(&mut env.svm, T0 + 20);
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
    .expect("charge at t=20");

    assert_eq!(read_subscription(&env.svm, &sub_pk).withdrawn_amount, 20);

    // t=30: settle 5 — unlock=30, withdrawn=20, remaining=10 → ok
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
            5,
        )],
        &[&facilitator],
    )
    .expect("settle 5 at t=30");

    assert_eq!(
        read_subscription(&env.svm, &sub_pk).withdrawn_amount,
        25,
        "withdrawn=25 (charge 20 + settle 5), no double-spend"
    );
}

#[test]
fn interleaved_settle_charge_full_walkthrough() {
    // The ADR-x402-001 §405-413 timeline verbatim:
    // t=0:   subscribe(deposited=100, rate=1/s, period=100s)
    // (note: ADR uses 100 deposit but our setup gives 200 — test scaled)
    // t=0:   open_session(session_id=42, reservation_cap=...)
    // t=10:  settle_usage(amount=5)   → parent.withdrawn = 5,  session.usage = 5
    // t=20:  charge()                  → unlock=20, parent.withdrawn += 15 = 20
    // t=30:  settle_usage(amount=5)   → parent.withdrawn = 25, session.usage = 10
    // t=40:  close_session()           → session closed, parent.withdrawn = 25
    // t=50:  charge()                  → unlock=50, parent.withdrawn += 25 = 50
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 42u64;
    let sub_pk = setup_active_with_session(&mut env, &actors, &facilitator.pubkey(), session_id);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    // t=10: settle 5
    clock::set_clock(&mut env.svm, T0 + 10);
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
            5,
        )],
        &[&facilitator],
    )
    .expect("t=10 settle 5");
    assert_eq!(read_subscription(&env.svm, &sub_pk).withdrawn_amount, 5);

    // t=20: charge — claimable = 20 - 5 = 15
    clock::set_clock(&mut env.svm, T0 + 20);
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
    .expect("t=20 charge");
    assert_eq!(read_subscription(&env.svm, &sub_pk).withdrawn_amount, 20);

    // t=30: settle 5 — remaining = 30 - 20 = 10, fine
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
            5,
        )],
        &[&facilitator],
    )
    .expect("t=30 settle 5");
    assert_eq!(read_subscription(&env.svm, &sub_pk).withdrawn_amount, 25);

    // t=40: close_session — withdrawn unchanged
    clock::set_clock(&mut env.svm, T0 + 40);
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
    .expect("t=40 close_session");
    assert_eq!(read_subscription(&env.svm, &sub_pk).withdrawn_amount, 25);

    // t=50: charge — claimable = 50 - 25 = 25
    clock::set_clock(&mut env.svm, T0 + 50);
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
    .expect("t=50 charge");

    let final_sub = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        final_sub.withdrawn_amount, 50,
        "withdrawn=50 — full ADR §405-413 walkthrough validated"
    );

    // Merchant balance delta = 50 (5 settle + 15 charge + 5 settle + 25 charge)
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    assert_eq!(
        merchant_delta, 50,
        "merchant net = 50, equals parent.withdrawn_amount (single source invariant)"
    );
}
