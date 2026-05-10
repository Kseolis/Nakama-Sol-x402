//! Stage 3 gap-fill — x402 boundary contract: parent state guards.
//!
//! Source ADRs:
//! - ADR-x402-001 §"open_session" — `parent.state == Active` guard
//! - ADR-x402-001 §"settle_usage" — `parent.state == Active` guard
//! - ADR-006 §"Pause" — Subscription.state = Paused
//! - ADR-007 §"GracePeriod entry" — boundary contract with x402
//! - ADR-013 §"Cancel handler" — Subscription tombstone in state=Cancelled
//!
//! Gap report items: G1 (P0), G2 (P1), G3 (P1), G4 (P1), G5 (P2). See
//! `docs/qa/test-coverage-gaps.md`.
//!
//! These tests prove that x402 lifecycle instructions correctly REJECT calls
//! when the parent Subscription is not in `Active` state. Without this
//! enforcement, a facilitator could drain (or attempt to drain) escrow after
//! the subscription has been cancelled, paused, or exhausted into GracePeriod.

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_anchor_err, assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_program_id, vault_pda,
    Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;

/// Plan: price=1200 µUSDC over 120s ⇒ rate=10/s. Subscribe with 1 period
/// prefund (deposited=1200, vault=1200).
///
/// Returns (plan_pk, sub_pk).
fn setup_active_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    periods: u8,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            1200,
            120,
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
            periods,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    (plan_pk, sub_pk)
}

/// Cancel the active subscription. Post-state: Subscription tombstone alive,
/// state byte = 4 (Cancelled), vault closed.
fn cancel_to_tombstone(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    sub_pk: &solana_pubkey::Pubkey,
) {
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    )
    .expect("cancel parent");
}

/// Pause the active subscription. Post-state: state byte = 1 (Paused),
/// PausedSubscription satellite created.
fn pause_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    sub_pk: &solana_pubkey::Pubkey,
) {
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause parent");
}

// =========================================================================
// Gap G1 [P0] — settle_usage rejected after parent Cancelled
// =========================================================================

/// Source: ADR-x402-001 §"settle_usage" + ADR-013 R1.
///
/// Sequence:
///  1. subscribe → Active
///  2. open_session(reservation_cap=500) → PaySession Open
///  3. cancel(subscriber) → parent state=Cancelled, vault closed
///  4. settle_usage(amount=10) → MUST fail
///
/// **Why critical (P0):** A facilitator that retains an open PaySession
/// after the subscription has been cancelled must NOT be able to call
/// `settle_usage` and move funds. ADR-013 R1 closes the vault on cancel.
///
/// **Surface error.** Because Anchor account validation runs BEFORE the
/// handler `parent.state` guard, the closed `vault` account triggers
/// Anchor's `AccountNotInitialized` (3012) FIRST. The defense-in-depth
/// `ParentNotActive` guard never fires here — the vault closure is the
/// hard stop. Asserting 3012 documents the actual ordering so a future
/// refactor that re-opens the vault (e.g. lazy close) would re-expose the
/// `parent.state` guard as the first failure and force this test to be
/// updated in tandem. Same surface as
/// `charge_after_cancel_hits_account_not_initialized_due_to_closed_vault`.
#[test]
fn settle_usage_after_cancel_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 1);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    // Open a PaySession while parent is still Active.
    let session_id = 7u64;
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session before cancel");

    // Cancel parent → tombstone (state=Cancelled).
    cancel_to_tombstone(&mut env, &actors, &sub_pk);

    // Sanity: state byte at offset 192 == 4 (Cancelled).
    let state_byte = env
        .svm
        .get_account(&sub_pk)
        .expect("tombstone alive")
        .data[STATE_OFFSET];
    assert_eq!(state_byte, 4, "parent must be in Cancelled state");

    // Attempt settle_usage on the tombstoned parent.
    let (vault_pk, _) = vault_pda(&sub_pk);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            10,
        )],
        &[&facilitator],
    );

    // Vault closed on cancel (BLK-15) — Anchor account validation surfaces
    // 3012 before our state guard fires.
    assert_anchor_err(result, anchor_codes::ACCOUNT_NOT_INITIALIZED);
}

// =========================================================================
// Gap G2 [P1] — settle_usage rejected when parent in GracePeriod
// =========================================================================

/// Source: ADR-007 §"Boundary contract x402" + integration scenario 6.4.
///
/// Sequence:
///  1. subscribe(periods=1, deposited=1200, rate=10/s)
///  2. open_session(reservation_cap=500)
///  3. warp(+120s) → charge() drains vault → state=GracePeriod
///  4. settle_usage(amount=1) → MUST fail `ParentNotActive`
///
/// **Why critical (P1):** GracePeriod is the explicit ADR-007 contract with
/// the x402 layer — once the parent enters GracePeriod, all x402 settlement
/// is suspended until `top_up` recovers it to Active. Without this guard,
/// a facilitator could double-spend against an exhausted parent.
#[test]
fn settle_usage_in_grace_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 1);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let session_id = 9u64;
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    // Warp past one full period → unlocked == deposited == 1200.
    // Charge will drain vault and flip state to GracePeriod (ADR-007 §I-CHARGE-1).
    clock::set_clock(&mut env.svm, T0 + 120);
    env.svm.expire_blockhash();

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), 1);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = common::grace_pda(&sub_pk);

    // Use the third-party keeper as charge signer (permissionless — ADR-004 §1).
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
            &token_program_id(),
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("charge tail enters grace");

    // Confirm parent is in GracePeriod (state byte = 2).
    let state_byte = env
        .svm
        .get_account(&sub_pk)
        .expect("subscription alive")
        .data[STATE_OFFSET];
    assert_eq!(state_byte, 2, "parent must be in GracePeriod state");

    // Attempt settle on graced parent.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            1,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::ParentNotActive);
}

// =========================================================================
// Gap G3 [P1] — settle_usage rejected when parent Paused
// =========================================================================

/// Source: ADR-006 + ADR-x402-001 cross-layer guard.
///
/// Sequence:
///  1. subscribe → Active
///  2. open_session
///  3. pause(merchant) → parent state=Paused
///  4. settle_usage(amount=10) → MUST fail `ParentNotActive`
#[test]
fn settle_usage_when_parent_paused_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop");

    let session_id = 13u64;
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    pause_subscription(&mut env, &actors, &sub_pk);

    let state_byte = env.svm.get_account(&sub_pk).expect("alive").data[STATE_OFFSET];
    assert_eq!(state_byte, 1, "parent must be Paused (state=1)");

    let (vault_pk, _) = vault_pda(&sub_pk);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &actors.merchant_ata,
            &token_program_id(),
            10,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::ParentNotActive);
}

// =========================================================================
// Gap G4 [P1] — open_session rejected after parent Cancelled
// =========================================================================

/// Source: ADR-x402-001 §"open_session" `parent.state == Active` guard.
///
/// Module header `x402_open_close_invariants.rs` claims this coverage but
/// no executable test asserts the rejection. This test fills that hole.
///
/// Sequence:
///  1. subscribe → Active
///  2. cancel → parent state=Cancelled
///  3. open_session(new session_id) → MUST fail `ParentNotActive`
#[test]
fn open_session_after_cancel_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 1);

    cancel_to_tombstone(&mut env, &actors, &sub_pk);

    let facilitator = solana_keypair::Keypair::new();
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            42, // fresh session_id
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::ParentNotActive);
}

// =========================================================================
// Gap G5 [P2] — open_session rejected when parent Paused
// =========================================================================

/// Source: ADR-x402-001 + ADR-006 cross-layer.
///
/// Pause is a temporary, merchant-controlled state. While paused, no new
/// PaySession can be opened — facilitator must wait for resume.
#[test]
fn open_session_when_parent_paused_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors, 2);

    pause_subscription(&mut env, &actors, &sub_pk);

    let facilitator = solana_keypair::Keypair::new();
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            55,
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::ParentNotActive);
}
