//! Adversarial tests for `charge` interaction with grace state — ADR-007
//! §I-CHARGE-3 + §6.2 (kickoff).
//!
//! Coverage:
//! - C.3 / I-CHARGE-3: charge with `state == GracePeriod` is rejected with
//!   `IllegalStateForCharge` (ADR-004 §2.h, post-ADR-007 reachable through
//!   the natural exhaustion → second-charge flow).
//! - C.7: when a charge would exhaust the stream but the keeper omitted the
//!   `GracedSubscription` PDA, the handler raises `MissingGraceSatellite`
//!   so the keeper can re-submit with the satellite. ADR-007 §"charge
//!   handler tail" + impl note in `top_up.rs`/`charge.rs`.

mod common;

use common::{
    clock,
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

fn create_and_subscribe(
    env: &mut common::TestEnv,
    actors: &common::Actors,
) -> (
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
    solana_pubkey::Pubkey,
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

fn fresh_keeper(env: &mut common::TestEnv) -> solana_keypair::Keypair {
    let k = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&k.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");
    k
}

/// Source: ADR-007 §I-CHARGE-3 + ADR-004 §2.h — once state flips to
/// GracePeriod, subsequent charges hit the FSM guard at the top of
/// `charge_handler` BEFORE any account-level validation. The satellite
/// already exists (init was at-most-once), so a re-charge with the same
/// satellite passed cannot trigger init-collision; the FSM guard fires
/// first.
#[test]
fn charge_grace_state_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = fresh_keeper(&mut env);
    let (graced_pk, _) = common::grace_pda(&sub_pk);

    // First charge exhausts the stream → state == GracePeriod.
    clock::set_clock(&mut env.svm, T0 + 2 * PLAN_PERIOD);
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
    .expect("first charge enters grace");

    let sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(sub_acct.data[STATE_OFFSET], 2, "state must be GracePeriod");

    // Second charge against the same Subscription. ADR-007 §I-CHARGE-3:
    // the FSM guard at top of `charge_handler` fires before any further
    // CPI / account validation. We pass `None` for graced_subscription
    // (`init` would re-fail anyway since the satellite is alive); the FSM
    // guard short-circuits before that check.
    env.svm.expire_blockhash();
    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix_full(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
            &common::token_program_id(),
            None,
        )],
        &[&keeper],
    );

    assert_nakama_err::<()>(result, NakamaError::IllegalStateForCharge);
}

/// Source: ADR-007 §"charge handler tail" — when the post-CPI math would
/// flip into Grace but the caller omitted the satellite (passed
/// `program_id` placeholder), the handler raises `MissingGraceSatellite`.
///
/// Production keeper protocol: on receiving this error, the keeper
/// re-submits the same charge tx with the pre-derived GracedSubscription
/// PDA in the optional slot.
#[test]
fn charge_exhausts_without_satellite_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = fresh_keeper(&mut env);

    // advance to T = stream_start + 2*period — the post-CPI math will
    // exactly exhaust the stream (deposited_amount == withdrawn_amount).
    clock::set_clock(&mut env.svm, T0 + 2 * PLAN_PERIOD);

    // Default `charge_ix` passes None for graced_subscription (placeholder).
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

    assert_nakama_err::<()>(result, NakamaError::MissingGraceSatellite);
}
