//! ADR-015 — Impl-Cycle-2 Security Remediation regression tests (F1, F2, F4).
//!
//! Black-box: written from ADR-015 §Decision text, NOT from internal
//! handler code. Each test pins one fix:
//!
//! * **F1 regression** — `charge_with_grace_satellite_on_healthy_sub_fails`
//!   reproduces the permissionless pre-init poison vector. Attacker
//!   plants the `[GRACE_SEED, sub]` PDA into a healthy mid-stream charge;
//!   the handler must reject with `UnexpectedGraceSatellite` instead of
//!   silently allocating the satellite (which would brick the next
//!   honest exhausting charge with `AccountAlreadyInUse`).
//!
//! * **F2 regression** —
//!   `top_up_from_grace_shifts_stream_start_so_charge_yields_partial`.
//!   After GracePeriod recovery via top_up, the unlock math must resume
//!   from the exhaustion moment (stream_start += grace_duration). The
//!   inverse property: an immediate charge transfers AT MOST the
//!   already-elapsed-post-recovery rate × price/period, never the full
//!   top-up.
//!
//! * **F4 regression** —
//!   `cancel_at_period_boundary_full_price_to_merchant_zero_refund`
//!   exercises a plan where `price % period != 0` (USDC $10/month → rate
//!   truncates from 3.858 to 3 base units/sec under the old math, an
//!   ~22% merchant under-payment). With lazy precise division, cancel at
//!   exact period boundary settles the full `price` to merchant and
//!   refunds zero to subscriber.

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, grace_pda, ix, plan_pda, send_tx, setup, subscription_pda, token_balance,
    vault_pda, Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;

// ─────────────────────────────────────────────────────────────────────
// F1 — permissionless pre-init grace satellite poison
// ─────────────────────────────────────────────────────────────────────

/// ADR-015 §F1. With a healthy subscription (`withdrawn != deposited`),
/// calling `charge` with `Some(grace_pda)` must fail with
/// `UnexpectedGraceSatellite`. Pre-fix: init succeeded, leaving a
/// pre-allocated satellite that bricks the next honest exhausting charge
/// at `system_program::create_account` (`Custom(0)`
/// AccountAlreadyInUse).
#[test]
fn charge_with_grace_satellite_on_healthy_sub_fails() {
    // Plan: 600 base units / 60s. Subscribe with 2 periods (= 1200 deposited).
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = grace_pda(&sub_pk);

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

    // Advance into the mid-stream window. After 1 period, claimable = 600
    // (1 period worth), withdrawn = 0; post-charge withdrawn = 600 <
    // deposited = 1200. Sub stays Active — F1 attack window.
    clock::set_clock(&mut env.svm, T0 + plan_period);

    // Adversarial keeper attempts to plant the grace satellite during a
    // healthy charge by passing `Some(graced_pk)`.
    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");

    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
            &common::token_program_id(),
            Some(graced_pk),
        )],
        &[&attacker],
    );
    assert_nakama_err::<()>(result, NakamaError::UnexpectedGraceSatellite);

    // Side-effect verification: subscription state byte still Active and
    // satellite NOT allocated (would be Some(...) if init had fired).
    let sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        sub_acct.data[STATE_OFFSET], 0,
        "state must remain Active after rejected attacker tx"
    );
    match env.svm.get_account(&graced_pk) {
        None => {}
        Some(a) => assert_eq!(
            a.owner,
            solana_pubkey::Pubkey::default(),
            "grace satellite must not exist post-rejection"
        ),
    }

    // Positive control: a clean honest charge (None placeholder) succeeds
    // on the same healthy sub, proving the fix didn't break the routine path.
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &attacker,
        &[ix::charge_ix(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &attacker.pubkey(),
        )],
        &[&attacker],
    )
    .expect("honest charge with None satellite still succeeds");
}

/// ADR-015 §F1 — boundary case: the legitimate exhausting charge with
/// `Some(grace_pda)` continues to succeed (and creates the satellite).
/// This pins that the F1 fix did not regress §I-CHARGE-1 (ADR-007).
#[test]
fn charge_into_grace_with_satellite_still_succeeds() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = grace_pda(&sub_pk);

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

    // Advance to exact exhaustion (2 * period).
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
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("exhausting charge with Some(satellite) must succeed");

    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 2,
        "state flipped to GracePeriod on exhausting charge"
    );
    assert!(
        env.svm.get_account(&graced_pk).is_some(),
        "grace satellite was allocated by init"
    );
}

// ─────────────────────────────────────────────────────────────────────
// F2 — top_up does not shift stream_start (merchant drain on recovery)
// ─────────────────────────────────────────────────────────────────────

/// ADR-015 §F2. Drive to GracePeriod, wait `K` seconds inside grace,
/// top_up one period of price, then immediately charge. The expected
/// claimable is the per-second rate × seconds elapsed POST-recovery (≈ 0,
/// since charge fires in the same tx) — NOT the entire top-up.
///
/// Pre-fix behaviour (the bug): without the stream_start shift,
/// `now - stream_start_old` is huge after grace, so `unlocked` saturates
/// to `deposited_new` and merchant drains the entire top-up in one call.
///
/// We assert the post-charge merchant delta is strictly less than the
/// top-up amount minus one period's worth — a wide margin that holds for
/// any K > 0.
#[test]
fn top_up_from_grace_shifts_stream_start_so_charge_yields_partial() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = grace_pda(&sub_pk);

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

    // Drive into grace at exact exhaustion (settles 2-period prefund).
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
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("charge into grace");

    // Wait K seconds inside grace, then top up exactly one period of price.
    let k_grace_wait: i64 = 1_000; // arbitrary positive interval inside grace
    let top_up_time = T0 + 2 * plan_period + k_grace_wait;
    clock::set_clock(&mut env.svm, top_up_time);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    env.svm.expire_blockhash();
    let top_up_amount = plan_price * 2; // two periods of margin so the
                                        // post-recovery one-period charge
                                        // below does not re-exhaust at
                                        // the exact period boundary.
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            top_up_amount,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up recovers to Active");

    // Immediate charge — same on-chain clock, no time has elapsed since
    // top_up. With F2 fix: stream_start += grace_duration (=k_grace_wait),
    // so now - new_stream_start ≈ 0 and claimable ≈ 0. Charge must
    // therefore FAIL with InsufficientUnlockedFunds because claimable == 0,
    // since no service-second elapsed post-recovery.
    env.svm.expire_blockhash();
    let r = send_tx(
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
    );
    assert_nakama_err::<()>(r, NakamaError::InsufficientUnlockedFunds);

    // Merchant balance unchanged — F2 fix prevents the drain.
    let post_merchant = token_balance(&env.svm, &actors.merchant_ata);
    assert_eq!(
        post_merchant, pre_merchant,
        "F2: merchant cannot drain top-up immediately after recovery; \
         stream_start shift forces zero-claimable on same-second charge"
    );

    // Cross-property: advance by one period of seconds post-recovery →
    // charge yields exactly one-period worth (price), proving recovery
    // streams ARE resuming, just from the shifted anchor.
    clock::set_clock(&mut env.svm, top_up_time + plan_period);
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
    .expect("post-recovery charge after one period elapses must succeed");

    let final_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let delta = final_merchant - pre_merchant;
    // After F2 shift, unlocked at +period == price (exactly one period).
    // The pre-grace exhaustion already settled 2*price to merchant; after
    // recovery + 1 period, merchant earns +price.
    assert_eq!(
        delta, plan_price,
        "post-recovery one-period charge transfers exactly `price`, \
         confirming stream resumes from shifted anchor"
    );
}

// ─────────────────────────────────────────────────────────────────────
// F4 — rate truncation under-pays merchant
// ─────────────────────────────────────────────────────────────────────

/// ADR-015 §F4. USDC $10/month plan — `price=10_000_000`,
/// `period=2_592_000`. Old math: `rate_per_second = 3` (truncated from
/// 3.858…). At cancel-on-exact-period, settle = `elapsed * rate =
/// 2_592_000 * 3 = 7_776_000`, refund = `10_000_000 - 7_776_000 =
/// 2_224_000` — 22% under-paying the merchant.
///
/// With lazy precise division `unlocked = elapsed * price / period`,
/// settle = `2_592_000 * 10_000_000 / 2_592_000 = 10_000_000` (full
/// price), refund = 0. Test pins this exact result.
#[test]
fn cancel_at_period_boundary_full_price_to_merchant_zero_refund() {
    let plan_price: u64 = 10_000_000; // $10 USDC base units
    let plan_period: i64 = 2_592_000; // 30 days in seconds
    let mut env = setup();
    // Subscribe pre-funds price * periods; we need >= plan_price in ata.
    let actors = fund_actors(&mut env, plan_price);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (_vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0);
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1, // one period prefund = plan_price deposited
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe with $10 monthly plan");

    // Advance exactly one full period.
    clock::set_clock(&mut env.svm, T0 + plan_period);

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
    .expect("cancel at exact period boundary");

    let post_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let post_subscriber = token_balance(&env.svm, &actors.subscriber_ata);

    let merchant_delta = post_merchant - pre_merchant;
    let subscriber_refund = post_subscriber - pre_subscriber;

    // F4 fix: merchant earns exactly `price` (full period), refund is 0.
    assert_eq!(
        merchant_delta,
        plan_price,
        "F4: merchant must receive full `price` on full-period cancel \
         (pre-fix: would receive {} due to rate truncation)",
        2_592_000u64 * 3 // = 7_776_000, the buggy pre-fix value
    );
    assert_eq!(
        subscriber_refund, 0,
        "F4: subscriber refund is exactly 0 on full-period cancel \
         (pre-fix: would receive 2_224_000 = ~22% of plan price)"
    );

    // Cross-property: refund is strictly less than the 0.5 USDC = 500_000
    // base units boundary called out in ADR-015 §"Тесты" — slack-bound
    // formulation for the same property in case future math reorders
    // produce a 1-base-unit residue.
    assert!(
        subscriber_refund < 500_000,
        "F4: refund must be less than 0.5 USDC (acceptance bound from \
         ADR-015 §Тесты); observed {}",
        subscriber_refund
    );
}

/// ADR-015 §F4 Q3 acceptance gate. CU budget for a happy-path charge
/// under the new lazy precise math MUST stay below 100k CU (per ADR-015
/// §"Acceptance criteria for dev subagents"). The ADR predicts +~50 CU
/// over the previous integer-mul math; the upper bound is a wide margin
/// to absorb sBPF version drift.
#[test]
fn f4_charge_cu_budget_under_100k() {
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
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

    clock::set_clock(&mut env.svm, T0 + plan_period);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let metadata = send_tx(
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
    .expect("happy-path charge");

    let cu = metadata.compute_units_consumed;
    // ADR-015 §F4 acceptance bound. Charge happy-path empirically reports
    // ~18k CU on Anchor 1.0.2 / sBPF; 100k is a 5.5× safety margin.
    assert!(
        cu < 100_000,
        "F4 Q3: happy-path charge consumed {} CU, exceeds 100k acceptance \
         bound; escalate [ADR_DRIFT] per ADR-015 §F4",
        cu
    );

    // Print for the cycle report.
    println!("ADR-015 §F4 CU measurement: charge happy path = {} CU", cu);
}

/// ADR-015 §F4 sanity — full lifecycle on a plan where `price % period ==
/// 0` (the existing test-fixture parameters: price=600, period=60). The
/// new math must agree byte-for-byte with the old math when truncation
/// is a no-op. This guards against the F4 refactor accidentally
/// regressing the integer-clean case.
#[test]
fn f4_math_unchanged_when_price_divides_period_exactly() {
    let plan_price: u64 = 600; // exactly 10 per second over 60 seconds
    let plan_period: i64 = 60;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000);
    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            plan_price,
            plan_period,
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

    // Charge half a period; expect price/2 = 300 base units.
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");
    let pre = token_balance(&env.svm, &actors.merchant_ata);
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
    .expect("charge at half period");
    let post = token_balance(&env.svm, &actors.merchant_ata);
    assert_eq!(
        post - pre,
        plan_price / 2,
        "F4: integer-clean case (price%period==0) yields same exact result \
         as pre-fix math"
    );
}
