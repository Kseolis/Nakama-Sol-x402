//! On-chain layout invariants.
//!
//! Coverage:
//! - ADR-001 §Plan account — total size 161 (153 borsh + 8 discriminator)
//! - ADR-001 §Subscription account — total size 275 (267 borsh + 8 disc)
//! - BLK-19 — `state` byte at offset 192 inside Subscription.data
//! - ADR-001 §Field-order rationale — `next_charge_at` at offset 8 (memcmp-friendly)

mod common;

use common::{
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer, STATE_OFFSET,
};

/// Source: ADR-001 §Plan account — `Plan` borsh footprint pinned at 153 bytes
/// (+8 disc = 161 on chain). Test materialises a Plan and reads its raw
/// account.data length.
#[test]
fn plan_account_total_size_is_161() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);

    let plan_id = 7u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let acct = env.svm.get_account(&plan_pk).expect("plan");
    assert_eq!(
        acct.data.len(),
        161,
        "Plan total bytes must be 161 (8 disc + 153 borsh) per ADR-001"
    );
}

/// Source: ADR-001 §Subscription account (revised 2026-04-27) — total bytes
/// = 275, with `state` byte at offset 192 (BLK-19) and `next_charge_at` at
/// offset 8 (memcmp-friendly).
#[test]
fn subscription_account_layout_offsets() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
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
    .expect("subscribe");

    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let data = env.svm.get_account(&sub_pk).expect("subscription").data;

    // Total size.
    assert_eq!(
        data.len(),
        275,
        "Subscription total bytes must be 275 (ADR-001 revised, BLK-01)"
    );

    // State at offset 192 == 0 (Active) right after subscribe (BLK-19).
    assert_eq!(
        data[STATE_OFFSET], 0,
        "state byte at offset 192 must be 0 (Active)"
    );

    // `next_charge_at` is the i64 at offset 8; since we set clock at the
    // default LiteSVM timestamp it must be > 0 (some sane future timestamp
    // = stream_start + period). Verify only that it's non-zero and
    // non-negative — exact value depends on Clock at subscribe time.
    let next_charge_at = i64::from_le_bytes(data[8..16].try_into().unwrap());
    assert!(
        next_charge_at > 0,
        "next_charge_at at offset 8 must be a positive timestamp, got {}",
        next_charge_at
    );
}
