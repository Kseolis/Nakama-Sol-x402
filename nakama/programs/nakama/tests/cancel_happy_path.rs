//! Happy-path `cancel`.
//!
//! Coverage:
//! - ADR-002 §cancel pseudocode steps 1–12
//! - ADR-003 §Cancel decomposition (MVP fused: state=Cancelled then close)
//! - BLK-15 (vault close via SPL CPI)
//! - ADR-002 Tests #6 (mid-period: pro-rata refund + final settle, both accounts closed)

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer,
};

/// Source: ADR-002 §cancel — mid-period cancel produces fair pro-rata split.
///
/// Setup: plan price=600 µUSDC over period=60s ⇒ rate=10 µUSDC/s.
/// Subscribe with 2 periods prefund → vault holds 1200, deposited=1200.
/// Warp +30s ⇒ unlocked = min(1200, 30*10) = 300.
/// Cancel: merchant gets 300, subscriber gets 1200-300 = 900 refund.
#[test]
fn cancel_settles_pro_rata_and_closes_accounts() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let plan_id = 1u64;
    let price = 600u64; // tiny to keep math obvious
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
    let pre_cancel_subscriber = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_cancel_merchant = token_balance(&env.svm, &actors.merchant_ata);
    assert_eq!(token_balance(&env.svm, &vault_pk), deposited);

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

    // Subscription account closed (Anchor `close = subscriber`). LiteSVM
    // returns either `None` or a zero-lamport residual depending on close
    // semantics; we accept "no longer a Nakama-owned account".
    match env.svm.get_account(&sub_pk) {
        None => {}
        Some(a) => assert_eq!(
            a.lamports, 0,
            "subscription account should be closed (lamports zeroed)"
        ),
    }

    // Vault closed (BLK-15 explicit SPL close_account CPI).
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "vault should be closed (lamports zeroed)"),
    }

    // Merchant settle = 30s * 10/s = 300.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_cancel_merchant;
    assert_eq!(
        merchant_delta, 300,
        "merchant must receive 30s × rate=10 = 300 µUSDC"
    );

    // Subscriber refund = deposited - unlocked = 1200 - 300 = 900.
    let subscriber_delta = token_balance(&env.svm, &actors.subscriber_ata) - pre_cancel_subscriber;
    assert_eq!(subscriber_delta, 900, "subscriber must refund 900 µUSDC");
}
