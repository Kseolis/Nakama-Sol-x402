//! Hardening cycle S3 — behavioral-trigger coverage for error variants that
//! had ZERO direct trigger test in the 161-test baseline (only appeared in the
//! `common::error` mirror or the `x402_state_layout` code-value table).
//!
//! Variants closed here:
//! - 6025 `IllegalStateForSettle`        — settle on a non-Open PaySession
//!   (ADR-x402-001 §"settle_usage" guard 3 + §"Internal FSM").
//! - 6028 `PaySessionParentMismatch`     — PaySession `.subscription` back-ref
//!   diverges from the supplied parent (ADR-x402-001 §Adversarial 9
//!   defence-in-depth above the PDA seed).
//! - 6029 `IllegalStateForClose`         — close on a stuck `Settling` session
//!   (ADR-x402-001 §"close_session" R3 boundary).
//! - 6032 `PaySessionMerchantAtaMismatch`— settle with a merchant_ata that does
//!   not match the session snapshot (ADR-x402-001 §"settle_usage" address-pin).
//! - 6038 `InvalidPeriod`                — defensive guard on a corrupted
//!   `Subscription.period <= 0` snapshot (ADR-015 §F4).
//!
//! Black-box discipline: account injection via `svm.set_account` mutates only
//! the on-chain bytes (state byte / period i64 / back-ref pubkey) reachable to
//! any adversary that can craft account data; instruction dispatch stays on the
//! public ABI builders in `common::ix`.

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_program_id, vault_pda,
    Signer,
};
use solana_pubkey::Pubkey;

const T0: i64 = 1_700_000_000;

/// PaySession `state` byte: account.data offset 8 (disc) + 168 (layout) = 176.
/// ADR-x402-001 §"PaySession PDA Layout".
const PAY_SESSION_STATE_OFFSET: usize = 8 + 168;
/// PaySession `subscription` back-ref: account.data offset 8 (disc) + 0.
const PAY_SESSION_SUBSCRIPTION_OFFSET: usize = 8;
/// `PaySessionState::Settling` discriminant (Open=0, Settling=1, Closed=2).
const PAY_SESSION_STATE_SETTLING: u8 = 1;

/// `Subscription.period` i64: account.data offset 8 + 8 + 32 + 32 + 8 = 88.
/// ADR-001 revised layout (next_charge_at, subscriber, plan, price precede it).
const SUBSCRIPTION_PERIOD_OFFSET: usize = 8 + 8 + 32 + 32 + 8;

fn setup_active_subscription(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    price: u64,
    period: i64,
    periods: u8,
) -> (Pubkey, Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            price,
            period,
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

fn open_session(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    sub_pk: &Pubkey,
    session_id: u64,
    facilitator: &Pubkey,
    cap: u64,
) {
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            sub_pk,
            session_id,
            facilitator,
            cap,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");
}

/// Overwrite a single byte inside an account's data, preserving owner/lamports.
fn poke_byte(svm: &mut litesvm::LiteSVM, address: &Pubkey, offset: usize, value: u8) {
    let mut acct = svm.get_account(address).expect("account alive for poke");
    acct.data[offset] = value;
    svm.set_account(*address, acct).expect("set poked account");
}

/// Overwrite 32 bytes (a Pubkey) inside an account's data.
fn poke_pubkey(svm: &mut litesvm::LiteSVM, address: &Pubkey, offset: usize, value: &Pubkey) {
    let mut acct = svm.get_account(address).expect("account alive for poke");
    acct.data[offset..offset + 32].copy_from_slice(value.as_ref());
    svm.set_account(*address, acct).expect("set poked account");
}

/// 6025 — settle_usage rejects a PaySession whose persisted state is `Settling`.
/// Reachable in production only if a prior settle crashed mid-CPI; we synthesise
/// the stuck byte via injection. ADR-x402-001 §"settle_usage" guard 3.
#[test]
fn settle_usage_on_settling_state_session_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let (_plan, sub_pk) = setup_active_subscription(&mut env, &actors, 600, 60, 4);
    let session_id = 7u64;
    open_session(
        &mut env,
        &actors,
        &sub_pk,
        session_id,
        &facilitator.pubkey(),
        0,
    );

    let (pay_session_pk, _) = common::pay_session_pda(&sub_pk, session_id);
    poke_byte(
        &mut env.svm,
        &pay_session_pk,
        PAY_SESSION_STATE_OFFSET,
        PAY_SESSION_STATE_SETTLING,
    );

    let (vault_pk, _) = vault_pda(&sub_pk);
    clock::set_clock(&mut env.svm, T0 + 30);
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
            50,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForSettle);
}

/// 6029 — close_session rejects a stuck `Settling` PaySession. Recovery is the
/// post-MVP R3 force_close path; close must NOT silently reclaim rent on an
/// indeterminate settle. ADR-x402-001 §"close_session".
#[test]
fn close_session_on_settling_state_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();

    let (_plan, sub_pk) = setup_active_subscription(&mut env, &actors, 600, 60, 4);
    let session_id = 11u64;
    open_session(
        &mut env,
        &actors,
        &sub_pk,
        session_id,
        &facilitator.pubkey(),
        0,
    );

    let (pay_session_pk, _) = common::pay_session_pda(&sub_pk, session_id);
    poke_byte(
        &mut env.svm,
        &pay_session_pk,
        PAY_SESSION_STATE_OFFSET,
        PAY_SESSION_STATE_SETTLING,
    );

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::close_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForClose);
}

/// 6028 — settle_usage rejects a PaySession whose `.subscription` back-ref does
/// not equal the supplied parent. We corrupt the back-ref to a foreign pubkey
/// while the PDA seeds still resolve (seed uses `parent.key()` +
/// `pay_session.session_id`, both unchanged). The defence-in-depth constraint
/// `pay_session.subscription == parent.key()` fires. ADR-x402-001 §Adversarial 9.
#[test]
fn settle_usage_with_parent_ref_mismatch_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let (_plan, sub_pk) = setup_active_subscription(&mut env, &actors, 600, 60, 4);
    let session_id = 13u64;
    open_session(
        &mut env,
        &actors,
        &sub_pk,
        session_id,
        &facilitator.pubkey(),
        0,
    );

    let (pay_session_pk, _) = common::pay_session_pda(&sub_pk, session_id);
    let foreign = Pubkey::new_unique();
    poke_pubkey(
        &mut env.svm,
        &pay_session_pk,
        PAY_SESSION_SUBSCRIPTION_OFFSET,
        &foreign,
    );

    let (vault_pk, _) = vault_pda(&sub_pk);
    clock::set_clock(&mut env.svm, T0 + 30);
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
            50,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::PaySessionParentMismatch);
}

/// 6032 — settle_usage rejects a merchant_ata that does not match the
/// PaySession snapshot. Anchor's `address = pay_session.merchant_ata @
/// PaySessionMerchantAtaMismatch` surfaces the custom code. We pass a real but
/// wrong TokenAccount (a freshly created ATA owned by a third party with the
/// correct mint). ADR-x402-001 §"settle_usage".
#[test]
fn settle_usage_with_wrong_merchant_ata_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");

    let (_plan, sub_pk) = setup_active_subscription(&mut env, &actors, 600, 60, 4);
    let session_id = 17u64;
    open_session(
        &mut env,
        &actors,
        &sub_pk,
        session_id,
        &facilitator.pubkey(),
        0,
    );

    // A correctly-minted but wrong-destination ATA (third party owner).
    let interloper = solana_keypair::Keypair::new();
    let wrong_ata =
        common::install_funded_ata(&mut env.svm, &interloper.pubkey(), &common::usdc_mint(), 0);

    let (vault_pk, _) = vault_pda(&sub_pk);
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &facilitator,
        &[ix::settle_usage_ix(
            &facilitator.pubkey(),
            &sub_pk,
            session_id,
            &vault_pk,
            &wrong_ata,
            &token_program_id(),
            50,
        )],
        &[&facilitator],
    );

    assert_nakama_err::<()>(result, NakamaError::PaySessionMerchantAtaMismatch);
}

/// 6038 — `InvalidPeriod` defensive guard. The snapshot `Subscription.period`
/// is `> 0` for every honestly-created subscription (subscribe enforces
/// `Plan.period > 0`), so this is reached only on a corrupted account. We
/// inject `period = 0` on an Active subscription and trigger the guard via the
/// `cancel` settle math path (charge / settle_usage share the same guard).
/// ADR-015 §F4 — distinct from `ZeroPeriod` (which guards create_plan/subscribe).
#[test]
fn cancel_with_corrupted_zero_period_snapshot_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let (_plan, sub_pk) = setup_active_subscription(&mut env, &actors, 600, 60, 4);

    // Corrupt the period i64 to 0 (leaves stream_start / seeds intact so the
    // clock guard at cancel passes and the `period > 0` guard is the rejecter).
    let mut acct = env.svm.get_account(&sub_pk).expect("sub alive");
    acct.data[SUBSCRIPTION_PERIOD_OFFSET..SUBSCRIPTION_PERIOD_OFFSET + 8]
        .copy_from_slice(&0i64.to_le_bytes());
    env.svm
        .set_account(sub_pk, acct)
        .expect("set corrupted sub");

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::InvalidPeriod);
}
