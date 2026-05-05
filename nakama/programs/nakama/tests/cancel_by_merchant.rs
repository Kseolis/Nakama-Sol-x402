//! ADR-009 — polymorphic cancel: merchant-as-signer scenarios.
//!
//! Coverage matrix:
//! - Merchant cancels Active subscription mid-period → settle math identical
//!   to subscriber-cancel; vault-close rent → snapshotted subscriber (not
//!   merchant); event records `cancelled_by == merchant`.
//! - Merchant cancels from GracePeriod → effective_now clamps to grace_until;
//!   GracedSubscription closed (rent → subscriber per ADR-007 invariant);
//!   `had_graced_satellite == true` in event.
//! - Stranger signer rejected with `NoCancelAuthority` (ADR-009 §"Adversarial 1").
//! - Subscriber-slot swap rejected with `SubscriberAccountMismatch`
//!   (ADR-009 §"Rent-flow invariant").
//! - Sequential ix replay: subscriber-cancel followed by merchant-cancel hits
//!   `IllegalStateForCancel` on the second tx (ADR-009 §"Adversarial 4").

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, grace_pda, ix, plan_pda, send_tx, setup, subscription_pda, token_balance,
    vault_pda, Signer, STATE_OFFSET,
};

const GRACE_DURATION: i64 = 7 * 24 * 60 * 60;

/// Helper — bring an Actors set to a fully-Active subscription at clock T0.
fn create_active_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    plan_id: u64,
    price: u64,
    period: i64,
    periods: u8,
    t0: i64,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
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

    clock::set_clock(&mut env.svm, t0);
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

/// Decode the latest `SubscriptionCancelled` event from program logs. We don't
/// have a runtime IDL parser in tests, but Anchor emits events as base64
/// `event-discriminator || borsh-payload` lines. We grep for "Program data:"
/// in logs and decode the latest one.
///
/// Returns the parsed event payload (cancelled_by + flags) for assertions.
#[derive(AnchorDeserialize, Debug)]
#[allow(dead_code)] // unused fields kept for layout fidelity + Debug print on failure
struct CancelledEventPayload {
    pub subscription: solana_pubkey::Pubkey,
    pub subscriber: solana_pubkey::Pubkey,
    pub plan: solana_pubkey::Pubkey,
    pub merchant: solana_pubkey::Pubkey,
    pub cancelled_by: solana_pubkey::Pubkey,
    pub final_settled: u64,
    pub refunded: u64,
    pub had_graced_satellite: bool,
    pub timestamp: i64,
}

fn decode_cancelled_event(meta: &litesvm::types::TransactionMetadata) -> CancelledEventPayload {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let line = meta
        .logs
        .iter()
        .rev()
        .find(|l| l.starts_with("Program data: "))
        .expect("emit! produces 'Program data:' line");
    let b64 = line.trim_start_matches("Program data: ").trim();
    let bytes = STANDARD.decode(b64).expect("base64 decode");
    // First 8 bytes are the event discriminator; we trust source order
    // (only one event type from `cancel`).
    let payload = &bytes[8..];
    CancelledEventPayload::deserialize(&mut &payload[..]).expect("borsh decode CancelledEvent")
}

/// ADR-009 §"Decision" — merchant signs cancel mid-Active. Settle math runs
/// identical to subscriber-cancel; rent flow unchanged.
#[test]
fn merchant_cancels_active_settles_and_closes_vault() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let t0: i64 = 1_700_000_000;
    let (_plan_pk, sub_pk) = create_active_subscription(&mut env, &actors, 1, 600, 60, 2, t0);
    let (vault_pk, _) = vault_pda(&sub_pk);

    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    let pre_merchant_lamports = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("merchant alive")
        .lamports;
    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);
    assert_eq!(token_balance(&env.svm, &vault_pk), 1200);

    // Half a period later. Stream unlocked = 30s × 10/s = 300.
    clock::set_clock(&mut env.svm, t0 + 30);

    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_by_merchant(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.merchant],
    )
    .expect("merchant cancel from Active");

    // ── State byte flipped to Cancelled. ──
    let post_sub = env
        .svm
        .get_account(&sub_pk)
        .expect("ADR-013 tombstone alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 4,
        "state byte must be Cancelled (=4) post merchant-cancel"
    );

    // ── Settle math: merchant +300, subscriber +900 USDC. ──
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc;
    let subscriber_delta = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber_usdc;
    assert_eq!(merchant_delta, 300, "merchant must receive 30s × 10 = 300");
    assert_eq!(
        subscriber_delta, 900,
        "subscriber must refund 1200 - 300 = 900"
    );

    // ── Rent flow: vault rent → subscriber wallet, NOT merchant.
    //    ADR-009 §"Rent-flow invariant" — merchant signs but does not collect. ──
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    let post_merchant_lamports = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("merchant alive")
        .lamports;
    assert!(
        post_subscriber_lamports > pre_subscriber_lamports,
        "subscriber lamports must increase (vault close rent → subscriber)"
    );
    // Merchant pays tx fee, so net delta is negative or zero — never positive.
    assert!(
        post_merchant_lamports <= pre_merchant_lamports,
        "merchant lamports must NOT increase (cancel actor pays fees, no rent gain)"
    );

    // ── Vault closed (BLK-15). ──
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "vault closed via SPL close_account CPI"),
    }

    // ── Event payload: cancelled_by == merchant, had_graced_satellite == false. ──
    let payload = decode_cancelled_event(&result);
    assert_eq!(payload.cancelled_by, actors.merchant.pubkey());
    assert_eq!(payload.subscriber, actors.subscriber.pubkey());
    assert_eq!(payload.merchant, actors.merchant.pubkey());
    assert_eq!(payload.final_settled, 300);
    assert_eq!(payload.refunded, 900);
    assert!(!payload.had_graced_satellite);
}

/// ADR-009 §"Adversarial 1" — stranger signer (neither subscriber nor
/// merchant) rejected by polymorphic guard.
#[test]
fn stranger_signer_rejected_with_no_cancel_authority() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let t0: i64 = 1_700_000_000;
    let (_plan_pk, sub_pk) = create_active_subscription(&mut env, &actors, 1, 600, 60, 2, t0);

    let stranger = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&stranger.pubkey(), 5_000_000_000)
        .expect("airdrop stranger");

    clock::set_clock(&mut env.svm, t0 + 30);

    let result = send_tx(
        &mut env.svm,
        &stranger,
        &[ix::cancel_ix_with_signer(
            &stranger.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            None,
        )],
        &[&stranger],
    );

    assert_nakama_err::<()>(result, NakamaError::NoCancelAuthority);
}

/// ADR-009 §"Rent-flow invariant" — merchant tries to redirect vault-close
/// rent by passing their own pubkey in the `subscriber` slot. Address
/// constraint fires.
#[test]
fn merchant_cannot_redirect_rent_via_subscriber_swap() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let t0: i64 = 1_700_000_000;
    let (_plan_pk, sub_pk) = create_active_subscription(&mut env, &actors, 1, 600, 60, 2, t0);

    clock::set_clock(&mut env.svm, t0 + 30);

    // Merchant passes themselves as the subscriber slot — rent redirection
    // attempt. Anchor's `address = subscription.subscriber` constraint must
    // fire before any state mutation or CPI.
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_with_signer(
            &actors.merchant.pubkey(),
            &actors.merchant.pubkey(), // subscriber slot ≠ subscription.subscriber
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            None,
        )],
        &[&actors.merchant],
    );

    assert_nakama_err::<()>(result, NakamaError::SubscriberAccountMismatch);
}

/// ADR-009 §"Adversarial 4" — merchant+subscriber sequential cancel: second
/// fires `IllegalStateForCancel` against the alive Cancelled tombstone.
///
/// Note: cycle-3 [DRIFT-1] — the actual surface is Anchor 3012 because
/// `vault` is closed by the first cancel and Anchor's pre-handler validation
/// fires before the FSM guard. We assert any-error here rather than pin the
/// specific code, since the inherited drift is documented in
/// `cancel_invariants.rs::double_cancel_hits_account_not_initialized_due_to_closed_vault`.
#[test]
fn second_cancel_after_first_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let t0: i64 = 1_700_000_000;
    let (_plan_pk, sub_pk) = create_active_subscription(&mut env, &actors, 1, 600, 60, 2, t0);

    clock::set_clock(&mut env.svm, t0 + 30);

    // First cancel: subscriber actor.
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
    .expect("first cancel by subscriber");

    // Second cancel attempt: merchant actor against the tombstone.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_by_merchant(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.merchant],
    );

    // We accept either NakamaError::IllegalStateForCancel (if FSM guard fires
    // first) or Anchor 3012 / 3007 (if vault validation fires first per the
    // documented [DRIFT-1] pin). The test simply asserts the second cancel
    // does NOT succeed — the tombstone is non-cancellable regardless of actor.
    match result {
        Err(_) => {}
        Ok(_) => panic!("ADR-009 §Adversarial 4: second cancel must not succeed"),
    }
}

/// ADR-009 — merchant cancels from GracePeriod. Inherits ADR-007 effective_now
/// clamp; rent flow unchanged. Event records had_graced_satellite=true.
#[test]
fn merchant_cancels_from_grace_closes_satellite_and_records_event() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    // Plan: price=1200, period=60s, rate=20 µUSDC/s. Subscribe 1 period =
    // 1200 deposited; will exhaust within 60s.
    let plan_id = 1u64;
    let price = 1200u64;
    let period = 60i64;
    let t0: i64 = 1_700_000_000;
    let (plan_pk, sub_pk) =
        create_active_subscription(&mut env, &actors, plan_id, price, period, 1, t0);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = grace_pda(&sub_pk);

    // Drive to grace via charge at T0 + period: stream fully unlocked; charge
    // tail flips state to Grace + creates satellite.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    clock::set_clock(&mut env.svm, t0 + period);
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
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("charge tail into grace");

    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;

    // Cancel from inside grace window via merchant-signer.
    let cancel_at = t0 + period + 60; // well within GRACE_DURATION
    assert!(cancel_at < t0 + period + GRACE_DURATION);
    clock::set_clock(&mut env.svm, cancel_at);

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_with_signer(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            Some(graced_pk),
        )],
        &[&actors.merchant],
    )
    .expect("merchant cancel from Grace");

    // ── State byte flipped to Cancelled. ──
    let post_sub = env
        .svm
        .get_account(&sub_pk)
        .expect("ADR-013 tombstone alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 4,
        "Cancelled state byte after merchant-cancel from Grace"
    );

    // ── Satellite closed. ──
    assert!(
        env.svm.get_account(&graced_pk).is_none()
            || env
                .svm
                .get_account(&graced_pk)
                .map(|a| a.lamports == 0)
                .unwrap_or(true),
        "graced_subscription closed by Anchor close=subscriber"
    );

    // ── Subscriber received vault-close + satellite-close rent. ──
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    assert!(
        post_subscriber_lamports > pre_subscriber_lamports,
        "subscriber lamports must increase (vault + satellite rent → subscriber)"
    );

    // ── Event: cancelled_by=merchant, had_graced_satellite=true. ──
    let payload = decode_cancelled_event(&result);
    assert_eq!(payload.cancelled_by, actors.merchant.pubkey());
    assert!(
        payload.had_graced_satellite,
        "cancel from Grace must set had_graced_satellite=true"
    );
}
