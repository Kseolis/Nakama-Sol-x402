//! ADR-015 — partial-FSM-implementation pattern
//! (security-audit-patterns.md §P6). The pattern: one handler (`charge`)
//! triggers a state transition at boundary (`withdrawn == deposited` →
//! `GracePeriod`), but `settle_usage` mutates the same trigger field
//! without the analogous transition tail.
//!
//! This is **documented and intentional** on the Subscription FSM side
//! (ADR-x402-001 §"Settle handler" — facilitator-settle is permissionless
//! by-the-second; the FSM transition into Grace is reserved for
//! `charge`-driven exhaustion to preserve the keeper-bot contract).
//!
//! Therefore these tests pin OBSERVABLE STATE under the divergence so that
//! a future refactor that "fixes" P6 by auto-flipping in settle_usage
//! must explicitly update these tests (and ADRs) instead of doing so
//! silently.
//!
//! Acceptance criteria (mapping):
//! * settle_usage drains parent → parent.state stays Active (NOT auto-Grace).
//! * Subsequent charge on the drained Active parent fails with a clear
//!   error (`InsufficientUnlockedFunds` per ADR-004 §3 / §7).
//! * top_up from this Active-but-drained parent works (allowed from Active
//!   per ADR-007), restores deposited > withdrawn, AND a subsequent charge
//!   resumes correctly.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, pay_session_pda, plan_pda, send_tx, setup, subscription_pda, token_program_id,
    vault_pda, Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

fn read_subscription(svm: &litesvm::LiteSVM, sub_pk: &Pubkey) -> nakama::state::Subscription {
    let data = svm.get_account(sub_pk).expect("alive").data;
    nakama::state::Subscription::deserialize(&mut &data[8..]).expect("decode")
}

/// P6 pin. Facilitator settles full escrow via x402; `parent.withdrawn ==
/// parent.deposited`. parent.state MUST stay Active (ADR-x402-001 design
/// intent — Grace is for charge-driven exhaustion only). Subsequent charge
/// returns `InsufficientUnlockedFunds`. Subscriber can top_up from Active.
/// Subsequent charge then succeeds.
///
/// **Note for off-chain consumers**: Active + (withdrawn == deposited) is a
/// logical-exhaustion signal. Computed-status derivation MAY surface this
/// as an "x402-drained" view; the on-chain state byte is the canonical
/// truth. This is intentional, not a bug — ADR-015 §F5 covers the
/// off-chain owner-check; the FSM mirror is in ADR-x402-001.
#[test]
fn settle_usage_drain_leaves_active_state_charge_blocks_then_topup_restores() {
    let plan_price: u64 = 1_200;
    let plan_period: i64 = 60;
    let periods_prefund: u8 = 2;
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    // create_plan + subscribe at T0 with 2 periods.
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
    let (vault_pk, _) = vault_pda(&sub_pk);

    clock::set_clock(&mut env.svm, T0);
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            periods_prefund,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    let deposited = plan_price * periods_prefund as u64; // = 2400

    // Open PaySession with reservation_cap = full deposit.
    let session_id = 1u64;
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            deposited, // full cap
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    // Advance to exact exhaustion in stream-time so settle_usage's
    // `unlocked` formula caps at `deposited`. Facilitator then settles the
    // entire deposit in one call.
    clock::set_clock(&mut env.svm, T0 + periods_prefund as i64 * plan_period);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            deposited,
        )],
        &[&facilitator],
    )
    .expect("settle_usage drains the entire deposit");

    // Pin: state byte is Active, but withdrawn == deposited.
    let post_settle = read_subscription(&env.svm, &sub_pk);
    assert_eq!(
        post_settle.state,
        nakama::state::SubscriptionState::Active,
        "P6 divergence is intentional — settle_usage does NOT flip to Grace"
    );
    assert_eq!(
        post_settle.withdrawn_amount, post_settle.deposited_amount,
        "logical exhaustion via x402 — withdrawn == deposited"
    );
    assert_eq!(post_settle.withdrawn_amount, deposited);

    // Subsequent charge on the drained Active parent. Math claimable = 0
    // (`unlocked` caps at `deposited`, `withdrawn == deposited` ⇒ claimable
    // = 0) → `InsufficientUnlockedFunds`. This is the observable signal
    // that the parent is logically out-of-funds despite state == Active.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop");
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

    // top_up from Active is permitted (ADR-007 §"Per-state eligibility").
    // Add 2 periods of funds so the subsequent half-period charge leaves
    // headroom (avoids immediate re-exhaustion that would need a satellite).
    let topup_amount = 2 * plan_price;
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            topup_amount,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up from Active restores claimable headroom");

    let post_topup = read_subscription(&env.svm, &sub_pk);
    assert_eq!(post_topup.state, nakama::state::SubscriptionState::Active);
    assert_eq!(
        post_topup.deposited_amount,
        deposited + topup_amount,
        "deposited grew by the top_up amount"
    );

    // Advance clock by half a period so unlock grows but doesn't reach
    // the new deposited cap. Headroom remains so charge can fire without
    // a grace satellite.
    clock::set_clock(
        &mut env.svm,
        T0 + periods_prefund as i64 * plan_period + plan_period / 2,
    );
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
    .expect("post-topup charge resumes — divergence is recoverable");

    let post_charge = read_subscription(&env.svm, &sub_pk);
    assert_eq!(post_charge.state, nakama::state::SubscriptionState::Active);
    // sanity: withdrawn advanced past previous mark.
    assert!(post_charge.withdrawn_amount > post_settle.withdrawn_amount);

    // Use pay_session_pda to silence the unused import warning when the
    // session helpers aren't otherwise referenced.
    let _ = pay_session_pda(&sub_pk, session_id);
}
