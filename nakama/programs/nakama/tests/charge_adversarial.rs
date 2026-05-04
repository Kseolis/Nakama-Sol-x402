//! Adversarial tests for `charge` (sign-off handoff item 7).
//!
//! Black-box: written from ADR-004 §9 Accounts struct + §6 Token program
//! whitelist, NOT from `instructions/charge.rs`.
//!
//! Coverage:
//! - ADR-004 §9: `address = subscription.merchant_ata @ AtaMismatch`.
//! - ADR-004 §9: `vault seeds = [VAULT_SEED, subscription.key()]`
//!   + `bump = subscription.vault_bump`.
//! - ADR-004 §6: `Program<Token>` (classic SPL Token only — Token-2022 reject).
//! - ADR-004 §9: `has_one = plan` (BLK-04 keep-rationale).

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_anchor_err, assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;

fn create_and_subscribe_plan(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    plan_id: u64,
) -> (solana_pubkey::Pubkey, solana_pubkey::Pubkey, solana_pubkey::Pubkey) {
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

    if clock::now(&env.svm) != T0 {
        clock::set_clock(&mut env.svm, T0);
    }
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

/// Fresh keeper keypair with lamports for tx fee.
fn fresh_keeper(env: &mut common::TestEnv) -> solana_keypair::Keypair {
    let k = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&k.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");
    k
}

/// Source: ADR-004 §9 — `address = subscription.merchant_ata @ AtaMismatch`.
///
/// Plant a foreign ATA (correct mint, but owner = attacker) and pass it as
/// `merchant_ata`. The Anchor `address = ...` constraint should fire and map
/// to `NakamaError::AtaMismatch`.
#[test]
fn wrong_merchant_ata_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe_plan(&mut env, &actors, 1);

    // Attacker ATA on the same mint — passes mint check, fails address check.
    let attacker = solana_keypair::Keypair::new();
    let attacker_ata =
        common::install_funded_ata(&mut env.svm, &attacker.pubkey(), &common::usdc_mint(), 0);

    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
    let keeper = fresh_keeper(&mut env);

    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &attacker_ata, // not subscription.merchant_ata
            &keeper.pubkey(),
        )],
        &[&keeper],
    );

    assert_nakama_err::<()>(result, NakamaError::AtaMismatch);
}

/// Source: ADR-004 §9 — `vault seeds = [VAULT_SEED, subscription.key()]`.
///
/// Build two subscriptions on two plans for the same subscriber/merchant.
/// Pass subscription B's accounts (subscription + plan + merchant_ata) to
/// charge, but with vault A's address. Anchor's seed constraint
/// `seeds = [VAULT_SEED, subscription.key()] / bump = subscription.vault_bump`
/// should fire (`ConstraintSeeds`, 2006) before the handler body.
#[test]
fn wrong_vault_pda_rejected() {
    let mut env = setup();
    // Big enough ATA to cover two prefunds at price=600 * 2 periods * 2 plans = 2400.
    let actors = fund_actors(&mut env, 1_000_000);

    // Plan A + subscription A.
    let (_plan_a_pk, sub_a_pk, vault_a_pk) = create_and_subscribe_plan(&mut env, &actors, 1);

    // Plan B + subscription B (re-uses same subscriber's ATA, plan_id = 2).
    env.svm.expire_blockhash();
    let (plan_b_pk, sub_b_pk, _vault_b_pk) = create_and_subscribe_plan(&mut env, &actors, 2);

    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
    let keeper = fresh_keeper(&mut env);

    // Use sub B's subscription + plan + merchant_ata, but vault A — the
    // vault PDA must be derived from the subscription pubkey, so seeds
    // disagree.
    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix(
            &sub_b_pk,
            &plan_b_pk,
            &vault_a_pk, // wrong: belongs to subscription A
            &actors.merchant_ata,
            &keeper.pubkey(),
        )],
        &[&keeper],
    );

    // Acceptable failures (any one is correct):
    // - ConstraintSeeds (2006): vault seed derivation mismatch.
    // - ConstraintTokenOwner (2015): vault.authority != subscription B.
    // We pin to ConstraintSeeds (the constraint that fires first per ADR-004 §9
    // ordering: seeds checked before token::authority).
    //
    // Note (yellow flag for spec/test coupling): if anchor-engineer reorders
    // constraints in `Charge<'info>` such that `token::authority` runs first,
    // this assertion will need to swap to CONSTRAINT_TOKEN_OWNER (2015). That
    // would itself be an ADR-004 §9 deviation worth flagging back.
    assert_anchor_err(result, anchor_codes::CONSTRAINT_SEEDS);

    // Sanity: vault A is still a valid TokenAccount we never touched
    // (proves we didn't accidentally also drain A in the failed tx).
    let _ = sub_a_pk;
    assert!(env.svm.get_account(&vault_a_pk).is_some());
}

/// Source: ADR-004 §6 — `Program<'info, Token>` rejects Token-2022 program id.
///
/// We can't trivially substitute a Token-2022 *vault* in LiteSVM (would need
/// to plant a Token-2022-program-owned account at the vault PDA, which
/// requires the Token-2022 SO loaded — out of scope). Instead we exercise
/// the same constraint via the simpler vector: pass the Token-2022 program
/// id as the `token_program` AccountMeta. `Program<'info, Token>` checks
/// the AccountInfo's pubkey == classic Token program id (3008
/// InvalidProgramId on mismatch).
#[test]
fn token_2022_program_id_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe_plan(&mut env, &actors, 1);

    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
    let keeper = fresh_keeper(&mut env);

    // Token-2022 program id (mainnet & devnet identical).
    let token_2022_id: solana_pubkey::Pubkey =
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();

    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix_with_overrides(
            &sub_pk,
            &plan_pk,
            &vault_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
            &token_2022_id, // wrong program id
        )],
        &[&keeper],
    );

    assert_anchor_err(result, anchor_codes::INVALID_PROGRAM_ID);
}

/// Source: ADR-004 §9 — `has_one = plan` (BLK-04 keep-rationale).
///
/// Build two plans (A and B) on the same merchant, subscribe on A, then call
/// charge with subscription A but plan B. Anchor's `has_one = plan`
/// constraint compares `subscription.plan == plan.key()` and fails with
/// `ConstraintHasOne` (2001).
#[test]
fn plan_substitution_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

    // Plan A + subscribe.
    let (plan_a_pk, sub_a_pk, vault_a_pk) = create_and_subscribe_plan(&mut env, &actors, 1);

    // Plan B (no subscription on it).
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            2,
            PLAN_PRICE,
            PLAN_PERIOD,
        )],
        &[&actors.merchant],
    )
    .expect("create plan B");
    let (plan_b_pk, _) = plan_pda(&actors.merchant.pubkey(), 2);

    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
    let keeper = fresh_keeper(&mut env);

    let result = send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix(
            &sub_a_pk,
            &plan_b_pk, // attacker passes plan B
            &vault_a_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
        )],
        &[&keeper],
    );

    let _ = plan_a_pk; // suppress unused
    assert_anchor_err(result, anchor_codes::CONSTRAINT_HAS_ONE);
}
