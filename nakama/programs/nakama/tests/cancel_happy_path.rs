//! Happy-path `cancel` (ADR-013 cycle-3 split semantics).
//!
//! Coverage:
//! - ADR-002 §cancel pseudocode steps 1–9 (settle + refund math)
//! - ADR-013 §"Cancel handler" — Subscription account **preserved as
//!   tombstone**; only the vault is closed. `state == Cancelled` (= 4)
//!   persists at STATE_OFFSET = 192 and is observable on-chain (BLK-19).
//! - ADR-013 invariants 3, 4, 5 (tombstone alive + vault closed + state byte).
//! - BLK-15 (vault close via SPL `close_account` CPI).
//!
//! Cycle-2 baseline asserted Subscription closed in the same instruction.
//! Post-split (this cycle), that flips: Subscription stays alive, lamports
//! unchanged, the rent-return moment moves to `cleanup`.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer, STATE_OFFSET,
};

/// Source: ADR-002 §cancel mid-period math + ADR-013 §"Cancel handler".
///
/// Setup: plan price=600 µUSDC over period=60s ⇒ rate=10 µUSDC/s.
/// Subscribe with 2 periods prefund → vault holds 1200, deposited=1200.
/// Warp +30s ⇒ unlocked = min(1200, 30*10) = 300.
/// Cancel: merchant gets 300, subscriber gets 1200-300 = 900 refund.
///
/// **Post-split assertions** (vs cycle-2):
/// - Subscription account **alive** post-cancel (still rent-paying).
/// - `state` byte at offset 192 == 4 (`SubscriptionState::Cancelled`).
/// - Vault closed (BLK-15) — unchanged from cycle-2.
#[test]
fn cancel_settles_pro_rata_keeps_subscription_alive_closes_vault() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let plan_id = 1u64;
    let price = 600u64;
    let period = 60i64;
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

    // Pin clock at a known point, then subscribe → stream_start = T0.
    let t0: i64 = 1_700_000_000;
    clock::set_clock(&mut env.svm, t0);

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

    let deposited = price * 2; // 1200
    let pre_cancel_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_cancel_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);
    assert_eq!(token_balance(&env.svm, &vault_pk), deposited);

    // Snapshot subscription account state pre-cancel — we'll prove the
    // tombstone has the same data length & rent post-cancel (proof of "alive").
    let pre_sub_acct = env
        .svm
        .get_account(&sub_pk)
        .expect("subscription alive after subscribe");
    let pre_sub_lamports = pre_sub_acct.lamports;
    let pre_sub_data_len = pre_sub_acct.data.len();
    assert_eq!(
        pre_sub_acct.data[STATE_OFFSET], 0,
        "pre-cancel state byte must be Active (0)"
    );

    // Half a period later.
    clock::set_clock(&mut env.svm, t0 + 30);

    // Cancel.
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

    // ---- ADR-013 invariant 3: Subscription account ALIVE post-cancel. ----
    let post_sub_acct = env
        .svm
        .get_account(&sub_pk)
        .expect("ADR-013: Subscription must persist as tombstone after cancel");
    assert_eq!(
        post_sub_acct.lamports, pre_sub_lamports,
        "tombstone subscription rent must be unchanged (no Anchor close on cancel post-split)"
    );
    assert_eq!(
        post_sub_acct.data.len(),
        pre_sub_data_len,
        "tombstone data length must be unchanged (account not closed)"
    );

    // ---- ADR-013 invariant 5: state byte == 4 (Cancelled) at STATE_OFFSET. ----
    assert_eq!(
        post_sub_acct.data[STATE_OFFSET], 4,
        "state byte at offset 192 must be SubscriptionState::Cancelled (4) post-cancel — \
         observable to indexers and x402 satellites per ADR-013 §x402 forward-compat"
    );

    // ---- ADR-013 invariant 4: vault closed (BLK-15). ----
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(
            a.lamports, 0,
            "vault should be closed via SPL close_account CPI (lamports zeroed)"
        ),
    }

    // ---- ADR-002 §cancel math. ----
    // Merchant settle = 30s * 10/s = 300.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_cancel_merchant_usdc;
    assert_eq!(
        merchant_delta, 300,
        "merchant must receive 30s × rate=10 = 300 µUSDC"
    );
    // Subscriber refund = deposited - unlocked = 1200 - 300 = 900.
    let subscriber_delta =
        token_balance(&env.svm, &actors.subscriber_ata) - pre_cancel_subscriber_usdc;
    assert_eq!(subscriber_delta, 900, "subscriber must refund 900 µUSDC");
}
