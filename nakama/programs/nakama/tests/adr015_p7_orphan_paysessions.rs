//! ADR-015 — orphan-rent-lockup pattern
//! (security-audit-patterns.md §P7). Cleanup of Subscription parent does
//! NOT enumerate live PaySession children. After parent close, any extant
//! PaySession satellite becomes unreachable via `close_session` because
//! Anchor `Account<Subscription>` deserialization fails at account
//! validation (parent slot owner == system, not the program).
//!
//! Documented limitation — ADR-015 mentions a future-work entry for
//! enumerable enforcement OR a close_session refactor that accepts the
//! parent pubkey directly without Anchor-deserializing it. This test pins
//! the CURRENT behavior so a future fix that closes the orphan path must
//! explicitly update the assertion.
//!
//! Pinned cases:
//! * 3 open PaySessions → cancel → cleanup → close_session on any of them
//!   fails (orphaned).
//! * 0 open PaySessions → cancel → cleanup → no orphan (sanity).
//! * 3 open → close all → cancel → cleanup → happy path (SDK protocol works).

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, error::anchor_codes, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

fn read_account_owner(svm: &litesvm::LiteSVM, pk: &Pubkey) -> Option<Pubkey> {
    svm.get_account(pk).map(|a| a.owner)
}

fn setup_active_sub_with_sessions(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    facilitator_pk: &Pubkey,
    n_sessions: u64,
) -> Pubkey {
    let plan_price: u64 = 1_200;
    let plan_period: i64 = 60;
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
            2,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    for session_id in 0..n_sessions {
        env.svm.expire_blockhash();
        send_tx(
            &mut env.svm,
            &actors.subscriber,
            &[ix::open_session_ix(
                &actors.subscriber.pubkey(),
                &sub_pk,
                session_id,
                facilitator_pk,
                0, // unlimited (no reservation cap)
            )],
            &[&actors.subscriber],
        )
        .unwrap_or_else(|e| panic!("open_session #{session_id}: {:?}", e));
    }

    sub_pk
}

/// P7 main pin. 3 PaySessions open → cancel parent → cleanup parent → each
/// of the 3 pay_sessions is orphaned. close_session on any of them fails
/// because Anchor cannot deserialize the (now-closed) Subscription parent
/// account: owner != program_id (cleaned to system) AND data is empty,
/// producing `ACCOUNT_NOT_INITIALIZED` (3012) or
/// `ACCOUNT_OWNED_BY_WRONG_PROGRAM` (3007). The test accepts either —
/// both prove the orphan condition.
#[test]
fn cleanup_orphans_paysessions_close_fails() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();

    let sub_pk = setup_active_sub_with_sessions(&mut env, &actors, &facilitator.pubkey(), 3);

    // cancel + cleanup. Cancel mid-period so settle math is well-defined.
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
    .expect("cancel parent");

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup parent");

    // Parent is closed: account may not exist OR may be owned by system.
    let parent_owner = read_account_owner(&env.svm, &sub_pk);
    assert!(
        parent_owner.is_none() || parent_owner == Some(Pubkey::default()),
        "parent Subscription closed by cleanup, owner = system_program (default) or account absent"
    );

    // Now try to close each session — the orphan rent is locked.
    for session_id in 0..3u64 {
        env.svm.expire_blockhash();
        let r = send_tx(
            &mut env.svm,
            &actors.subscriber,
            &[ix::close_session_ix(
                &actors.subscriber.pubkey(),
                &sub_pk,
                session_id,
            )],
            &[&actors.subscriber],
        );
        // Either Anchor 3012 (AccountNotInitialized) OR 3007
        // (AccountOwnedByWrongProgram) — depends on whether close_account
        // ran `system_program::transfer` (zero lamports) which deletes
        // the account vs leaves it owner=system, data=[].
        let meta = match r {
            Ok(_) => panic!(
                "orphan close_session #{} unexpectedly succeeded",
                session_id
            ),
            Err(m) => m,
        };
        let code = common::error::extract_custom_code(&meta).unwrap_or(0);
        assert!(
            code == anchor_codes::ACCOUNT_NOT_INITIALIZED
                || code == anchor_codes::ACCOUNT_OWNED_BY_WRONG_PROGRAM,
            "expected Anchor 3012 or 3007 for orphaned close_session, got {}: {:?}",
            code,
            meta.err
        );
    }
}

/// Sanity: 0 sessions → cleanup is happy. No orphans possible — the SDK
/// happy-path baseline.
#[test]
fn cleanup_with_zero_sessions_no_orphan() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();

    let sub_pk = setup_active_sub_with_sessions(&mut env, &actors, &facilitator.pubkey(), 0);

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
    .expect("cancel parent");

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup parent — no orphans to worry about");
}

/// Happy-path teardown: 3 sessions opened, all closed FIRST, then cancel +
/// cleanup. No orphans. Pins that the SDK's recommended teardown order
/// works on-chain. If a future SDK helper enforces this order, the
/// regression catches breakage.
#[test]
fn close_all_sessions_then_cleanup_happy_path() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();

    let sub_pk = setup_active_sub_with_sessions(&mut env, &actors, &facilitator.pubkey(), 3);

    // Close each session while parent is still Active.
    for session_id in 0..3u64 {
        env.svm.expire_blockhash();
        send_tx(
            &mut env.svm,
            &actors.subscriber,
            &[ix::close_session_ix(
                &actors.subscriber.pubkey(),
                &sub_pk,
                session_id,
            )],
            &[&actors.subscriber],
        )
        .unwrap_or_else(|e| panic!("close_session #{session_id}: {:?}", e));
    }

    // Now cancel + cleanup is clean.
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

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup after orderly teardown");
}

/// Sanity: PaySession is alive between cleanup and close attempt — Borsh
/// decode of an unrelated artefact. Exercises the parent.owner == system
/// post-cleanup state to confirm test setup assumption.
#[test]
fn parent_subscription_account_is_dead_after_cleanup() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    // Minimal lifecycle: subscribe → cancel → cleanup, no sessions.
    let plan_price: u64 = 600;
    let plan_period: i64 = 60;
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
    clock::set_clock(&mut env.svm, T0 + plan_period / 2);
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
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup");

    let owner = read_account_owner(&env.svm, &sub_pk);
    assert!(
        owner.is_none() || owner == Some(Pubkey::default()),
        "parent dead after cleanup; observed owner: {:?}",
        owner
    );

    // Borsh decode would fail, but we don't call it — that's the whole point
    // of the orphan condition for downstream session-close attempts.
    let _ = nakama::state::Subscription::deserialize;
}
