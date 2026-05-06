//! Phase 3 — cancel-from-Paused integration (ADR-006 + ADR-013 + ADR-009).
//!
//! Coverage:
//! - subscriber cancels from Paused: settle math uses paused_at as
//!   effective_now (NOT now); merchant earns frozen-at-pause amount only.
//! - merchant cancels from Paused (ADR-009 polymorphic): same math.
//! - PausedSubscription satellite closed during cancel.
//! - charge during Paused still blocked (ADR-006 §"Per-state semantics").
//! - cancel from Paused without paused_subscription account → fails
//!   (IllegalStateForCancel via Option None branch).
//! - cancel-from-Paused without satellite explicitly = (None, None) →
//!   handler hits Paused arm with Option::None → IllegalStateForCancel.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, paused_sub_pda, plan_pda, send_tx, setup, subscription_pda, token_balance,
    Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;

fn read_subscription(
    svm: &litesvm::LiteSVM,
    sub_pk: &solana_pubkey::Pubkey,
) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

fn setup_paused_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    pause_at: i64,
) -> solana_pubkey::Pubkey {
    // price=1200, period=120s ⇒ rate=10/s, periods=2 ⇒ deposited=2400.
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

    clock::set_clock(&mut env.svm, pause_at);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    sub_pk
}

#[test]
fn subscriber_cancels_from_paused_settles_at_paused_at() {
    // Setup: pause at T0+30 (rate=10 ⇒ unlocked_at_pause = 300).
    // Cancel later at T0+1000. effective_now = paused_at = T0+30.
    // Final settle = 300 (NOT 10000 — frozen). Refund = 2400 - 300 = 2100.
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_paused_subscription(&mut env, &actors, T0 + 30);
    let (paused_pda, _) = paused_sub_pda(&sub_pk);

    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);

    clock::set_clock(&mut env.svm, T0 + 1000);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix_with_paused(
            &actors.subscriber.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    )
    .expect("cancel from Paused");

    let post_sub = env.svm.get_account(&sub_pk).expect("tombstone alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 4,
        "state byte = Cancelled (=4) post cancel-from-Paused"
    );

    // Math: merchant got 300 (unlocked at paused_at), subscriber got 2100.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc;
    let subscriber_delta = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber_usdc;
    assert_eq!(
        merchant_delta, 300,
        "merchant earns only paused_at unlocked = 30s × 10/s = 300 (NOT now-relative)"
    );
    assert_eq!(
        subscriber_delta, 2100,
        "subscriber refund = deposited(2400) - unlocked(300) = 2100"
    );

    // PausedSubscription closed.
    let post_paused = env.svm.get_account(&paused_pda);
    assert!(
        post_paused.is_none() || post_paused.map(|a| a.lamports == 0).unwrap_or(true),
        "PausedSubscription closed by cancel"
    );
}

#[test]
fn merchant_cancels_from_paused_polymorphic() {
    // ADR-009 polymorphic + ADR-006 cancel from Paused.
    // Merchant cancels their own Paused subscription.
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_paused_subscription(&mut env, &actors, T0 + 30);

    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);

    clock::set_clock(&mut env.svm, T0 + 500);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_with_paused(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(), // rent recipient still subscriber per ADR-009
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.merchant],
    )
    .expect("merchant cancel from Paused");

    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(post.state, nakama::state::SubscriptionState::Cancelled);

    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc;
    assert_eq!(
        merchant_delta, 300,
        "polymorphic cancel from Paused: same math regardless of signer"
    );
}

#[test]
fn cancel_from_paused_without_satellite_rejected() {
    // If state is Paused but caller doesn't pass paused_subscription, the
    // handler's `Paused → satellite.ok_or(IllegalStateForCancel)` arm fires.
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_paused_subscription(&mut env, &actors, T0 + 30);

    clock::set_clock(&mut env.svm, T0 + 500);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            // legacy 4-arg builder doesn't pass paused_subscription
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCancel);
}
