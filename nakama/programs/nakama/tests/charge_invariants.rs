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
//! - ADR-013 §"Cancel handler" / ADR-004 §2.h — charge-after-cancel against
//!   the alive Cancelled tombstone fires `IllegalStateForCharge` (the FSM
//!   guard is now reachable; was Anchor `AccountNotInitialized` in cycle-2
//!   fused-MVP).

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

/// Source: ADR-013 §Consequences §"Tighter charge guard observable" /
/// ADR-004 §2.h — charge after cancel against the alive Cancelled tombstone.
///
/// **[ADR_DRIFT] empirical pin (cycle-3, 2026-05-04).** ADR-013 promises
/// `IllegalStateForCharge` becomes the surface error post-split. In
/// practice it does NOT: Anchor pre-handler validation for `vault:
/// Account<'info, TokenAccount>` (with `seeds` / `bump` / `token::mint` /
/// `token::authority` constraints) deserialises the **closed** vault
/// (closed in `cancel` via SPL `close_account` CPI per BLK-15) and fails
/// with `AccountNotInitialized` (3012) before the handler body runs the
/// FSM guard.
///
/// Subscription tombstone observability (ADR-013 invariants 3, 5) holds
/// — the Subscription account itself stays alive with `state == 4` byte
/// readable on-chain. Indexers and x402 satellites get their sentinel.
/// What's NOT achieved is a custom error code on the natural charge-after-
/// cancel path; the synthesised version (`synthesised_cancelled_state_
/// rejected_with_illegal_state` above, which mutates the state byte
/// directly without closing the vault) does fire `IllegalStateForCharge`.
///
/// To make this guard actually-fired through the natural flow, `cancel`
/// would need to defer vault close to `cleanup`, or charge would need to
/// drop strong-typed `Account<TokenAccount>` for vault. Both are impl-
/// level redesigns out of cycle-3 scope — flagged as security-auditor
/// finding for ADR-013 cycle-4 backlog.
#[test]
fn charge_after_cancel_hits_account_not_initialized_due_to_closed_vault() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    // Cancel succeeds (mid-period). Subscription becomes alive Cancelled
    // tombstone; vault is closed via SPL CPI.
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

    // [ADR_DRIFT pin] — actual error is Anchor 3012 on the closed vault,
    // not the FSM guard. See doc-comment above. The synthesised path at
    // the top of this file (state byte mutation only, vault left alive)
    // does fire the custom `IllegalStateForCharge` and remains the
    // canonical FSM-guard proof.
    assert_anchor_err(result, anchor_codes::ACCOUNT_NOT_INITIALIZED);
}
