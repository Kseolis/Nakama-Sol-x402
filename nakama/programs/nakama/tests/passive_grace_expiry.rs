//! Passive grace expiry — ADR-007 §Decision §"Passive expiry" + §I-GRACE-4
//! + Q5 fixation.
//!
//! Black-box: written from the kickoff §6.4 file plan + ADR-007's
//! "passive expiry" contract.
//!
//! On-chain contract: there is NO `expire_grace` instruction (rejected
//! alternative (h)). When `now > grace_until` and no `top_up`/`cancel`
//! has fired, the state byte STAYS `GracePeriod` and the satellite STAYS
//! alive. Off-chain `ComputedStatus::GraceExpired` is derived from
//! `(state, grace_until, now)` — that derivation is OFF-chain and is NOT
//! the subject of this file.
//!
//! Coverage:
//! - I-GRACE-4: state byte byte-equal `GracePeriod` (= 2) before AND after
//!   warping past `grace_until` with no instruction invoked.
//! - I-GRACE-4: satellite account body length unchanged (still 56) past
//!   `grace_until`.
//! - Q5 cross-pin: a top_up after passive expiry STILL works (parallel to
//!   `top_up_grace::top_up_after_passive_grace_expiry_still_works`; here
//!   we add a stricter assertion sweep right at `grace_until` itself, then
//!   far past, to prove the byte-only guard).

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
    GRACE_DURATION, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

fn drive_to_grace(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    i64,
) {
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
    let (graced_pk, _) = common::grace_pda(&sub_pk);

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

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");

    let exhaust_at = T0 + 2 * PLAN_PERIOD;
    clock::set_clock(&mut env.svm, exhaust_at);
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
    .expect("charge into grace");

    (
        plan_pk,
        sub_pk,
        vault_pk,
        graced_pk,
        exhaust_at + GRACE_DURATION,
    )
}

/// Source: ADR-007 §I-GRACE-4 — passive expiry contract.
///
/// After warping the clock to `grace_until + N` for both small N (=1) and
/// large N (= 2 * GRACE_DURATION), with NO instruction invoked, the
/// observable on-chain state must be unchanged:
///   - Subscription state byte == 2 (GracePeriod).
///   - GracedSubscription account exists with body length 56.
///   - GracedSubscription's `entered_grace_at` and `grace_until` fields
///     unchanged.
///
/// This pins the "no on-chain transition out of Grace passively"
/// invariant the off-chain `ComputedStatus::GraceExpired` derivation
/// depends on.
#[test]
fn passive_grace_expiry_no_state_change() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (_plan_pk, sub_pk, _vault_pk, graced_pk, grace_until) = drive_to_grace(&mut env, &actors);

    // Snapshot satellite + parent state.
    let pre_graced = env.svm.get_account(&graced_pk).expect("alive").data.clone();
    let pre_state_byte = env.svm.get_account(&sub_pk).expect("alive").data[STATE_OFFSET];
    assert_eq!(
        pre_state_byte, 2,
        "state must be GracePeriod at grace entry"
    );
    assert_eq!(
        pre_graced.len(),
        56,
        "I-GRACE-2 / I-GRACE-4: satellite body length must be 56 at grace entry"
    );

    // 1) Warp to grace_until + 1 — first second past expiry, no ix invoked.
    clock::set_clock(&mut env.svm, grace_until + 1);
    let post_state = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post_state.data[STATE_OFFSET], 2,
        "I-GRACE-4: state byte must stay GracePeriod 1s past grace_until"
    );
    let post_graced = env.svm.get_account(&graced_pk).expect("alive");
    assert_eq!(
        post_graced.data.len(),
        56,
        "I-GRACE-4: satellite body unchanged 1s past grace_until"
    );
    assert_eq!(
        post_graced.data, pre_graced,
        "I-GRACE-4: satellite bytes byte-equal pre/post passive expiry boundary"
    );

    // 2) Warp far past — 2 × GRACE_DURATION beyond grace_until.
    clock::set_clock(&mut env.svm, grace_until + 2 * GRACE_DURATION);
    let far_state = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        far_state.data[STATE_OFFSET], 2,
        "I-GRACE-4: state byte must stay GracePeriod arbitrarily far past grace_until"
    );
    let far_graced = env.svm.get_account(&graced_pk).expect("alive");
    assert_eq!(
        far_graced.data, pre_graced,
        "I-GRACE-4: satellite bytes byte-equal pre/post 2x GRACE_DURATION"
    );
}

/// Source: ADR-007 Q5 — top_up after passive expiry succeeds at the EXACT
/// boundary `now == grace_until + 1` (the smallest "past expiry"
/// timestamp). Cross-pin to `top_up_grace::top_up_after_passive_grace_
/// expiry_still_works` which tested far-past; this one tests the
/// boundary +1.
#[test]
fn top_up_at_grace_until_plus_one_succeeds() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (_plan_pk, sub_pk, _vault_pk, _graced_pk, grace_until) = drive_to_grace(&mut env, &actors);

    clock::set_clock(&mut env.svm, grace_until + 1);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix_with_grace(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            500,
        )],
        &[&actors.subscriber],
    )
    .expect("Q5: top_up succeeds at grace_until + 1");

    // State flipped to Active.
    let post = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        post.data[STATE_OFFSET], 0,
        "Q5: top_up at grace_until + 1 must recover to Active"
    );
}

/// Source: kickoff §6.1 row "(no satellite, but state==GracePeriod by
/// hand) → top_up reject" — synthesise an UNREACHABLE-via-natural-flow
/// state to prove the handler-side `MissingGraceSatellite` guard fires
/// at the boundary we expect.
///
/// Setup: subscribe → byte-mutate the state byte to GracePeriod (= 2)
/// without ever inviting `charge` to init the satellite. Top-up call
/// SHOULD raise `MissingGraceSatellite`, NOT touch the vault.
///
/// This is a partial duplicate of the same case in
/// `top_up_signer_guards::top_up_grace_state_without_satellite_rejected`,
/// kept here so the passive-expiry file is a self-contained pin on the
/// "byte is the only on-chain guard" contract that Q5 depends on. If you
/// remove either copy, leave a comment pointing at the other so the
/// invariant doesn't quietly lose coverage.
#[test]
fn passive_synth_grace_state_no_satellite_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    let plan_id = 9u64;
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
    let _ = vault_pda(&sub_pk);

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

    // Mutate state byte to GracePeriod without ever creating the satellite.
    let mut sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    sub_acct.data[STATE_OFFSET] = 2;
    env.svm
        .set_account(sub_pk, sub_acct)
        .expect("plant GracePeriod byte");

    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            500,
        )],
        &[&actors.subscriber],
    );

    common::error::assert_nakama_err::<()>(
        result,
        common::error::NakamaError::MissingGraceSatellite,
    );
}
