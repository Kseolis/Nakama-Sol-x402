//! ADR-015 §F2 — stream_start shift symmetry (security-audit-patterns.md §P2).
//! Expansion of the smoke regression in `adr015_security_remediation.rs`.
//!
//! Invariants pinned here:
//!
//! * **Active top_up** does NOT shift `stream_start`. Top-up funds simply
//!   extend the deposited balance; the unlock clock keeps ticking from the
//!   original anchor. Acceptance criterion §F2 line 453.
//! * **Paused top_up** does NOT shift `stream_start` (Paused freeze is handled
//!   by resume's shift; double-shift would over-credit subscriber). The
//!   ADR-007 §"top_up handler" branch is `GracePeriod → Active`, not
//!   `Paused → Active`.
//! * **GracePeriod top_up with `entered_grace_at == now`** is a no-op shift
//!   (grace_duration = 0). Edge of the F2 formula; must not error.
//! * **GracePeriod top_up with `entered_grace_at > now`** (forced clock
//!   regression) → `ClockBackwards` error.
//! * **Repeated grace recovery cycles**: each cycle adds its own
//!   `grace_duration` to `stream_start`. After N cycles, the anchor reflects
//!   the cumulative freeze.
//!
//! Black-box: state byte at `STATE_OFFSET` and `Subscription` borsh-decode
//! via `nakama::state::Subscription::deserialize`.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, grace_pda, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

fn read_subscription(svm: &litesvm::LiteSVM, sub_pk: &Pubkey) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

fn create_plan_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    price: u64,
    period: i64,
    periods: u8,
) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            price,
            period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (grace_pk, _) = grace_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0);
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

    (plan_pk, sub_pk, vault_pk, grace_pk)
}

/// ADR-015 §F2 boundary — top_up from `Active` MUST NOT shift stream_start.
/// Only the `GracePeriod → Active` recovery branch shifts. The acceptance
/// criterion in ADR-015 line 453 is explicit.
#[test]
fn top_up_from_active_does_not_shift_stream_start() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (_plan_pk, sub_pk, _vault_pk, _grace_pk) =
        create_plan_and_subscribe(&mut env, &actors, plan_price, plan_period, 2);

    // Read stream_start at subscribe time.
    let pre = read_subscription(&env.svm, &sub_pk);
    let stream_start_before = pre.stream_start;
    assert_eq!(pre.state, nakama::state::SubscriptionState::Active);

    // Top up from Active mid-stream. amount > 0 required.
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up from Active");

    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        post.stream_start, stream_start_before,
        "stream_start MUST NOT shift on top_up from Active"
    );
    assert_eq!(
        post.state,
        nakama::state::SubscriptionState::Active,
        "state stays Active after top_up from Active"
    );
    assert_eq!(
        post.deposited_amount,
        pre.deposited_amount + 500,
        "deposited_amount grows by exactly amount"
    );
}

/// ADR-015 §F2 boundary — top_up from `Paused` MUST NOT shift stream_start.
/// Paused freeze is settled by ADR-006 resume_handler's shift; top_up just
/// adds funds. Double-shifting would over-credit subscriber when resume
/// later fires.
#[test]
fn top_up_from_paused_does_not_shift_stream_start() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (_plan_pk, sub_pk, _vault_pk, _grace_pk) =
        create_plan_and_subscribe(&mut env, &actors, plan_price, plan_period, 2);

    // Drive to Paused.
    clock::set_clock(&mut env.svm, T0 + plan_period / 4);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let pre = read_subscription(&env.svm, &sub_pk);
    let stream_start_before = pre.stream_start;
    assert_eq!(pre.state, nakama::state::SubscriptionState::Paused);

    // top_up during Paused. ADR-007 §"Per-state eligibility" — Paused allowed,
    // satellite optional is None (Paused does not have a grace satellite).
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up from Paused");

    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        post.stream_start, stream_start_before,
        "stream_start MUST NOT shift on top_up from Paused"
    );
    assert_eq!(
        post.state,
        nakama::state::SubscriptionState::Paused,
        "state remains Paused after top_up (ADR-006 — only resume flips)"
    );
}

/// ADR-015 §F2 edge — `entered_grace_at == now` produces grace_duration = 0.
/// The shift is a no-op; recovery still flips state to Active. The handler
/// MUST NOT error on the degenerate `checked_sub(0)` path.
#[test]
fn top_up_at_zero_grace_duration_no_shift() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        create_plan_and_subscribe(&mut env, &actors, plan_price, plan_period, 2);

    // Drive to GracePeriod at exact exhaustion (T0 + 2*period).
    clock::set_clock(&mut env.svm, T0 + 2 * plan_period);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");
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
            Some(grace_pk),
        )],
        &[&keeper],
    )
    .expect("exhausting charge into grace");

    // Read pre-recovery stream_start. We DO NOT advance the clock — top_up
    // happens at the exact same timestamp as grace entry, so
    // `now == entered_grace_at` and grace_duration = 0.
    let pre = read_subscription(&env.svm, &sub_pk);
    let stream_start_before = pre.stream_start;
    assert_eq!(pre.state, nakama::state::SubscriptionState::GracePeriod);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            plan_price,
        )],
        &[&actors.subscriber],
    )
    .expect("zero-duration grace recovery is well-defined");

    let post = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        post.stream_start, stream_start_before,
        "zero-grace-duration recovery shift is a no-op"
    );
    assert_eq!(
        post.state,
        nakama::state::SubscriptionState::Active,
        "state flips to Active even with zero shift"
    );
}

/// ADR-015 §F2 — `entered_grace_at > now` (clock regressed inside grace
/// window). Handler MUST fail `ClockBackwards`, never produce a negative
/// shift (which would over-credit subscriber by aging the anchor backward).
#[test]
fn top_up_from_grace_with_clock_backwards_errors() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        create_plan_and_subscribe(&mut env, &actors, plan_price, plan_period, 2);

    // Drive to GracePeriod at T0 + 2*period.
    clock::set_clock(&mut env.svm, T0 + 2 * plan_period);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");
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
            Some(grace_pk),
        )],
        &[&keeper],
    )
    .expect("charge into grace");

    // Adversarial: regress the clock to before entered_grace_at. LiteSVM
    // permits this via `set_clock` (no validator monotonicity); production
    // sBPF clock is monotonic, so this is purely a defensive test for the
    // handler's `require!(now >= entered_grace_at)`. Per `common::clock`
    // docstring, `advance(svm, -K)` is explicitly supported for adversarial
    // pinning.
    clock::set_clock(&mut env.svm, T0 + plan_period); // < entered_grace_at

    env.svm.expire_blockhash();
    let r = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            plan_price,
        )],
        &[&actors.subscriber],
    );
    assert_nakama_err::<()>(r, NakamaError::ClockBackwards);
}

/// ADR-015 §F2 cumulative property. Two grace→recover cycles. After both,
/// stream_start MUST equal initial_stream_start + sum of grace_durations.
/// Pins that the shift compounds correctly across cycles, no off-by-one.
#[test]
fn repeated_grace_recovery_cycles_cumulative_shifts() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    // Subscriber needs enough USDC for 2 prefund + 2 top_ups = ~4 periods.
    let actors = fund_actors(&mut env, 4 * plan_price + 1000);
    let (plan_pk, sub_pk, vault_pk, grace_pk) =
        create_plan_and_subscribe(&mut env, &actors, plan_price, plan_period, 2);

    let initial = read_subscription(&env.svm, &sub_pk);
    let stream_start_t0 = initial.stream_start;

    // ── Cycle 1 ──
    // Exhaust at T0 + 2*period → grace.
    clock::set_clock(&mut env.svm, T0 + 2 * plan_period);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");
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
            Some(grace_pk),
        )],
        &[&keeper],
    )
    .expect("charge into grace #1");
    let grace_wait_1: i64 = 500;
    let recover_t1 = T0 + 2 * plan_period + grace_wait_1;
    clock::set_clock(&mut env.svm, recover_t1);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            // 2 periods so cycle 2's drain works.
            2 * plan_price,
        )],
        &[&actors.subscriber],
    )
    .expect("recover #1");

    let after_recover_1 = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        after_recover_1.stream_start,
        stream_start_t0 + grace_wait_1,
        "stream_start shifted by exactly grace_wait_1 after cycle 1"
    );
    assert_eq!(
        after_recover_1.state,
        nakama::state::SubscriptionState::Active
    );

    // ── Cycle 2 ──
    // After cycle 1: shifted stream_start = stream_start_t0 + grace_wait_1.
    // Cycle 1 settled withdrawn_amount = 2 * plan_price (full exhaustion at
    // T0+2*period); cycle 1 top_up added 2 * plan_price, so deposited =
    // 4 * plan_price, withdrawn = 2 * plan_price, claimable cap = 2 periods.
    // Drive to second exhaustion: another 2 periods AFTER the shifted anchor.
    let exhaust_t2 = after_recover_1.stream_start + 4 * plan_period;
    clock::set_clock(&mut env.svm, exhaust_t2);
    env.svm.expire_blockhash();
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
            Some(grace_pk),
        )],
        &[&keeper],
    )
    .expect("charge into grace #2");
    let grace_wait_2: i64 = 1_234;
    let recover_t2 = exhaust_t2 + grace_wait_2;
    clock::set_clock(&mut env.svm, recover_t2);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            plan_price,
        )],
        &[&actors.subscriber],
    )
    .expect("recover #2");

    let after_recover_2 = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        after_recover_2.stream_start,
        stream_start_t0 + grace_wait_1 + grace_wait_2,
        "stream_start shifts compound additively across two grace cycles"
    );
    assert_eq!(
        after_recover_2.state,
        nakama::state::SubscriptionState::Active
    );
}
