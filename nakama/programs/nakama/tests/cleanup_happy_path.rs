//! Happy-path `cleanup` (ADR-013 cycle-3 — terminal account-lifecycle action).
//!
//! Coverage:
//! - ADR-013 §"Cleanup handler" — close Subscription account, lamports →
//!   subscriber. Subscriber is the rent beneficiary (Q1, Q6).
//! - ADR-013 invariant 5: state == 4 byte at STATE_OFFSET=192 observable
//!   on-chain between cancel and cleanup.
//! - ADR-013 invariant 8: cleanup closes Subscription, returns rent.
//! - ADR-013 invariant 9: cleanup → resubscribe pattern works (PDA freed
//!   by Anchor close).
//!
//! Black-box: drives the public ABI (subscribe → cancel → cleanup → subscribe),
//! reads lamport balances and the state byte at offset 192 only.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

fn create_plan_subscribe_cancel(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            PLAN_PRICE,
            PLAN_PERIOD,
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

    // Warp + cancel → tombstone state.
    clock::set_clock(&mut env.svm, T0 + 30);
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
    .expect("cancel");

    (plan_pk, sub_pk)
}

/// Source: ADR-013 §"Cleanup handler" + invariants 5, 8.
///
/// Flow:
/// 1. subscribe → cancel → assert Subscription alive + state=4 byte at offset 192.
/// 2. cleanup → assert Subscription closed (account gone or lamports=0),
///    subscriber lamports balance increased by the original Subscription rent.
///
/// Rent figure verified empirically rather than asserting an exact magic
/// number — Anchor 1.0 / Solana rent calculations are version-sensitive.
/// We snapshot the tombstone's lamports pre-cleanup and assert the same
/// amount lands in subscriber's lamports post-cleanup.
#[test]
fn cleanup_after_cancel_closes_subscription_returns_rent() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_subscribe_cancel(&mut env, &actors);

    // ADR-013 invariant 5: state == 4 (Cancelled) on the tombstone.
    let pre_cleanup = env
        .svm
        .get_account(&sub_pk)
        .expect("Subscription must persist as tombstone (ADR-013 invariant 3)");
    assert_eq!(
        pre_cleanup.data[STATE_OFFSET], 4,
        "state byte at offset 192 must be Cancelled (4) before cleanup"
    );
    let tombstone_rent = pre_cleanup.lamports;
    assert!(
        tombstone_rent > 0,
        "tombstone must hold non-zero rent before cleanup"
    );

    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber wallet exists")
        .lamports;

    // Cleanup tx pays its own fee from `subscriber`, so we cannot just compare
    // lamports raw — fees and the rent return both move the needle. We
    // instead assert the tombstone disappeared AND that the lamport delta is
    // positive within a fee envelope (Anchor / SDK rent return magnitude is
    // ~2.19M lamports, fees on a 2-account ix are ~5k).
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup");

    // ADR-013 invariant 8a: Subscription closed.
    match env.svm.get_account(&sub_pk) {
        None => {} // best outcome — account fully gone
        Some(a) => {
            assert_eq!(
                a.lamports, 0,
                "Subscription must be closed by cleanup (lamports zeroed)"
            );
            // Anchor's close zeroes out the data discriminator. We don't pin
            // an exact length here; the lamport==0 invariant is the
            // load-bearing check for "account closed".
        }
    }

    // ADR-013 invariant 8b: rent returned to subscriber.
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber wallet exists")
        .lamports;

    // Lower bound: subscriber must have received at least (tombstone_rent − tx_fee_envelope).
    // tx_fee_envelope on LiteSVM defaults to 5_000 lamports per signature; we
    // give a generous 50_000 ceiling to absorb future LiteSVM fee changes.
    let fee_ceiling: u64 = 50_000;
    assert!(
        post_subscriber_lamports + fee_ceiling >= pre_subscriber_lamports + tombstone_rent,
        "subscriber lamports delta {} must approximate tombstone rent {} (fee envelope {})",
        post_subscriber_lamports as i128 - pre_subscriber_lamports as i128,
        tombstone_rent,
        fee_ceiling
    );
}

/// Source: ADR-013 invariant 9 + Q7 — after cleanup the Subscription PDA is
/// free again, so a fresh `subscribe` with the same `(subscriber, plan)`
/// seeds succeeds.
///
/// This proves the explicit-cleanup default for re-subscribe (ADR-003 §Re-
/// subscribe race option 1; ADR-008 critical path).
#[test]
fn cleanup_then_resubscribe_works() {
    let mut env = setup();
    // Fund the subscriber with enough USDC for two subscribe rounds.
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) = create_plan_subscribe_cancel(&mut env, &actors);

    // Cleanup the tombstone.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup");

    // The PDA address is now free; a fresh subscribe must succeed.
    env.svm.expire_blockhash();
    clock::set_clock(&mut env.svm, T0 + 1_000); // fresh stream_start far from old window

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("re-subscribe must succeed after cleanup (ADR-013 invariant 9)");

    // The fresh Subscription is in Active state (byte 0).
    let fresh = env
        .svm
        .get_account(&sub_pk)
        .expect("fresh Subscription must exist post-resubscribe");
    assert_eq!(
        fresh.data[STATE_OFFSET], 0,
        "fresh subscription must have state == Active (0) at offset 192"
    );
}
