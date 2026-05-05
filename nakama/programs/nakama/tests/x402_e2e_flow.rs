//! Phase 5 RED — end-to-end x402 flow + ADR §"Acceptance criteria"
//! checklist coverage in a single walkthrough.
//!
//! This is the Loom-pitch test: same invariant flow that the demo will
//! show on devnet. If this is green, the on-chain side of the pitch is
//! ready.
//!
//! Coverage:
//! - subscribe → open_session → settle ×3 → close_session
//! - parent.withdrawn_amount tracks all settles cumulatively
//! - merchant_ata receives Σ(settle amounts) exactly
//! - vault.balance = deposited - Σ(settle amounts)
//! - usage_amount on PaySession matches Σ(settles for that session)
//! - close returns rent to subscriber
//! - Subscription layout untouched throughout (forward-compat invariant
//!   — pinned by reading state byte at offset 192 + reserved zeroed)
//! - Multiple sessions per subscription work in parallel (Q1)

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, fund_actors, ix, pay_session_pda, plan_pda, send_tx, setup, subscription_pda,
    token_balance, token_program_id, vault_pda, Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const RESERVED_LEN: usize = 32;

fn read_subscription(
    svm: &litesvm::LiteSVM,
    sub_pk: &solana_pubkey::Pubkey,
) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

fn read_pay_session(
    svm: &litesvm::LiteSVM,
    pay_session_pk: &solana_pubkey::Pubkey,
) -> nakama::state::PaySession {
    let data = svm.get_account(pay_session_pk).expect("alive").data;
    nakama::state::PaySession::deserialize(&mut &data[8..]).expect("decode")
}

#[test]
fn e2e_x402_full_flow_acceptance() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    // Plan: price=1200, period=120s ⇒ rate=10/s. periods=2 ⇒ deposited=2400.
    let plan_id = 1u64;
    let price = 1200u64;
    let period = 120i64;
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
    let (vault_pk, _) = vault_pda(&sub_pk);

    // ── Step 1: subscribe ──
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

    let pre_subscription_data = env.svm.get_account(&sub_pk).expect("alive").data;
    let pre_state_byte = pre_subscription_data[STATE_OFFSET];
    let pre_reserved =
        pre_subscription_data[pre_subscription_data.len() - RESERVED_LEN..].to_vec();

    assert_eq!(pre_state_byte, 0, "pre-state Active");
    assert_eq!(pre_reserved, vec![0u8; RESERVED_LEN], "reserved zeroed");
    assert_eq!(token_balance(&env.svm, &vault_pk), 2400);

    // ── Step 2: open_session with cap=300 ──
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let session_id = 0xabcd_1234u64;
    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            300,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    let s_post_open = read_pay_session(&env.svm, &pay_sess_pk);
    assert_eq!(s_post_open.subscription, sub_pk);
    assert_eq!(s_post_open.merchant, actors.merchant.pubkey());
    assert_eq!(s_post_open.facilitator, facilitator.pubkey());
    assert_eq!(s_post_open.reservation_cap, 300);
    assert_eq!(s_post_open.usage_amount, 0);

    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);

    // ── Step 3: settle three times — t=20 (50), t=40 (100), t=60 (150) ──
    let settles = [(T0 + 20, 50u64), (T0 + 40, 100u64), (T0 + 60, 150u64)];
    let mut total_settled = 0u64;
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
        total_settled += amount;

        let s = read_pay_session(&env.svm, &pay_sess_pk);
        assert_eq!(s.usage_amount, total_settled);
        assert_eq!(
            s.last_settle_at, t,
            "last_settle_at should equal current clock"
        );
        assert_eq!(
            s.state,
            nakama::state::PaySessionState::Open as u8,
            "state must be Open post-settle (Settling is transient)"
        );
    }

    // Σ(settle) = 50 + 100 + 150 = 300 — exactly hits reservation_cap.
    assert_eq!(total_settled, 300);

    // ── Step 4: invariant — parent.withdrawn_amount == Σ(settles) ──
    let sub_post_settles = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        sub_post_settles.withdrawn_amount, total_settled,
        "ADR-002 single source of truth: withdrawn_amount = Σ(settle) (no charges \
         in this flow ⇒ exact equality)"
    );

    // Merchant got exactly Σ(settle).
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc;
    assert_eq!(merchant_delta, total_settled);

    // Vault decreased by Σ(settle) — initial 2400 minus 300 settled = 2100.
    assert_eq!(token_balance(&env.svm, &vault_pk), 2400 - total_settled);

    // ── Step 5: layout invariant unchanged through 4 ix submissions ──
    let mid_subscription_data = env.svm.get_account(&sub_pk).expect("alive").data;
    let mid_state_byte = mid_subscription_data[STATE_OFFSET];
    let mid_reserved =
        mid_subscription_data[mid_subscription_data.len() - RESERVED_LEN..].to_vec();
    assert_eq!(mid_state_byte, 0, "state byte still Active");
    assert_eq!(mid_reserved, pre_reserved, "reserved still zeroed");
    assert_eq!(
        mid_subscription_data.len(),
        pre_subscription_data.len(),
        "Subscription account size unchanged (forward-compat invariant)"
    );

    // ── Step 6: close_session — rent → subscriber ──
    let pre_close_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("alive")
        .lamports;

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
    .expect("close_session");

    let post_close = env.svm.get_account(&pay_sess_pk);
    assert!(
        post_close.is_none() || post_close.map(|a| a.lamports == 0).unwrap_or(true),
        "PaySession PDA closed"
    );

    let post_close_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("alive")
        .lamports;
    assert!(
        post_close_subscriber_lamports > pre_close_subscriber_lamports,
        "subscriber lamports must increase (PaySession rent → subscriber)"
    );

    // ── Final invariants assertion ──
    let final_sub = read_subscription(&env.svm, &sub_pk);
    assert_eq!(final_sub.state, nakama::state::SubscriptionState::Active);
    assert_eq!(
        final_sub.withdrawn_amount, total_settled,
        "post-close, withdrawn unchanged"
    );
}

#[test]
fn e2e_two_concurrent_sessions_settle_independently() {
    // ADR-x402-001 Q1 — N concurrent sessions per Subscription.
    // Demo angle: Loom shows two browser tabs → two parallel x402 sessions.
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            1200,
            120,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);

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

    let facilitator_a = solana_keypair::Keypair::new();
    let facilitator_b = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator_a.pubkey(), 5_000_000_000)
        .expect("a");
    env.svm
        .airdrop(&facilitator_b.pubkey(), 5_000_000_000)
        .expect("b");

    let id_a = 1u64;
    let id_b = 2u64;

    for (id, fac) in [(id_a, &facilitator_a), (id_b, &facilitator_b)] {
        env.svm.expire_blockhash();
        send_tx(
            &mut env.svm,
            &actors.subscriber,
            &[ix::open_session_ix(
                &actors.subscriber.pubkey(),
                &sub_pk,
                id,
                &fac.pubkey(),
                500,
            )],
            &[&actors.subscriber],
        )
        .expect("open");
    }

    clock::set_clock(&mut env.svm, T0 + 30);

    // facilitator A settles 80 on session A.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &facilitator_a,
        &[ix::settle_usage_ix(
            &facilitator_a.pubkey(),
            &sub_pk,
            id_a,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            80,
        )],
        &[&facilitator_a],
    )
    .expect("settle A");

    // facilitator B settles 70 on session B.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &facilitator_b,
        &[ix::settle_usage_ix(
            &facilitator_b.pubkey(),
            &sub_pk,
            id_b,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            70,
        )],
        &[&facilitator_b],
    )
    .expect("settle B");

    let sub = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        sub.withdrawn_amount, 150,
        "parent.withdrawn = sum of all sessions' settles (80 + 70)"
    );

    // Per-session ledgers are independent.
    let (pay_a_pk, _) = pay_session_pda(&sub_pk, id_a);
    let (pay_b_pk, _) = pay_session_pda(&sub_pk, id_b);
    assert_eq!(read_pay_session(&env.svm, &pay_a_pk).usage_amount, 80);
    assert_eq!(read_pay_session(&env.svm, &pay_b_pk).usage_amount, 70);
}
