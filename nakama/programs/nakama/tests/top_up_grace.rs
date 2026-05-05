//! Happy-path tests for `top_up` from `GracePeriod` — ADR-007 §I-TOPUP-6 +
//! §I-GRACE-3.
//!
//! Black-box: written from ADR-007 §"top_up handler" pseudocode +
//! §"Per-state eligibility table", NOT from `instructions/top_up.rs`.
//!
//! Coverage:
//! - I-TOPUP-6: top_up from GracePeriod transitions state to Active AND
//!   closes the GracedSubscription satellite. `deposited_amount += amount`.
//! - I-GRACE-3: the satellite is closed (account absent OR lamports == 0)
//!   on top_up from Grace. Rent flows to subscriber.
//! - Q5 (kickoff §3): top_up from passively-expired grace (state byte ==
//!   GracePeriod, `now > grace_until`) STILL succeeds. The on-chain state
//!   guard is byte-only, not time-dependent.
//! - Composability: post-recovery (state == Active again), a fresh
//!   `charge` succeeds — proves the recovery path closes the loop.
//! - I-LAYOUT-2: `Subscription.reserved` byte-equal `[0; 32]` post recovery.
//! - I-LAYOUT-3: `Subscription.vault_bump` byte-equal pre/post recovery.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer, GRACE_DURATION, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
const VAULT_BUMP_OFFSET: usize = 194;
const RESERVED_LEN: usize = 32;

/// Helper: create plan, subscribe (2 periods prefund), drive to GracePeriod
/// via natural charge-tail at exact exhaustion (T0 + 2*period).
/// Returns (plan, subscription, vault, graced) PDAs.
fn drive_to_grace(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (
    solana_pubkey::Pubkey,
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

    // advance to T = stream_start + 2*period — exactly exhaust 2-period prefund.
    clock::set_clock(&mut env.svm, T0 + 2 * PLAN_PERIOD);
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

    // Confirm we are in Grace.
    let sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(sub_acct.data[STATE_OFFSET], 2, "state must be GracePeriod");
    assert!(
        env.svm.get_account(&graced_pk).is_some(),
        "satellite present"
    );

    (plan_pk, sub_pk, vault_pk, graced_pk)
}

/// Source: ADR-007 §I-TOPUP-6 + §I-GRACE-3.
///
/// Drive to Grace, top_up with the satellite passed → state flips back to
/// Active (= 0); satellite is closed (account absent OR lamports == 0);
/// `deposited_amount` increments by `amount`. Subscriber lamports increase
/// by approximately the satellite rent (~10^6 lamports), modulo tx fees.
#[test]
fn top_up_grace_recovers_to_active() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk, vault_pk, graced_pk) = drive_to_grace(&mut env, &actors);

    // Snapshot pre-recovery.
    let pre_sub_data = env.svm.get_account(&sub_pk).expect("alive").data.clone();
    let pre_vault_bump = pre_sub_data[VAULT_BUMP_OFFSET];
    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber wallet")
        .lamports;
    let pre_vault_balance = token_balance(&env.svm, &vault_pk);

    let amount: u64 = 1_000;

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            amount,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up from Grace recovers to Active");

    // I-TOPUP-6: state byte flipped back to Active.
    let post_sub = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post_sub.data[STATE_OFFSET], 0,
        "I-TOPUP-6: state must flip to Active after top_up from Grace"
    );

    // I-LAYOUT-3: vault_bump unchanged.
    assert_eq!(
        post_sub.data[VAULT_BUMP_OFFSET], pre_vault_bump,
        "I-LAYOUT-3: vault_bump byte-equal pre/post recovery"
    );
    // I-LAYOUT-2: reserved still zeroed.
    let post_reserved = &post_sub.data[post_sub.data.len() - RESERVED_LEN..];
    assert_eq!(
        post_reserved, &[0u8; RESERVED_LEN],
        "I-LAYOUT-2: Subscription.reserved must remain [0; 32]"
    );

    // I-GRACE-3: satellite closed. Anchor `close = subscriber` zeroes
    // lamports on the satellite (account may remain in the SVM at zero
    // lamports OR disappear entirely depending on backend behavior; both
    // are valid "closed" indicators).
    match env.svm.get_account(&graced_pk) {
        None => {} // best outcome
        Some(a) => assert_eq!(
            a.lamports, 0,
            "I-GRACE-3: GracedSubscription must be closed (lamports zeroed) after top_up from Grace"
        ),
    }

    // I-GRACE-3: subscriber lamports increased (satellite rent returned).
    // A top_up tx pays its own fee; lower-bound the delta minus a fee
    // ceiling.
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("alive")
        .lamports;
    let fee_ceiling: u64 = 50_000;
    assert!(
        post_subscriber_lamports + fee_ceiling >= pre_subscriber_lamports + 100_000,
        "I-GRACE-3: subscriber lamports delta must approximate satellite rent (≥100k); \
         observed pre={} post={}",
        pre_subscriber_lamports,
        post_subscriber_lamports
    );

    // Vault balance increased by `amount`.
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        pre_vault_balance + amount,
        "vault must increase by exactly `amount` on top_up from Grace"
    );
}

/// Source: ADR-007 Q5 (kickoff §3) — passive grace expiry contract: the
/// on-chain state byte stays `GracePeriod` even when `now > grace_until`,
/// and `top_up` is byte-only-state-dependent. Therefore top_up succeeds
/// AFTER passive expiry as long as `cancel`/`top_up` was never called.
///
/// Demo angle (I-DEMO-1): subscriber rescues even after technical grace
/// expiry, as long as no cleanup has fired (and there is no on-chain
/// cleanup ix for grace).
#[test]
fn top_up_after_passive_grace_expiry_still_works() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk, _vault_pk, graced_pk) = drive_to_grace(&mut env, &actors);

    // Decode grace_until from satellite, then warp the clock past it.
    let body = env
        .svm
        .get_account(&graced_pk)
        .expect("satellite alive")
        .data;
    let grace_until = i64::from_le_bytes(body[8 + 32 + 8..8 + 32 + 16].try_into().unwrap());

    // advance to grace_until + 100 — definitely past passive expiry.
    clock::set_clock(&mut env.svm, grace_until + 100);

    // Sanity: state byte still GracePeriod (passive expiry doesn't change it).
    let pre = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        pre.data[STATE_OFFSET], 2,
        "state stays GracePeriod past expiry"
    );

    // top_up succeeds despite passive expiry.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up succeeds after passive grace expiry (Q5)");

    // State flipped to Active; satellite closed.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 0,
        "Q5: top_up after passive expiry must still recover to Active"
    );
    match env.svm.get_account(&graced_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "satellite closed post-recovery"),
    }
}

/// Source: ADR-007 §"Per-state eligibility table" + §I-CHARGE-3 — composability.
///
/// After Grace → top_up → Active, a fresh `charge` succeeds (the ADR-004
/// §2.h `IllegalStateForCharge` guard no longer fires, because state is
/// Active again). Loom-pitch flow (I-DEMO-1): subscriber rescues + service
/// resumes streaming.
#[test]
fn top_up_then_charge_resumes() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk, vault_pk, _graced_pk) = drive_to_grace(&mut env, &actors);

    // Top up enough to cover one more period's worth (1 period × rate=10 = 600).
    let amount: u64 = PLAN_PRICE;
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            amount,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up recovers to Active");

    let active = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        active.data[STATE_OFFSET], 0,
        "state == Active post-recovery"
    );

    // advance clock to T0 + 2*period + period — one new period worth unlocked
    // since exhaust_at. ADR-004 streaming math runs against the same
    // stream_start, so claimable at this point should be > 0 (the math is
    // monotonic in elapsed × rate; deposited grew by 600).
    let resume_at = T0 + 3 * PLAN_PERIOD;
    clock::set_clock(&mut env.svm, resume_at);

    // Pre-merchant balance.
    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    // Pass placeholder `None` for graced_subscription: this charge does NOT
    // re-exhaust (deposited_amount = 1200 + 600 = 1800; at t=T0+180 elapsed
    // is 180s × 10/s = 1800 — exactly exhausts again actually). Hmm — to
    // keep this test on the "no-grace" branch, we top up by *more* than
    // one period of rate, so the post-charge withdrawn < deposited.
    //
    // The above branch happens to land exactly at exhaustion. Top up an
    // extra full period so 2 unlock-periods-since-charge < 3 deposited
    // periods.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            PLAN_PRICE, // top up another period so we don't immediately re-exhaust
        )],
        &[&actors.subscriber],
    )
    .expect("second top_up to avoid immediate re-exhaust");

    // Now deposited_amount = 2400 (was 1800 + 600). At resume_at = T0 + 180,
    // unlocked = min(2400, 180*10) = 1800. withdrawn pre-charge = 1200
    // (settled at exhaust_at). claimable = 1800 - 1200 = 600.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
        )],
        &[&keeper],
    )
    .expect("charge after recovery resumes streaming");

    // Merchant got the 1-period claimable.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    assert_eq!(
        merchant_delta, PLAN_PRICE,
        "post-recovery charge must transfer exactly one period × rate"
    );

    // State is still Active (this charge did not exhaust).
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 0,
        "state must remain Active after non-exhausting charge"
    );
}

/// Source: ADR-007 Q5 + §"Storage decision" passive expiry.
///
/// One more pin on the same property: even after grace_until, the state
/// byte AND the satellite both persist as long as nothing closed them.
/// This proves we have NOT silently transitioned to Exhausted on-chain.
#[test]
fn passive_expiry_state_byte_unchanged() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk, _vault_pk, graced_pk) = drive_to_grace(&mut env, &actors);

    // Decode grace_until.
    let body = env.svm.get_account(&graced_pk).unwrap().data;
    let grace_until = i64::from_le_bytes(body[8 + 32 + 8..8 + 32 + 16].try_into().unwrap());

    // advance well past grace_until.
    clock::set_clock(&mut env.svm, grace_until + GRACE_DURATION);

    // No on-chain ix invoked → state must be unchanged.
    let sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        sub_acct.data[STATE_OFFSET], 2,
        "Q5 / passive expiry: state byte stays GracePeriod with no ix invoked"
    );
    assert!(
        env.svm.get_account(&graced_pk).is_some(),
        "satellite persists past grace_until without intervention"
    );
}
