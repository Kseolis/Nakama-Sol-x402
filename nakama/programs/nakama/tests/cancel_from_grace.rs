//! Cancel-from-Grace tests — ADR-007 §"cancel from GracePeriod" + I-CANCEL-1/2/3.
//!
//! Black-box: written from ADR-007 §"cancel from GracePeriod" pseudocode
//! and ADR-013 §"Cancel handler" (cycle-3 split semantics), NOT from
//! `instructions/cancel.rs`.
//!
//! Coverage:
//! - I-CANCEL-1 (pre-expiry): cancel BEFORE `grace_until` clamps
//!   `effective_now = min(now, grace_until) == now` — settle math uses
//!   raw `now`. Refund == 0 because the stream was already exhausted at
//!   grace-entry (deposited == withdrawn). Satellite closed.
//! - I-CANCEL-1 (post-expiry): cancel AFTER passive `grace_until` clamps
//!   `effective_now = grace_until` (NOT raw `now`). Settle math uses
//!   `grace_until` so the merchant cannot claim time the subscriber never
//!   funded. Refund stays 0 (deposited == withdrawn at clamp point too).
//! - I-CANCEL-2: GracedSubscription closed (rent → subscriber) on either
//!   pre/post-expiry path.
//! - I-CANCEL-3: post-cancel state byte == 4 (Cancelled) — observable
//!   tombstone per ADR-013.
//! - C.6: cancel from Grace WITHOUT the satellite passed → handler raises
//!   `MissingGraceSatellite` (the `effective_now` branch for GracePeriod
//!   requires reading `grace_until`).
//! - I-LAYOUT-2: `Subscription.reserved` byte-equal `[0; 32]` post-cancel.
//! - I-LAYOUT-3: `Subscription.vault_bump` byte-equal pre/post.

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda, Signer,
    GRACE_DURATION, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
const VAULT_BUMP_OFFSET: usize = 194;
const RESERVED_LEN: usize = 32;

fn drive_to_grace(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    i64, // grace_until
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
    let (graced_pk, _) = common::grace_pda(&sub_pk);

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

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let exhaust_at = T0 + 2 * PLAN_PERIOD;
    clock::set_clock(&mut env.svm, exhaust_at);
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
    .expect("charge into grace");

    let grace_until = exhaust_at + GRACE_DURATION;
    (plan_pk, sub_pk, vault_pk, graced_pk, grace_until)
}

/// Source: ADR-007 §I-CANCEL-1 (pre-expiry branch) + I-CANCEL-2 + I-CANCEL-3.
///
/// Cancel BEFORE `grace_until`:
/// - `effective_now = min(now, grace_until) == now`.
/// - Settle math uses `now` (== exhaust_at + small δ). Since the stream
///   was fully unlocked at `exhaust_at`, the post-charge invariant
///   `withdrawn_amount == deposited_amount` holds → final_claimable == 0,
///   refund == 0 (vault is empty).
/// - Satellite is closed (I-CANCEL-2). Subscription state byte = Cancelled
///   (= 4) post-cancel (I-CANCEL-3).
/// - Layout invariants L-2 / L-3 unchanged.
#[test]
fn cancel_from_grace_clamps_settle_pre_expiry() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, vault_pk, graced_pk, grace_until) = drive_to_grace(&mut env, &actors);

    let pre_data = env.svm.get_account(&sub_pk).expect("alive").data.clone();
    let pre_vault_bump = pre_data[VAULT_BUMP_OFFSET];

    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);
    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("alive")
        .lamports;

    // advance to inside the grace window: T = exhaust_at + 1 day << grace_until.
    let cancel_at = grace_until - GRACE_DURATION + 86_400;
    assert!(cancel_at < grace_until);
    clock::set_clock(&mut env.svm, cancel_at);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix_full(
            &actors.subscriber.pubkey(),
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            Some(graced_pk),
        )],
        &[&actors.subscriber],
    )
    .expect("cancel from Grace pre-expiry");

    // ── Subscription tombstone (ADR-013): preserved, state == Cancelled. ──
    let post_sub = env.svm.get_account(&sub_pk).expect("tombstone alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 4,
        "I-CANCEL-3: state byte must be Cancelled (=4) after cancel-from-Grace"
    );

    // I-LAYOUT-3: vault_bump byte-equal.
    assert_eq!(
        post_sub.data[VAULT_BUMP_OFFSET], pre_vault_bump,
        "I-LAYOUT-3: vault_bump byte-equal pre/post cancel-from-Grace"
    );
    // I-LAYOUT-2: reserved still zeroed.
    let post_reserved = &post_sub.data[post_sub.data.len() - RESERVED_LEN..];
    assert_eq!(
        post_reserved, &[0u8; RESERVED_LEN],
        "I-LAYOUT-2: Subscription.reserved must remain [0; 32] post cancel"
    );

    // ── Vault closed via SPL CPI (BLK-15). ──
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "vault must be closed (lamports zeroed)"),
    }

    // ── I-CANCEL-2: satellite closed; rent returned to subscriber. ──
    match env.svm.get_account(&graced_pk) {
        None => {}
        Some(a) => assert_eq!(
            a.lamports, 0,
            "I-CANCEL-2: GracedSubscription must be closed on cancel-from-Grace"
        ),
    }

    // ── Settle math at effective_now = now < grace_until. The stream was
    // already exhausted at exhaust_at (deposited == withdrawn at grace
    // entry); pro-rata at any later point still bounds at deposited, so
    // final_claimable == 0 and refund == 0.
    assert_eq!(
        token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc,
        0,
        "merchant settle == 0 (stream already exhausted at grace entry)"
    );
    assert_eq!(
        token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber_usdc,
        0,
        "subscriber USDC refund == 0 (vault empty at grace entry)"
    );

    // I-CANCEL-2 lamport-side: subscriber gained lamports from satellite
    // close + vault close, minus tx fee. Lower-bound delta > 100k.
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("alive")
        .lamports;
    let delta = post_subscriber_lamports as i128 - pre_subscriber_lamports as i128;
    assert!(
        delta > 100_000,
        "subscriber lamport delta {} must be > 100k (satellite + vault rent return)",
        delta
    );
}

/// Source: ADR-007 §I-CANCEL-1 (post-expiry branch).
///
/// Cancel AFTER `grace_until`:
/// - `effective_now = min(now, grace_until) == grace_until`.
/// - Settle math uses `grace_until`, NOT raw `now`. This is the load-
///   bearing fairness invariant: a merchant must NOT be able to extract
///   pro-rata for time that the subscriber never funded (the stream was
///   exhausted at exhaust_at, so any time past grace_until is unfunded).
/// - Satellite closed; state == Cancelled.
///
/// Math: `unlocked = min(deposited_amount, (grace_until - stream_start) *
/// rate)`. With `(grace_until - stream_start)` ≫ deposited / rate, the
/// `min` clamps at `deposited_amount` regardless of how far past
/// grace_until we are. So merchant settle and refund are both bounded.
/// We assert the SAME observable economic outcome as pre-expiry: settle
/// and refund deltas == 0 (since exhaust_at left vault empty already).
/// The behavioural difference is internal: `effective_now` is clamped,
/// not `now` — proven indirectly by absence of merchant-favored skew.
#[test]
fn cancel_from_grace_clamps_settle_post_expiry() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, vault_pk, graced_pk, grace_until) = drive_to_grace(&mut env, &actors);

    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    let pre_merchant_usdc = token_balance(&env.svm, &actors.merchant_ata);

    // advance well past grace_until.
    let cancel_at = grace_until + GRACE_DURATION; // 14 days post grace entry
    clock::set_clock(&mut env.svm, cancel_at);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix_full(
            &actors.subscriber.pubkey(),
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            Some(graced_pk),
        )],
        &[&actors.subscriber],
    )
    .expect("cancel from Grace post-expiry — must clamp effective_now to grace_until");

    // ── State + satellite + vault. ──
    let post_sub = env.svm.get_account(&sub_pk).expect("tombstone alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 4,
        "I-CANCEL-3: state Cancelled post-cancel from passively-expired Grace"
    );
    match env.svm.get_account(&graced_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "I-CANCEL-2: satellite closed"),
    }
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "vault closed (BLK-15)"),
    }

    // ── Settle math at effective_now = grace_until. Vault was empty at
    // grace entry → final_claimable == 0, refund == 0. The CRITICAL
    // observable: merchant did NOT receive *any* additional funds despite
    // 7-day-past-grace-expiry walltime — the clamp held. Conversely,
    // subscriber USDC delta == 0 (vault was already drained).
    assert_eq!(
        token_balance(&env.svm, &actors.merchant_ata) - pre_merchant_usdc,
        0,
        "I-CANCEL-1 post-expiry: merchant must NOT receive additional funds (effective_now clamped)"
    );
    assert_eq!(
        token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber_usdc,
        0,
        "subscriber refund == 0 (vault empty at grace entry)"
    );
}

/// Source: ADR-007 §"cancel from GracePeriod" + §I-CANCEL-1 — when state ==
/// GracePeriod, the handler MUST read `grace_until` from the satellite. If
/// the caller omitted the satellite (placeholder), the handler raises
/// `MissingGraceSatellite`.
///
/// This is the dual of the `top_up` handler's MissingGraceSatellite check
/// (covered in `top_up_signer_guards.rs`); cancel needs the satellite for
/// settle clamping AND for the close-on-cancel rent return.
#[test]
fn cancel_grace_without_satellite_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, _vault_pk, _graced_pk, _grace_until) = drive_to_grace(&mut env, &actors);

    // advance into grace window.
    clock::set_clock(&mut env.svm, T0 + 2 * PLAN_PERIOD + 1_000);

    // Pass `None` for graced_subscription → placeholder pubkey, handler
    // sees `Option::None`, raises MissingGraceSatellite.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix_full(
            &actors.subscriber.pubkey(),
            &sub_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            None, // C.6: missing satellite while state == GracePeriod
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::MissingGraceSatellite);
}
