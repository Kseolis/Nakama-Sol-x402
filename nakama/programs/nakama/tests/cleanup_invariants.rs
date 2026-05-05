//! Error-path tests for `cleanup` (ADR-013 cycle-3).
//!
//! Coverage:
//! - ADR-013 invariant 1, 10: state guard — cleanup only from {Cancelled,
//!   Exhausted}. Active rejected with `IllegalStateForCleanup`. The exhaustive
//!   `matches!` pattern in the handler ensures Paused/GracePeriod (ADR-006/007
//!   territory) also fall into the reject path when those become reachable.
//! - ADR-013 invariant 2 / Q1: signer guard — cleanup signed by non-subscriber
//!   rejected. Either `UnauthorizedCleanup` (custom) or `ConstraintHasOne`
//!   (Anchor 2001) is acceptable; the handoff explicitly accepts both.
//! - ADR-013 invariant 7 / Q7: re-subscribe before cleanup → System Program
//!   `AccountAlreadyInUse` (`Custom(0)`). Pinned via dedicated helper, NOT
//!   `assert_any_err`, because the SDK depends on this exact surface error
//!   for "click cleanup first" UX.
//!
//! Forward-deferred (ADR-013 §Tests §"forward-defer note for cleanup_after_
//! exhausted.rs"): cleanup from Paused/GracePeriod/Exhausted state — not
//! reachable in cycle-3 (ADR-006/007 not landed; no instruction writes those
//! state bytes). Will be added to this file in those cycles.

mod common;

use common::{
    clock,
    error::{
        assert_nakama_err, assert_system_account_already_in_use, extract_custom_code, NakamaError,
        ERROR_CODE_OFFSET,
    },
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

fn create_plan_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
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

    (plan_pk, sub_pk)
}

/// Source: ADR-013 invariant 1 / §"Per-state cleanup eligibility" — Active
/// must reject cleanup with `IllegalStateForCleanup`. Caller is required to
/// `cancel` first (fair settle + refund) — closes the rage-cleanup vector
/// where a subscriber could reclaim rent without paying the merchant for
/// already-streamed time.
///
/// This also covers ADR-013 invariant 10 (forward-compat exhaustive
/// `matches!`): Active is one of three variants (Active/Paused/GracePeriod)
/// that fall into the reject path. Paused and GracePeriod are not yet
/// writable on-chain (ADR-006/007 not landed), so Active is the only
/// reachable representative of the "must cancel first" bucket in cycle-3.
#[test]
fn cleanup_from_active_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCleanup);
}

/// Source: ADR-013 invariant 2 / Q1 — cleanup signed by an attacker who is
/// not the snapshotted `subscription.subscriber` must be rejected.
///
/// Acceptable error codes (handoff-mandated):
/// - `NakamaError::UnauthorizedCleanup` (custom, code 6015) if `has_one`
///   handler-level constraint fires with the explicit `@` error,
/// - or Anchor `ConstraintHasOne` (2001) if the declarative path fires
///   first.
///
/// Both are correct outcomes; we use a manual matcher rather than the
/// `assert_any_err` shotgun so the test still pins to one of two known-good
/// codes (any other code is a regression).
#[test]
fn cleanup_unauthorized_signer_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    // First cancel the subscription so state is Cancelled — this isolates the
    // signer guard from the state guard. Otherwise IllegalStateForCleanup
    // would fire first and we wouldn't be testing what we claim to test.
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

    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &attacker,
        &[ix::cleanup_ix_with_signer(
            &actors.subscriber.pubkey(), // the snapshotted subscriber
            &sub_pk,
            &attacker.pubkey(), // wrong signer goes into AccountMeta
        )],
        &[&attacker],
    );

    let meta = match result {
        Ok(_) => panic!("expected cleanup to fail with unauthorized signer, but tx succeeded"),
        Err(m) => m,
    };
    let code = extract_custom_code(&meta).unwrap_or_else(|| {
        panic!(
            "expected Custom(UnauthorizedCleanup or ConstraintHasOne), got non-Custom: {:?}",
            meta.err
        )
    });
    let unauthorized_cleanup = ERROR_CODE_OFFSET + (NakamaError::UnauthorizedCleanup as u32);
    let constraint_has_one = common::error::anchor_codes::CONSTRAINT_HAS_ONE;
    assert!(
        code == unauthorized_cleanup || code == constraint_has_one,
        "expected UnauthorizedCleanup ({}) or ConstraintHasOne ({}), got {} — {:?}",
        unauthorized_cleanup,
        constraint_has_one,
        code,
        meta.err
    );
}

/// Source: ADR-013 invariant 7 / Q7 — re-subscribe with the same
/// `(subscriber, plan)` seeds against an alive Cancelled tombstone fails with
/// System Program `AccountAlreadyInUse` (`Custom(0)`). This pins the surface
/// error so SDK / UI can show "click cleanup first" UX (ADR-008 critical-path).
///
/// Mechanism: Anchor's `init` constraint on the Subscription account calls
/// `system_instruction::create_account` via CPI; the System Program returns
/// `SystemError::AccountAlreadyInUse` when the target address already holds
/// a non-zero-data account, which surfaces on the wire as
/// `InstructionError::Custom(0)`. This is NOT Anchor's
/// `AccountAlreadyExists` (3014) — that fires only when an Anchor-owned
/// account (with discriminator) is found pre-init by Anchor's own check.
/// The pre-Anchor System Program check beats it to the punch.
///
/// Empirical reference: identical mechanism is observed in
/// `subscribe_invariants.rs::subscribe_with_subscriber_ata_equal_to_vault_rejected`.
#[test]
fn subscribe_before_cleanup_fails_account_already_in_use() {
    let mut env = setup();
    // Two periods of prefund × 2 subscribe attempts = 4 × price USDC needed.
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) = create_plan_and_subscribe(&mut env, &actors);

    // Cancel → Subscription becomes alive Cancelled tombstone.
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

    // Sanity: tombstone exists.
    assert!(
        env.svm.get_account(&sub_pk).is_some(),
        "tombstone must persist before resubscribe attempt"
    );

    // Attempt resubscribe with the same seeds — without cleanup in between.
    env.svm.expire_blockhash();
    let result = send_tx(
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

    assert_system_account_already_in_use(result);
}
