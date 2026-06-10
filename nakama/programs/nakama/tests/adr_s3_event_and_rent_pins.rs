//! Hardening cycle S3 — regression pins for the two S1 audit follow-ups on
//! cancel-from-Paused that had no explicit assertion in the 161-test baseline.
//!
//! - S1-ARC-2: `SubscriptionCancelled.had_paused_satellite` (added in S2). The
//!   baseline only exercised the `false` case (Active / Grace cancels in
//!   `cancel_by_merchant.rs`). This file pins the `true` case AND re-pins the
//!   `false` case for an Active cancel so both event-field values are proven.
//!   ADR-009 §"Telemetry: event log".
//! - S1-ARC-3: PausedSubscription satellite rent → **subscriber** (ratified
//!   2026-06-10; revises strict ADR-006 §"Symmetry"). The baseline closed the
//!   satellite but never asserted the rent destination by lamport delta with
//!   the merchant (cancel actor) excluded. Here a merchant-signed
//!   cancel-from-Paused proves the satellite rent lands on the subscriber, not
//!   the cancelling merchant. ADR-009 §"Rent-flow invariant" + Revision history.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, fund_actors, ix, paused_sub_pda, plan_pda, send_tx, setup, subscription_pda, Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

/// Mirror of `state::SubscriptionCancelled` (read black-box from `events.rs`
/// field order). Layout fidelity matters for the Borsh decode below.
#[derive(AnchorDeserialize, Debug)]
#[allow(dead_code)]
struct CancelledEventPayload {
    pub subscription: Pubkey,
    pub subscriber: Pubkey,
    pub plan: Pubkey,
    pub merchant: Pubkey,
    pub cancelled_by: Pubkey,
    pub final_settled: u64,
    pub refunded: u64,
    pub had_paused_satellite: bool,
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
    let payload = &bytes[8..];
    CancelledEventPayload::deserialize(&mut &payload[..]).expect("borsh decode CancelledEvent")
}

fn setup_paused_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    pause_at: i64,
) -> Pubkey {
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

fn setup_active_subscription(env: &mut common::TestEnv, actors: &common::Actors) -> Pubkey {
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

/// S1-ARC-2 — cancel from Paused MUST emit `had_paused_satellite == true`
/// (and `had_graced_satellite == false`). Direct pin of the S2-added field.
#[test]
fn cancel_from_paused_event_records_had_paused_satellite_true() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_paused_subscription(&mut env, &actors, T0 + 30);

    clock::set_clock(&mut env.svm, T0 + 500);
    env.svm.expire_blockhash();
    let result = send_tx(
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

    let payload = decode_cancelled_event(&result);
    assert!(
        payload.had_paused_satellite,
        "cancel-from-Paused MUST set had_paused_satellite=true (S1-ARC-2)"
    );
    assert!(
        !payload.had_graced_satellite,
        "no grace satellite in a Paused cancel"
    );
    assert_eq!(payload.cancelled_by, actors.subscriber.pubkey());
}

/// S1-ARC-2 (false case) — cancel from Active MUST emit
/// `had_paused_satellite == false`. Proves the flag is not hard-wired true.
#[test]
fn cancel_from_active_event_records_had_paused_satellite_false() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_active_subscription(&mut env, &actors);

    clock::set_clock(&mut env.svm, T0 + 30);
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
    )
    .expect("cancel from Active");

    let payload = decode_cancelled_event(&result);
    assert!(
        !payload.had_paused_satellite,
        "cancel-from-Active MUST set had_paused_satellite=false (S1-ARC-2)"
    );
    assert!(!payload.had_graced_satellite);
}

/// S1-ARC-3 — when the MERCHANT signs cancel-from-Paused, the PausedSubscription
/// satellite rent (which the merchant created at `pause`) flows to the
/// SUBSCRIBER, not back to the cancelling merchant. Ratified 2026-06-10.
///
/// We isolate the satellite rent from the vault rent by measuring the
/// subscriber lamport delta and asserting the merchant (cancel actor) does NOT
/// gain lamports. The vault close also pays subscriber; both rents land on the
/// subscriber wallet, and the merchant only pays the tx fee.
#[test]
fn cancel_from_paused_satellite_rent_flows_to_subscriber() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let sub_pk = setup_paused_subscription(&mut env, &actors, T0 + 30);
    let (paused_pk, _) = paused_sub_pda(&sub_pk);

    // The satellite is live and rent-bearing pre-cancel.
    let satellite_rent = env
        .svm
        .get_account(&paused_pk)
        .expect("paused satellite alive")
        .lamports;
    assert!(satellite_rent > 0, "satellite must hold rent pre-cancel");

    let pre_subscriber = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    let pre_merchant = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("merchant alive")
        .lamports;

    clock::set_clock(&mut env.svm, T0 + 500);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant, // merchant is the cancel actor (ADR-009 polymorphic)
        &[ix::cancel_ix_with_paused(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.merchant],
    )
    .expect("merchant cancel from Paused");

    // Satellite closed.
    let post_satellite = env.svm.get_account(&paused_pk);
    assert!(
        post_satellite.is_none() || post_satellite.map(|a| a.lamports == 0).unwrap_or(true),
        "PausedSubscription satellite closed by cancel"
    );

    let post_subscriber = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    let post_merchant = env
        .svm
        .get_account(&actors.merchant.pubkey())
        .expect("merchant alive")
        .lamports;

    // Subscriber gains AT LEAST the satellite rent (plus the vault rent).
    let subscriber_gain = post_subscriber - pre_subscriber;
    assert!(
        subscriber_gain >= satellite_rent,
        "subscriber lamport gain ({}) must cover the satellite rent ({}) → \
         S1-ARC-3 ratified rent-to-subscriber",
        subscriber_gain,
        satellite_rent
    );

    // Merchant (cancel actor) forfeits the satellite rent it created and only
    // pays the tx fee — never gains lamports. ADR-009 §"Rent-flow invariant".
    assert!(
        post_merchant <= pre_merchant,
        "merchant must NOT receive any satellite/vault rent when signing cancel"
    );
}
