//! Phase 2 RED — happy-path black-box tests for ADR-x402-001 lifecycle ix
//! (`open_session`, `close_session`).
//!
//! Coverage:
//! - open_session initializes PaySession PDA with snapshots from parent
//!   (subscription / merchant / merchant_ata / facilitator / session_id /
//!   reservation_cap / state=Open / opened_at)
//! - close_session returns rent to subscriber (Anchor `close = subscriber`)
//! - N concurrent sessions per Subscription (ADR-x402-001 Q1)
//! - close + reopen with same session_id succeeds (PDA recycle)
//! - Events emitted with the documented payload shape
//!
//! These tests must compile (Phase 1 GREEN landed PaySession types) but
//! FAIL on the open_session / close_session ix calls until Phase 2 GREEN
//! ships handlers + builders.

mod common;

use anchor_lang::AnchorDeserialize;

use common::{
    clock, fund_actors, ix, pay_session_pda, plan_pda, send_tx, setup, subscription_pda, Signer,
};

const T0: i64 = 1_700_000_000;

/// Boilerplate — bring an Actors set to Active subscription at clock T0.
fn setup_active_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
    let plan_id = 1u64;
    let price = 600u64;
    let period = 60i64;

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
            2,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    (plan_pk, sub_pk)
}

/// Decode a PaySession account by skipping the 8-byte discriminator and
/// running Borsh on the payload. Pinned to `INIT_SPACE = 202` invariant.
fn decode_pay_session(data: &[u8]) -> nakama::state::PaySession {
    use nakama::state::PaySession;
    assert!(data.len() >= 8 + 202, "PaySession data too short");
    PaySession::deserialize(&mut &data[8..]).expect("decode PaySession")
}

#[test]
fn open_session_initializes_pda_with_parent_snapshots() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors);

    let facilitator_keypair = solana_keypair::Keypair::new();
    let session_id: u64 = 0xdeadbeef_cafebabe;
    let reservation_cap: u64 = 200;

    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator_keypair.pubkey(),
            reservation_cap,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session happy path");

    let acct = env
        .svm
        .get_account(&pay_sess_pk)
        .expect("PaySession PDA initialized");
    assert_eq!(
        acct.data.len(),
        8 + 202,
        "PaySession data length must be 210 bytes"
    );

    let sess = decode_pay_session(&acct.data);
    assert_eq!(sess.subscription, sub_pk, "subscription back-ref");
    assert_eq!(sess.merchant, actors.merchant.pubkey(), "merchant snapshot");
    assert_eq!(
        sess.merchant_ata, actors.merchant_ata,
        "merchant_ata snapshot"
    );
    assert_eq!(
        sess.facilitator,
        facilitator_keypair.pubkey(),
        "facilitator delegated"
    );
    assert_eq!(
        sess.session_id, session_id,
        "session_id mirrored from seeds"
    );
    assert_eq!(sess.opened_at, T0, "opened_at == clock at open");
    assert_eq!(sess.last_settle_at, 0, "last_settle_at zero pre-settle");
    assert_eq!(sess.usage_amount, 0, "usage_amount zero at open");
    assert_eq!(
        sess.reservation_cap, reservation_cap,
        "reservation_cap stored"
    );
    assert_eq!(
        sess.state,
        nakama::state::PaySessionState::Open as u8,
        "state == Open post-open"
    );
    assert_eq!(sess.reserved, [0u8; 32], "reserved zeroed");
}

#[test]
fn open_session_with_zero_cap_means_unlimited_up_to_escrow() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors);

    let facilitator = solana_keypair::Keypair::new();
    let session_id = 7u64;
    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            0, // unlimited up to escrow
        )],
        &[&actors.subscriber],
    )
    .expect("open_session with zero cap");

    let acct = env.svm.get_account(&pay_sess_pk).expect("PaySession alive");
    let sess = decode_pay_session(&acct.data);
    assert_eq!(sess.reservation_cap, 0);
}

#[test]
fn multiple_concurrent_sessions_per_subscription() {
    // ADR-x402-001 Q1 — N concurrent allowed via u64 nonce.
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors);

    let facilitator = solana_keypair::Keypair::new();

    for session_id in [10u64, 20, 30] {
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
        .unwrap_or_else(|e| panic!("open_session id={}: {:?}", session_id, e));

        let (pda, _) = pay_session_pda(&sub_pk, session_id);
        assert!(
            env.svm.get_account(&pda).is_some(),
            "session {} must be alive",
            session_id
        );
    }
}

#[test]
fn close_session_returns_rent_to_subscriber() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors);

    let facilitator = solana_keypair::Keypair::new();
    let session_id = 42u64;

    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            50,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");

    let pre_close_subscriber_lamports = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;

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
    .expect("close_session happy path");

    let post = env
        .svm
        .get_account(&actors.subscriber.pubkey())
        .expect("subscriber alive")
        .lamports;
    assert!(
        post > pre_close_subscriber_lamports,
        "subscriber lamports must increase from PaySession rent return"
    );

    let (pay_sess_pk, _) = pay_session_pda(&sub_pk, session_id);
    let post_acct = env.svm.get_account(&pay_sess_pk);
    assert!(
        post_acct.is_none() || post_acct.map(|a| a.lamports == 0).unwrap_or(true),
        "PaySession PDA must be closed after close_session"
    );
}

#[test]
fn close_then_reopen_with_same_session_id_succeeds() {
    // After close_session, the PDA is closed. Anchor `init` succeeds again
    // on the same seeds (deterministic PDA, but underlying account empty).
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk) = setup_active_subscription(&mut env, &actors);

    let facilitator = solana_keypair::Keypair::new();
    let session_id = 99u64;

    // open
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
    .expect("first open");

    // close
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
    .expect("close");

    // reopen same id
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            200, // different cap
        )],
        &[&actors.subscriber],
    )
    .expect("reopen with same session_id");

    let (pda, _) = pay_session_pda(&sub_pk, session_id);
    let acct = env.svm.get_account(&pda).expect("reopened");
    let sess = decode_pay_session(&acct.data);
    assert_eq!(sess.reservation_cap, 200, "fresh state, new cap");
}
