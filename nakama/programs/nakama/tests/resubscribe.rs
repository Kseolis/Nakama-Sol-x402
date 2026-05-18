//! ADR-008 — Re-subscribe Pattern (composite cleanup+subscribe transaction).
//!
//! Scope: SDK-only ADR with **zero on-chain code change**. The tests in this
//! file therefore exercise the **Solana runtime atomicity primitive** —
//! `cleanup` and `subscribe` packed into a single transaction must commit
//! atomically (both or neither). The on-chain primitives are inherited
//! verbatim from ADR-013 (`cleanup` handler) and the existing MVP `subscribe`.
//!
//! Coverage matrix:
//!
//! | ADR section | Invariant | Test |
//! |---|---|---|
//! | §Decision step 2 | composite `[cleanup, subscribe]` atomic → fresh Active sub at same PDA | `composite_cleanup_subscribe_atomic` |
//! | §E1 / Q5 | sub-ix failure inside composite reverts cleanup → tombstone preserved | `composite_subscribe_failure_reverts_atomic` |
//! | §E3 / Q8 | different plan: composite `[cleanup_A, subscribe_B]` works, A closed, B fresh | `composite_resubscribe_different_plan` |
//! | §Q11 / E5 | fresh subscribe (no tombstone) works as single-ix tx | `fresh_subscribe_single_ix_works` |
//! | §Q12 / ADR-009 | after merchant-cancel, subscriber composite `[cleanup, subscribe]` works | `composite_after_merchant_cancel` |
//! | §"x402 forward-compat" / ADR-x402-001 §"R1 closure" | composite `[close_session, cleanup, subscribe]` atomic | `composite_with_orphan_pay_session` |
//! | §Adversarial (ADR-013 inv 1, ADR-006, ADR-007) | composite vs Active/Paused/Grace fails IllegalStateForCleanup | `composite_against_{active,paused,grace}_state_fails` |
//!
//! Black-box methodology (per agent rules):
//! - Composite transactions are assembled by passing an `&[Instruction]` slice
//!   to `send_tx`. The builders are the existing `ix::*` helpers — we do NOT
//!   reach into `programs/nakama/src/`.
//! - Note: SDK-side composite-tx **helpers** (`crates/nakama-client/src/resubscribe.rs`,
//!   `clients/ts/src/instructions/resubscribe.ts`) are tested in their own crates
//!   (cargo unit tests in nakama-client, mocha in clients/ts). This file proves
//!   the **on-chain semantics** that those SDK helpers rely on.
//! - State byte at `STATE_OFFSET = 192` is the only on-chain structure
//!   inspected directly; everything else is via public ABI.

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, paused_sub_pda, pay_session_pda, plan_pda, send_tx, setup, subscription_pda,
    token_balance, vault_pda, Signer, STATE_OFFSET,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
const PLAN_ID: u64 = 1;

/// Drive a fresh test environment to an Active subscription on `(subscriber, plan_id=1)`.
/// Returns `(plan_pk, sub_pk)`.
fn create_plan_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    plan_id: u64,
    price: u64,
    period: i64,
    periods_to_prefund: u8,
) -> (Pubkey, Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            price,
            period,
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
            periods_to_prefund,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    (plan_pk, sub_pk)
}

/// Drive subscribe → cancel → tombstone. Returns `(plan_pk, sub_pk)`.
fn drive_to_cancelled_tombstone(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (Pubkey, Pubkey) {
    let (plan_pk, sub_pk) =
        create_plan_and_subscribe(env, actors, PLAN_ID, PLAN_PRICE, PLAN_PERIOD, 2);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
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

    // Sanity — tombstone in Cancelled state, state byte == 4.
    let post = env
        .svm
        .get_account(&sub_pk)
        .expect("ADR-013 tombstone must be alive after cancel");
    assert_eq!(
        post.data[STATE_OFFSET], 4,
        "Cancelled state byte (=4) is the precondition for ADR-008 re-subscribe"
    );

    (plan_pk, sub_pk)
}

/// ADR-008 §Decision step 2 + §Atomicity model.
///
/// Composite `[cleanup, subscribe]` packed into a single tx; subscriber signs
/// once. After commit:
/// - The same Subscription PDA holds a **fresh** account (state == Active = 0,
///   vault re-initialised with prefund amount).
/// - PDA address is unchanged (ADR-001 invariant — seeds `[b"sub", subscriber,
///   plan]` produce the same address every cycle).
///
/// This is the canonical happy path (E4).
#[test]
fn composite_cleanup_subscribe_atomic() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) = drive_to_cancelled_tombstone(&mut env, &actors);
    let (vault_pk, _) = vault_pda(&sub_pk);

    // Pre-condition: vault closed by cancel (BLK-15 SPL close_account CPI).
    match env.svm.get_account(&vault_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "vault must be closed pre-composite"),
    }

    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);

    // Advance clock past the tombstone window so the fresh subscription's
    // stream_start anchors at a new point.
    clock::set_clock(&mut env.svm, T0 + 1_000);
    env.svm.expire_blockhash();

    // ── Composite tx: cleanup then subscribe, one signature ──
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1, // 1 period prefund, distinct from the original 2 to prove fresh state
        ),
    ];

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("composite [cleanup, subscribe] must succeed atomically");

    // ── Post-condition: fresh Active subscription at the SAME PDA ──
    let fresh = env
        .svm
        .get_account(&sub_pk)
        .expect("Subscription PDA must hold a fresh account after composite tx");
    assert_eq!(
        fresh.data[STATE_OFFSET], 0,
        "fresh subscription must have state == Active (=0) at offset 192"
    );

    // ── Vault re-initialised with the new prefund (1 × PLAN_PRICE) ──
    let post_vault_usdc = token_balance(&env.svm, &vault_pk);
    assert_eq!(
        post_vault_usdc, PLAN_PRICE,
        "fresh vault must hold exactly 1 period of prefund (PLAN_PRICE)"
    );

    // ── Subscriber's USDC ATA decreased by the prefund amount (besides
    //    whatever the prior subscribe/cancel cycle left). ──
    let post_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    assert_eq!(
        pre_subscriber_usdc - post_subscriber_usdc,
        PLAN_PRICE,
        "subscriber must pay exactly 1 × PLAN_PRICE for the fresh subscribe"
    );
}

/// ADR-008 §E1 / Q5 — failure semantics.
///
/// Compose `[cleanup, subscribe]` where the subscribe ix is **guaranteed to
/// fail** by passing `periods_to_prefund = 0` (triggers `ZeroPeriodsToFund`,
/// `NakamaError` variant index 2 / code 6002).
///
/// Solana runtime guarantees the whole tx reverts:
/// - cleanup's effect (closing the Subscription account) is rolled back;
/// - the Cancelled tombstone is still alive on chain, state == 4;
/// - rent has not moved.
///
/// This proves the all-or-nothing property the SDK and frontend rely on.
#[test]
fn composite_subscribe_failure_reverts_atomic() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) = drive_to_cancelled_tombstone(&mut env, &actors);

    let pre_tombstone = env
        .svm
        .get_account(&sub_pk)
        .expect("tombstone exists pre-composite");
    let pre_tombstone_lamports = pre_tombstone.lamports;
    let pre_state_byte = pre_tombstone.data[STATE_OFFSET];

    let pre_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;

    clock::set_clock(&mut env.svm, T0 + 1_000);
    env.svm.expire_blockhash();

    // Composite where subscribe will fail with ZeroPeriodsToFund.
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            0, // triggers NakamaError::ZeroPeriodsToFund (index 2, code 6002)
        ),
    ];

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    );
    // Specific error pin — Solana tx error wraps the failing ix's NakamaError.
    assert_nakama_err::<()>(result, NakamaError::ZeroPeriodsToFund);

    // ── Atomicity check 1: Subscription tombstone preserved verbatim ──
    let post_tombstone = env
        .svm
        .get_account(&sub_pk)
        .expect("tombstone MUST remain alive — cleanup reverted with subscribe");
    assert_eq!(
        post_tombstone.data[STATE_OFFSET], pre_state_byte,
        "state byte must remain Cancelled (=4) after reverted composite"
    );
    assert_eq!(
        post_tombstone.lamports, pre_tombstone_lamports,
        "tombstone lamports unchanged — cleanup rolled back"
    );

    // ── Atomicity check 2: subscriber lamports unchanged modulo tx fees ──
    // (Solana charges fees even on failed tx; we just bound them sensibly.)
    let post_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    let fee_ceiling: u64 = 50_000;
    assert!(
        pre_subscriber_lamports.saturating_sub(post_subscriber_lamports) <= fee_ceiling,
        "subscriber lamport delta {} must be within fee envelope ({}); rent NOT returned because cleanup reverted",
        pre_subscriber_lamports as i128 - post_subscriber_lamports as i128,
        fee_ceiling
    );
}

/// ADR-008 §E3 / Q8 — plan change between cancel and resubscribe.
///
/// Subscribe to plan_a → cancel → in **one** composite tx perform
/// `[cleanup(plan_a_tombstone), subscribe(plan_b)]`. Result:
/// - plan_a's Subscription PDA closed (rent → subscriber).
/// - plan_b's Subscription PDA initialised fresh (different PDA address —
///   seeds include plan).
///
/// Demonstrates the SDK bundling pattern documented in §E3 ("supported by the
/// pattern but not built into the SDK helper"); the runtime accommodates it
/// without special handling.
#[test]
fn composite_resubscribe_different_plan() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_a_pk, sub_a_pk) = drive_to_cancelled_tombstone(&mut env, &actors);

    // Create plan_b under the SAME merchant (different plan_id → different PDA).
    let plan_b_id = 2u64;
    let plan_b_price = 500u64;
    let plan_b_period = 120i64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_b_id,
            plan_b_price,
            plan_b_period,
        )],
        &[&actors.merchant],
    )
    .expect("create plan_b");

    let (plan_b_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_b_id);
    let (sub_b_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_b_pk);
    assert_ne!(
        sub_a_pk, sub_b_pk,
        "different plan ⇒ different Subscription PDA (ADR-001 seeds invariant)"
    );

    // plan_b Subscription must NOT exist yet.
    assert!(
        env.svm.get_account(&sub_b_pk).is_none(),
        "plan_b Subscription must be uninitialised pre-composite"
    );

    clock::set_clock(&mut env.svm, T0 + 500);
    env.svm.expire_blockhash();

    // Composite: cleanup plan_a tombstone + subscribe to plan_b.
    // SDK could just as well submit them in separate tx; bundling proves the
    // pattern generalises (§E3 "bundling is allowed but not required").
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_a_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_b_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("composite [cleanup A, subscribe B] must succeed");

    // ── plan_a tombstone closed by cleanup ──
    match env.svm.get_account(&sub_a_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "plan_a Subscription must be closed"),
    }

    // ── plan_b Subscription fresh-Active ──
    let fresh_b = env
        .svm
        .get_account(&sub_b_pk)
        .expect("plan_b Subscription must be initialised by the composite tx");
    assert_eq!(
        fresh_b.data[STATE_OFFSET], 0,
        "plan_b state byte must be Active (=0)"
    );

    // ── plan_b vault holds the new prefund ──
    let (vault_b_pk, _) = vault_pda(&sub_b_pk);
    assert_eq!(
        token_balance(&env.svm, &vault_b_pk),
        plan_b_price,
        "plan_b vault must hold 1 × plan_b_price"
    );
}

/// ADR-008 §Q11 / §E5 — fresh-subscribe path (no prior tombstone).
///
/// The composite-tx pattern degrades gracefully: when no Subscription exists
/// for `(subscriber, plan)`, the SDK helper returns a tx with **only** the
/// `subscribe` instruction. The cleanup branch is skipped because
/// `fetchNullable` returns null.
///
/// This test asserts the on-chain side of the contract — a single-ix tx
/// containing only `subscribe` succeeds when the PDA is uninitialised. The
/// SDK builder is responsible for choosing the right shape; cargo unit tests
/// in `crates/nakama-client` (Stage 2) pin the shape.
#[test]
fn fresh_subscribe_single_ix_works() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    // Plan exists but no Subscription yet.
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            PLAN_ID,
            PLAN_PRICE,
            PLAN_PERIOD,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), PLAN_ID);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);

    assert!(
        env.svm.get_account(&sub_pk).is_none(),
        "precondition: no prior Subscription"
    );

    clock::set_clock(&mut env.svm, T0);
    // Composite-of-length-1 — what the SDK helper produces when no tombstone exists.
    let composite = [ix::subscribe_ix(
        &actors.subscriber.pubkey(),
        &plan_pk,
        &actors.subscriber_ata,
        2,
    )];

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("single-ix subscribe must succeed when no tombstone exists");

    let fresh = env
        .svm
        .get_account(&sub_pk)
        .expect("fresh Subscription must be initialised");
    assert_eq!(
        fresh.data[STATE_OFFSET], 0,
        "state byte must be Active (=0) for fresh subscribe"
    );
}

/// ADR-008 §Q12 + ADR-009 interaction.
///
/// Merchant cancels via ADR-009 polymorphic signer; subscriber subsequently
/// submits the composite `[cleanup, subscribe]` themselves. Two invariants
/// proven:
/// 1. `cleanup` is subscriber-only regardless of who initiated the cancel
///    (ADR-013 §Q1 has_one=subscriber).
/// 2. The composite tx works identically after merchant-cancel and
///    subscriber-cancel — no SDK divergence per cancel actor.
#[test]
fn composite_after_merchant_cancel() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let (plan_pk, sub_pk) =
        create_plan_and_subscribe(&mut env, &actors, PLAN_ID, PLAN_PRICE, PLAN_PERIOD, 2);

    // ── Step 1: merchant cancels (ADR-009 path) ──
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::cancel_ix_by_merchant(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.merchant],
    )
    .expect("merchant-cancel must succeed (ADR-009)");

    let tombstone = env
        .svm
        .get_account(&sub_pk)
        .expect("tombstone must persist post merchant-cancel (ADR-013 §Q1)");
    assert_eq!(
        tombstone.data[STATE_OFFSET], 4,
        "state byte must be Cancelled (=4) after merchant-cancel"
    );

    // ── Step 2: subscriber-signed composite re-subscribe ──
    clock::set_clock(&mut env.svm, T0 + 1_000);
    env.svm.expire_blockhash();

    let composite = [
        // Subscriber signs cleanup (ADR-013 Q1 invariant: cleanup is subscriber-only).
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        // Subscriber signs subscribe (existing MVP invariant).
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("composite re-subscribe after merchant-cancel must succeed");

    // ── Step 3: fresh Active sub at same PDA ──
    let fresh = env
        .svm
        .get_account(&sub_pk)
        .expect("Subscription PDA must hold fresh account");
    assert_eq!(
        fresh.data[STATE_OFFSET], 0,
        "fresh subscription state must be Active (=0)"
    );
}

/// ADR-008 §"x402 forward-compat" + ADR-x402-001 §"R1 closure".
///
/// Subscribe → open_session → cancel (PaySession not closed by cancel —
/// verified `cancel.rs` only closes Pause/Grace satellites) → subscriber
/// builds composite `[close_session, cleanup, subscribe]` and submits it in
/// one transaction.
///
/// Asserts the full 3-instruction composite works within the runtime envelope
/// (CU + tx size) and clears the orphan PaySession in the same tx that re-
/// subscribes. Mirrors the SDK builder option `closeAlivePaySessions: true`.
#[test]
fn composite_with_orphan_pay_session() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) =
        create_plan_and_subscribe(&mut env, &actors, PLAN_ID, PLAN_PRICE, PLAN_PERIOD, 2);

    // ── Step 1: open a PaySession on the Active subscription ──
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");
    let session_id: u64 = 0xC0FFEE_u64;
    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            100,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session must succeed");

    assert!(
        env.svm.get_account(&pay_sess_pk).is_some(),
        "PaySession must be alive after open"
    );

    // ── Step 2: subscriber cancels the Subscription ──
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
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
    .expect("cancel must succeed");

    // ADR-008 §E5 invariant: PaySession orphan'd — cancel does NOT close it.
    let orphan = env
        .svm
        .get_account(&pay_sess_pk)
        .expect("PaySession must remain alive (orphan) after cancel — cancel.rs does not close it");
    assert!(orphan.lamports > 0, "orphan PaySession must hold rent");

    // ── Step 3: composite [close_session, cleanup, subscribe] in one tx ──
    clock::set_clock(&mut env.svm, T0 + 1_000);
    env.svm.expire_blockhash();

    let composite = [
        ix::close_session_ix(&actors.subscriber.pubkey(), &sub_pk, session_id),
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("3-ix composite [close_session, cleanup, subscribe] must succeed atomically");

    // ── Post-condition 1: PaySession closed ──
    match env.svm.get_account(&pay_sess_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0, "PaySession must be closed by close_session"),
    }

    // ── Post-condition 2: fresh Active Subscription ──
    let fresh = env
        .svm
        .get_account(&sub_pk)
        .expect("Subscription must hold fresh account after composite");
    assert_eq!(
        fresh.data[STATE_OFFSET], 0,
        "fresh subscription state must be Active (=0) after 3-ix composite"
    );
}

// ── Adversarial: composite against non-Cancelled parent state ──────────────
//
// ADR-008 implicitly relies on cleanup's state guard (ADR-013 §"Per-state
// cleanup eligibility"): cleanup is only legal from {Cancelled, Exhausted}.
// A naive SDK / third-party integrator that composes `[cleanup, subscribe]`
// against a non-cancelled subscription must fail loudly with
// IllegalStateForCleanup — the entire composite tx reverts atomically.

/// ADR-013 §"Per-state cleanup eligibility" / ADR-008 §Adversarial — composite
/// against an Active Subscription must fail with `IllegalStateForCleanup`,
/// and the Subscription remains intact.
#[test]
fn composite_against_active_state_fails() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) =
        create_plan_and_subscribe(&mut env, &actors, PLAN_ID, PLAN_PRICE, PLAN_PERIOD, 2);

    // Precondition: state == Active.
    let pre = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(pre.data[STATE_OFFSET], 0, "must start Active");

    env.svm.expire_blockhash();
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    );
    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCleanup);

    // Subscription is intact, still Active.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 0,
        "state byte must remain Active after rejected composite"
    );
}

/// ADR-006 + ADR-008 §Adversarial — composite against a Paused Subscription
/// must fail with `IllegalStateForCleanup`.
#[test]
fn composite_against_paused_state_fails() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_pk, sub_pk) =
        create_plan_and_subscribe(&mut env, &actors, PLAN_ID, PLAN_PRICE, PLAN_PERIOD, 2);

    // Drive to Paused via merchant pause (ADR-006).
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause must succeed");

    // Sanity: state byte == 1 (Paused).
    let pre = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(pre.data[STATE_OFFSET], 1, "must be Paused");

    // Sanity: Pause satellite alive — confirms we're really in Paused state, not
    // some edge case. Anchor's close = subscriber on Pause satellite is only
    // exercised by cancel / resume, not by this composite.
    let (paused_pda, _) = paused_sub_pda(&sub_pk);
    assert!(
        env.svm.get_account(&paused_pda).is_some(),
        "Pause satellite must be alive"
    );

    env.svm.expire_blockhash();
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    );
    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCleanup);

    // Paused state preserved.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 1,
        "state byte must remain Paused (=1) after rejected composite"
    );
}

/// ADR-007 + ADR-008 §Adversarial — composite against a GracePeriod
/// Subscription must fail with `IllegalStateForCleanup`. GracePeriod is
/// reached by exhausting the stream via charge tail (ADR-007 §I-CHARGE-1).
#[test]
fn composite_against_grace_state_fails() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    // Plan: 1 period prefund — exhausts within `period` seconds.
    let plan_id = 1u64;
    let price = 1200u64;
    let period = 60i64;
    let (plan_pk, sub_pk) = create_plan_and_subscribe(&mut env, &actors, plan_id, price, period, 1);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = common::grace_pda(&sub_pk);

    // Drive to Grace: warp to T0 + period, charge tail flips state + inits satellite.
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");
    clock::set_clock(&mut env.svm, T0 + period);
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
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("charge tail into Grace");

    // Sanity: state byte == 2 (GracePeriod).
    let pre = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(pre.data[STATE_OFFSET], 2, "must be GracePeriod");

    env.svm.expire_blockhash();
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            1,
        ),
    ];

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    );
    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCleanup);

    // Grace preserved; Grace satellite alive.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 2,
        "state byte must remain GracePeriod (=2) after rejected composite"
    );
    assert!(
        env.svm.get_account(&graced_pk).is_some(),
        "Grace satellite must remain alive after rejected composite"
    );
}
