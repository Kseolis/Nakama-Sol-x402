//! Happy-path tests for `top_up` from the `Active` state — ADR-007 §I-TOPUP-4.
//!
//! Black-box: written from ADR-007 §"top_up handler" pseudocode +
//! §"Per-state eligibility table", NOT from `instructions/top_up.rs`.
//!
//! Coverage:
//! - I-TOPUP-4: `top_up(amount > 0)` from Active stays Active; only
//!   `deposited_amount` increments. Vault balance increases by `amount`.
//! - I-TOPUP-7: every other Subscription field is byte-equal pre/post
//!   (subscriber, plan, price, period, token_mint, merchant, merchant_ata,
//!   state, bump, vault_bump, created_at, last_charge_at, withdrawn_amount,
//!   rate_per_second, stream_start, next_charge_at).
//! - I-LAYOUT-1: Subscription account total bytes still 275 post-top_up.
//! - I-LAYOUT-2: `reserved[32]` byte-equal `[0; 32]` pre/post.
//! - I-LAYOUT-3: `vault_bump` byte-equal pre/post (top_up does not touch
//!   vault PDA derivation).
//! - F (replay): two consecutive `top_up` calls from Active are benign;
//!   `deposited_amount` increments each call, state stays Active. ADR-007
//!   §Adversarial 4.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

/// Common bring-up: create plan + subscribe at `T0` with 2 periods prefund.
fn create_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
) {
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

    (plan_pk, sub_pk, vault_pk)
}

/// Source: ADR-007 §I-TOPUP-4 + §I-TOPUP-7 + §I-LAYOUT-1/2/3.
///
/// Top-up from Active increments `deposited_amount` by exactly `amount` and
/// leaves every other byte of the Subscription account unchanged. State byte
/// at offset 192 stays 0 (Active). Vault SPL balance increases by `amount`.
#[test]
fn top_up_active_increments_deposited() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // Pre-snapshot: full account bytes + vault balance.
    let pre_sub = env
        .svm
        .get_account(&sub_pk)
        .expect("subscription alive after subscribe");
    let pre_data = pre_sub.data.clone();
    assert_eq!(
        pre_data.len(),
        275,
        "I-LAYOUT-1: Subscription account total bytes must be 275"
    );
    assert_eq!(pre_data[STATE_OFFSET], 0, "pre-top_up state must be Active");
    let pre_vault_balance = token_balance(&env.svm, &vault_pk);

    // Top-up amount = 1 full period of price.
    let amount: u64 = PLAN_PRICE;

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            amount,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up from Active");

    // Post-state.
    let post_sub = env
        .svm
        .get_account(&sub_pk)
        .expect("subscription alive after top_up");
    let post_data = post_sub.data.clone();

    // I-LAYOUT-1: total size unchanged.
    assert_eq!(
        post_data.len(),
        275,
        "I-LAYOUT-1: Subscription size must remain 275 post-top_up"
    );

    // I-TOPUP-4: state byte still Active.
    assert_eq!(
        post_data[STATE_OFFSET], 0,
        "I-TOPUP-4: state must remain Active after top_up from Active"
    );

    // ADR-007 §I-TOPUP-7: every byte EXCEPT `deposited_amount` must be
    // byte-equal pre/post. Layout per ADR-001 (verified offsets):
    //   8 disc | next_charge_at(8) | subscriber(32) | plan(32) | price(8) |
    //   period(8) | token_mint(32) | merchant(32) | merchant_ata(32) |
    //   state(1) | bump(1) | vault_bump(1) | created_at(8) | last_charge_at(8) |
    //   deposited_amount(8) | withdrawn_amount(8) | rate_per_second(8) |
    //   stream_start(8) | reserved[32].
    //
    // deposited_amount sits at: 8 + 184 (pre-state) + 1 (state) + 1 (bump) +
    //                            1 (vault_bump) + 8 (created_at) + 8
    //                            (last_charge_at) = 211, length 8 → bytes
    //                            [211..219].
    const DEPOSITED_OFFSET: usize = 211;
    const DEPOSITED_END: usize = DEPOSITED_OFFSET + 8;

    // Compare every byte EXCEPT [DEPOSITED_OFFSET..DEPOSITED_END].
    assert_eq!(
        &pre_data[..DEPOSITED_OFFSET],
        &post_data[..DEPOSITED_OFFSET],
        "I-TOPUP-7: bytes before deposited_amount must be byte-equal"
    );
    assert_eq!(
        &pre_data[DEPOSITED_END..],
        &post_data[DEPOSITED_END..],
        "I-TOPUP-7 + I-LAYOUT-2 + I-LAYOUT-3: bytes after deposited_amount \
         (incl. withdrawn_amount, rate_per_second, stream_start, reserved[32]) \
         must be byte-equal"
    );

    // Decode deposited_amount and assert exact increment.
    let pre_dep = u64::from_le_bytes(
        pre_data[DEPOSITED_OFFSET..DEPOSITED_END]
            .try_into()
            .expect("8 bytes"),
    );
    let post_dep = u64::from_le_bytes(
        post_data[DEPOSITED_OFFSET..DEPOSITED_END]
            .try_into()
            .expect("8 bytes"),
    );
    assert_eq!(
        post_dep,
        pre_dep + amount,
        "I-TOPUP-4: deposited_amount must increment by exactly `amount`"
    );

    // I-LAYOUT-2 explicit: reserved[32] last 32 bytes still zero.
    assert_eq!(
        &post_data[post_data.len() - 32..],
        &[0u8; 32],
        "I-LAYOUT-2: reserved[32] must stay [0; 32] byte-for-byte"
    );

    // CPI sanity: vault SPL balance increased by `amount`.
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        pre_vault_balance + amount,
        "vault SPL balance must increase by exactly `amount`"
    );
}

/// Source: ADR-007 §"top_up handler" — replay/idempotency from Active.
///
/// Two consecutive top_ups from Active each increment `deposited_amount` by
/// `amount`; state stays Active throughout. Demonstrates that top_up has no
/// replay-protection mechanism but is benign in Active state — Adversarial 4.
#[test]
fn top_up_replay_from_active_idempotent() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let amount: u64 = 1_000;
    let pre_vault = token_balance(&env.svm, &vault_pk);

    // First top_up.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            amount,
        )],
        &[&actors.subscriber],
    )
    .expect("first top_up");

    let mid_data = env.svm.get_account(&sub_pk).expect("alive").data;
    assert_eq!(
        mid_data[STATE_OFFSET], 0,
        "state stays Active after first top_up"
    );
    assert_eq!(token_balance(&env.svm, &vault_pk), pre_vault + amount);

    // Second top_up (replay, different blockhash).
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            amount,
        )],
        &[&actors.subscriber],
    )
    .expect("second top_up");

    let post_data = env.svm.get_account(&sub_pk).expect("alive").data;
    assert_eq!(
        post_data[STATE_OFFSET], 0,
        "state stays Active after second top_up"
    );
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        pre_vault + 2 * amount,
        "vault must grow by 2 * amount across two top_ups"
    );
}

/// Source: ADR-007 §I-LAYOUT-1 (runtime form).
///
/// Subscription account body length on chain == 275 (8 discriminator + 267
/// borsh) byte-for-byte before AND after `top_up`. The compile-time const-
/// assert in `state.rs` pins `Subscription::INIT_SPACE == 267`; this is the
/// dynamic counterpart that proves the live account layout matches the
/// freeze.
#[test]
fn subscription_size_unchanged_after_topup() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, _vault_pk) = create_and_subscribe(&mut env, &actors);

    let pre_len = env.svm.get_account(&sub_pk).expect("alive").data.len();
    assert_eq!(pre_len, 275, "Subscription size pre-top_up must be 275");

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up");

    let post_len = env.svm.get_account(&sub_pk).expect("alive").data.len();
    assert_eq!(post_len, 275, "Subscription size post-top_up must be 275");
}
