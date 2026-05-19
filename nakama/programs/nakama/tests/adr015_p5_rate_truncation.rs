//! ADR-015 §F4 — rate-truncation economic skew
//! (security-audit-patterns.md §P5). Expansion of the smoke regression in
//! `adr015_security_remediation.rs::cancel_at_period_boundary_full_price...`.
//!
//! Test rationale: pre-F4 math is `unlocked = rate_per_second * elapsed`
//! where `rate_per_second = price / period` (integer-truncated at subscribe).
//! Post-F4 math is `unlocked = (elapsed * price) / period` — one divide at
//! the end, exact to the base-unit.
//!
//! Cases pinned here:
//! * Cancel at exactly half period — pro-rata to the base unit.
//! * Cancel at `period - 1s` — 1-second residue refund.
//! * Multi-period prefund cancel partway through lifetime.
//! * Subscribe rejection on extreme price/period where `rate_per_second = 0`
//!   (`ZeroRatePerSecond` is still the smoke guard at subscribe; F4 doesn't
//!   make it dead code).
//! * (Documented gap): u128 overflow path. Out of practical reach for any
//!   plan that subscribe accepts; documented in handoff Open issues.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda, Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

fn read_subscription(svm: &litesvm::LiteSVM, sub_pk: &Pubkey) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

/// $10/month plan canceled at exactly mid-period (1_296_000s after subscribe).
///
/// Old math: `rate = 10_000_000 / 2_592_000 = 3` (truncated). At elapsed
/// = 1_296_000, unlocked_old = 3 * 1_296_000 = 3_888_000 → refund =
/// 6_112_000 (over-refund subscriber, under-pay merchant ~22%).
///
/// New (F4) math: `unlocked = 1_296_000 * 10_000_000 / 2_592_000 = 5_000_000`
/// → refund = 5_000_000. Exactly half.
#[test]
fn cancel_mid_period_exact_pro_rata() {
    let plan_price: u64 = 10_000_000; // $10 USDC
    let plan_period: i64 = 2_592_000; // 30 days
    let mut env = setup();
    let actors = fund_actors(&mut env, plan_price);
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
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
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    // Mid-period cancel.
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let pre_subscriber = token_balance(&env.svm, &actors.subscriber_ata);

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
    .expect("cancel at half period");

    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    let subscriber_refund = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber;

    // F4 expected: 5_000_000 / 5_000_000.
    assert_eq!(
        merchant_delta, 5_000_000,
        "merchant earns exactly half price on mid-period cancel"
    );
    assert_eq!(
        subscriber_refund, 5_000_000,
        "subscriber refund is exactly half deposit on mid-period cancel"
    );

    // Cross-property: pre-fix would have produced 3 * 1_296_000 = 3_888_000
    // to merchant, 6_112_000 refund. Assert we're NOT in the pre-fix regime.
    assert_ne!(
        merchant_delta, 3_888_000,
        "must NOT match pre-F4 truncated-rate value"
    );
    assert_ne!(
        subscriber_refund, 6_112_000,
        "must NOT match pre-F4 over-refund value"
    );
}

/// Cancel at exactly `period - 1s`. With F4 lazy precise math:
/// `unlocked = 2_591_999 * 10_000_000 / 2_592_000 = 9_999_996` (one-divide
/// truncation = 4 base-unit residue refund). Subscriber refund = 4.
#[test]
fn cancel_one_second_before_boundary_exact_residue() {
    let plan_price: u64 = 10_000_000;
    let plan_period: i64 = 2_592_000;
    let mut env = setup();
    let actors = fund_actors(&mut env, plan_price);
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
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
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    clock::set_clock(&mut env.svm, T0 + plan_period - 1);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let pre_subscriber = token_balance(&env.svm, &actors.subscriber_ata);

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
    .expect("cancel at period-1");

    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    let subscriber_refund = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber;

    // (2_591_999 * 10_000_000) / 2_592_000
    // = 25_919_990_000_000 / 2_592_000
    // = 9_999_996  (integer-floor at the SINGLE division)
    assert_eq!(
        merchant_delta, 9_999_996,
        "merchant earns price minus 1-period-residue"
    );
    assert_eq!(
        subscriber_refund, 4,
        "subscriber receives the precise 4-base-unit residue"
    );

    // Acceptance bound from ADR-015 — refund must be < 500_000.
    assert!(
        subscriber_refund < 500_000,
        "F4 acceptance bound from ADR-015 §Tests"
    );
}

/// Multi-period prefund. periods_to_prefund=12 (one year of $10/month),
/// cancel after exactly 6 periods. Pre-F4: rate=3, settle = 3 * 6 * 2_592_000
/// = 46_656_000 (vs the honest 60_000_000 → ~22% under-payment to merchant).
/// Post-F4: settle = 6 * 10_000_000 = 60_000_000, refund = 60_000_000.
#[test]
fn multi_period_prefund_cancel_at_half_lifetime() {
    let plan_price: u64 = 10_000_000;
    let plan_period: i64 = 2_592_000;
    let total_periods: u8 = 12;
    let mut env = setup();
    // Subscribe deposits 12 * 10_000_000 = 120_000_000. Subscriber needs that
    // much USDC in ATA at subscribe time. ATA wide margin to absorb rent.
    let actors = fund_actors(&mut env, plan_price * total_periods as u64 + 1_000);
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
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
            total_periods,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe 12 periods");

    // Cancel after exactly 6 full periods.
    clock::set_clock(&mut env.svm, T0 + 6 * plan_period);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let pre_subscriber = token_balance(&env.svm, &actors.subscriber_ata);

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
    .expect("cancel at 6 periods");

    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    let subscriber_refund = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber;

    assert_eq!(
        merchant_delta,
        6 * plan_price,
        "merchant earns exactly 6 periods worth (60_000_000)"
    );
    assert_eq!(
        subscriber_refund,
        6 * plan_price,
        "subscriber refund equals remaining 6 periods (60_000_000)"
    );

    // Pre-F4 value (would have been): 3 * 6 * 2_592_000 = 46_656_000.
    assert_ne!(
        merchant_delta, 46_656_000,
        "must NOT match pre-F4 under-payment"
    );
}

/// Extreme low rate — `price=1`, `period=10_000`. Pre-F4 `rate_per_second =
/// 1 / 10_000 = 0` → ADR-002 `ZeroRatePerSecond` smoke guard at subscribe
/// MUST still reject. F4 didn't remove this guard; it only kept it as a
/// validity probe at subscribe time. Pins that the safety net is still
/// engaged for genuinely-broken plans even after the math refactor.
#[test]
fn subscribe_with_extreme_low_rate_rejects_at_subscribe() {
    let plan_price: u64 = 1;
    let plan_period: i64 = 10_000;
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan with extreme low rate");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);

    clock::set_clock(&mut env.svm, T0);
    let r = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        )],
        &[&actors.subscriber],
    );
    // Subscribe enforces `rate_per_second > 0`; with `price/period = 0`, the
    // ADR-002 smoke guard fires. ADR-015 §F4 explicitly notes this guard
    // stays in place.
    assert_nakama_err::<()>(r, NakamaError::ZeroRatePerSecond);
}

/// Sanity: integer-clean case (price exactly divides period) — half-period
/// charge yields exactly half-price. Mirror of the existing smoke
/// `f4_math_unchanged_when_price_divides_period_exactly` but on `cancel`
/// (not charge), to pin both code paths.
#[test]
fn cancel_clean_division_no_residue() {
    let plan_price: u64 = 600; // 10/sec exact
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let _ = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0);
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

    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let pre_subscriber = token_balance(&env.svm, &actors.subscriber_ata);

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
    .expect("cancel mid-period clean");

    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    let subscriber_refund = token_balance(&env.svm, &actors.subscriber_ata) - pre_subscriber;

    assert_eq!(merchant_delta, plan_price / 2);
    assert_eq!(subscriber_refund, plan_price / 2);

    // Verify subscription state is now Cancelled tombstone.
    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(post.state, nakama::state::SubscriptionState::Cancelled);
}
