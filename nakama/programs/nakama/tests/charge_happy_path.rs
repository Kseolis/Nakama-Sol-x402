//! Happy-path tests for `charge` (ADR-004).
//!
//! Black-box: written from ADR-004 §3 streaming math + §5 state update,
//! NOT from `instructions/charge.rs` internals (the handler may not exist
//! on disk yet — these tests will compile once it does).
//!
//! Coverage:
//! - ADR-004 §3 — `claimable = min(deposited, elapsed * rate) - withdrawn`.
//! - ADR-004 §5 — post-state: `withdrawn_amount` advances, `last_charge_at = now`.
//! - ADR-004 §7 row 1 — same-period replay fails (covered in invariants file).
//! - ADR-004 §1 — permissionless signer (any keypair as `payer`).
//!
//! Setup convention (matches `cancel_happy_path.rs`):
//! - plan `price = 600` µUSDC, `period = 60s` ⇒ `rate_per_second = 10`.
//! - prefund 2 periods ⇒ `deposited_amount = 1200`.
//! - clock pinned at `t0 = 1_700_000_000` before `subscribe`.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
const RATE: u64 = 10; // = price / period

/// Common bring-up: create plan + subscribe at `T0` with 2 periods prefund.
fn create_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
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

/// Source: ADR-004 §3 + §5 — at `now == stream_start + period`, exactly one
/// period worth of funds has unlocked. After charge: merchant_ata gained
/// `price`, vault lost `price`, no leftover claimable.
///
/// Permissionless signer (ADR-004 §1): we use a fresh "keeper" keypair that
/// has no relation to subscriber or merchant. This proves the contract
/// trusts math, not the signer.
#[test]
fn charge_at_period_boundary_succeeds() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);
    let pre_vault = token_balance(&env.svm, &vault_pk);
    assert_eq!(pre_vault, PLAN_PRICE * 2, "vault prefund sanity");

    // Permissionless keeper — pays tx fee, never sees user keys.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    // Warp to exact boundary.
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);

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
    .expect("charge at boundary");

    // ADR-004 §3: claimable at t = stream_start + period =
    //   min(1200, 60 * 10) - 0 = 600.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    assert_eq!(
        merchant_delta, PLAN_PRICE,
        "exactly one period's worth (= price) settled"
    );
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        pre_vault - PLAN_PRICE,
        "vault drained by exactly the claimable amount"
    );

    // State byte still Active (= 0). ADR-004 §5 says no FSM transition in
    // MVP `charge` (GracePeriod hook is commented out).
    let sub_acct = env.svm.get_account(&sub_pk).expect("subscription alive");
    assert_eq!(
        sub_acct.data[common::STATE_OFFSET], 0,
        "state must remain Active after a partial-unlock charge"
    );
}

/// Source: ADR-004 §3 — mid-period: at t = T0 + 30, unlocked = 30 * 10 = 300,
/// withdrawn = 0, claimable = 300. After charge: vault = 1200 - 300 = 900.
#[test]
fn charge_mid_period_settles_streaming_math() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    // Warp half a period in.
    clock::set_clock(&mut env.svm, T0 + 30);

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
    .expect("charge mid-period");

    let expected = 30u64 * RATE; // 300
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    assert_eq!(
        merchant_delta, expected,
        "ADR-004 §3: claimable = elapsed*rate when unlocked < deposited"
    );
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        PLAN_PRICE * 2 - expected,
        "vault drained by exactly the claimable amount"
    );
}

/// Source: ADR-004 §5 — `withdrawn_amount` is monotonic. Two charges
/// across a full period each transfer one period's worth; cumulative
/// settled = 2 * price. Also exercises ADR-004 §7 (row 1 absence-of-
/// double-spend through monotonic guard, not period-discrete guard).
#[test]
fn two_consecutive_charges_advance_withdrawn() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let pre_merchant = token_balance(&env.svm, &actors.merchant_ata);

    // First charge at t = T0 + period — claimable = 600.
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
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
    .expect("first charge");

    // Expire blockhash so second tx isn't deduped as AlreadyProcessed.
    env.svm.expire_blockhash();

    // Second charge at t = T0 + 2*period — incremental claimable = 600 again.
    clock::set_clock(&mut env.svm, T0 + 2 * PLAN_PERIOD);
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
    .expect("second charge");

    // Cumulative settle = 2 * price = 1200. Vault drained to 0.
    let merchant_delta = token_balance(&env.svm, &actors.merchant_ata) - pre_merchant;
    assert_eq!(
        merchant_delta,
        2 * PLAN_PRICE,
        "two charges across two periods sum to 2 * price"
    );
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        0,
        "vault fully drained after two full periods"
    );
}
