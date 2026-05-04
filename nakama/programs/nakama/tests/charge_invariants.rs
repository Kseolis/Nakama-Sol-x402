//! Error-path tests for `charge` (ADR-004 §8).
//!
//! Black-box: written from ADR-004 §8 error matrix + §2 precondition ordering,
//! NOT from `instructions/charge.rs`.
//!
//! Coverage:
//! - ADR-004 §2.h + §8 — `IllegalStateForCharge` when `state != Active`.
//!   Synthesised via direct byte write at `STATE_OFFSET = 192` (sign-off
//!   handoff item 2 / BLK-19) — see ADR-003 Q8 / BLK-10 for why the
//!   natural cancel-then-charge flow returns `AccountNotInitialized` instead.
//! - ADR-004 §2.j + §8 — `ClockBackwards` when `now < stream_start`.
//! - ADR-004 §3 + §7 row 3 — `InsufficientUnlockedFunds` when claimable=0.
//! - ADR-004 §7 row 4 / ADR-003 Q8 / BLK-10 — charge-after-cancel surfaces
//!   Anchor `AccountNotInitialized` (3012), not `IllegalStateForCharge`.

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_anchor_err, assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

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

/// Source: ADR-004 §2.h + §8 — `IllegalStateForCharge` when state != Active.
///
/// Synthesised: in MVP cancel fuses cleanup (ADR-003 Q8 / BLK-10), so the
/// natural cancel-then-charge flow returns Anchor `AccountNotInitialized`
/// (covered separately below). To exercise the FSM guard itself we must
/// keep the account alive while flipping its `state` byte. We do that by
/// reading the Subscription account post-subscribe, planting `state = 4`
/// (Cancelled, ADR-003 §enum) at byte offset 192, and writing it back via
/// `LiteSVM::set_account`.
///
/// Post-MVP (split cancel/cleanup): this path becomes naturally reachable
/// and the synthesis can be removed.
#[test]
fn synthesised_cancelled_state_rejected_with_illegal_state() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // Synthesise state=Cancelled (4) by direct byte write at STATE_OFFSET.
    let mut sub_acct = env
        .svm
        .get_account(&sub_pk)
        .expect("subscription alive after subscribe");
    sub_acct.data[common::STATE_OFFSET] = 4; // SubscriptionState::Cancelled
    env.svm
        .set_account(sub_pk, sub_acct)
        .expect("plant Cancelled state byte");

    // Warp to a point where, were state Active, claimable would be > 0.
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let result = send_tx(
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

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCharge);
}

/// Source: ADR-004 §2.j + §8 — `ClockBackwards` when `now < stream_start`.
/// Defence-in-depth against validator clock drift / fork replay.
#[test]
fn clock_before_stream_start_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // stream_start was T0 = 1_700_000_000. Push clock back below it.
    clock::set_clock(&mut env.svm, T0 - 1);

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let result = send_tx(
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

    assert_nakama_err::<()>(result, NakamaError::ClockBackwards);
}

/// Source: ADR-004 §3 + §7 row 3 — `claimable == 0` rejected.
///
/// Charge immediately after subscribe at `now == stream_start` ⇒
/// elapsed = 0 ⇒ unlocked = 0 ⇒ claimable = 0 ⇒ `InsufficientUnlockedFunds`.
#[test]
fn charge_with_zero_elapsed_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // Clock unchanged after subscribe — elapsed = 0.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let result = send_tx(
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

    assert_nakama_err::<()>(result, NakamaError::InsufficientUnlockedFunds);
}

/// Source: ADR-004 §7 row 4 / ADR-003 Q8 / BLK-10 — charge after cancel.
///
/// MVP: fused cancel; the Subscription account is closed at the end of
/// `cancel`, so the next `charge` hits Anchor's `AccountNotInitialized`
/// (3012) BEFORE the handler body — `IllegalStateForCharge` is unreachable
/// through this natural flow (test above synthesises that path).
///
/// Post-MVP (split cancel/cleanup): account survives in `Cancelled` state
/// and this expectation flips to `NakamaError::IllegalStateForCharge`.
/// Re-tighten the assertion then.
#[test]
fn charge_after_cancel_hits_account_not_initialized() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // Cancel succeeds (mid-period to keep math non-trivial; not strictly required).
    clock::set_clock(&mut env.svm, T0 + 30);
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

    // Expire blockhash so the next tx isn't deduped as AlreadyProcessed.
    env.svm.expire_blockhash();

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    // Warp further so, were sub alive, claimable would be > 0.
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);

    let result = send_tx(
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

    assert_anchor_err(result, anchor_codes::ACCOUNT_NOT_INITIALIZED);
}
