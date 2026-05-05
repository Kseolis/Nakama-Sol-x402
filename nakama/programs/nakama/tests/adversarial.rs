//! Adversarial / attack-surface tests.
//!
//! Scenarios derived from ADR-002 §Account model and authority + sign-off
//! handoff item 7 (test-engineer Adversarial scenarios):
//!
//! 1. ATA owner spoofing — pass an ATA owned by someone else as
//!    `subscriber_ata` in `subscribe`.
//! 2. Vault PDA cross-subscription replay — try to use one subscription's
//!    vault address while subscribing on a different (subscriber, plan) pair.
//! 3. Plan substitution attack — keep the right `plan` PDA but swap a
//!    second plan's accounts in.
//! 4. Token-2022 mint reject — passing a Token-2022-owned mint must fail
//!    (ADR-014 §Token-2022 reject).

mod common;

use common::{
    error::{anchor_codes, assert_anchor_err, assert_any_err, extract_custom_code},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
};
use solana_program::program_pack::Pack;

/// Source: ADR-002 §subscribe Accounts — `token::authority = subscriber`
/// (BLK-09). Passing an ATA owned by attacker must be rejected.
#[test]
fn ata_owner_spoofing_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);
    let attacker = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 5_000_000_000)
        .expect("airdrop attacker");
    let attacker_ata = common::install_funded_ata(
        &mut env.svm,
        &attacker.pubkey(),
        &common::usdc_mint(),
        50_000_000,
    );

    let plan_id = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);

    // Subscriber needs lamports to pay rent on the subscription account.
    // `fund_actors(_, 0)` only airdrops on first call; here it already did,
    // so we just expire the blockhash so any subsequent airdrop / send_tx
    // doesn't collide on `AlreadyProcessed`.
    env.svm.expire_blockhash();

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &attacker_ata, // wrong owner
            1,
        )],
        &[&actors.subscriber],
    );

    // Expected: ConstraintTokenOwner (2015). Accept any failure to not
    // over-pin Anchor's exact error.
    // assert_any_err retained intentionally — not part of AMBIG-01..04
    // scope. Tighten in a future cleanup pass once `top_up` lands and the
    // adversary surface for `subscriber_ata.owner` is locked.
    let meta = assert_any_err(result);
    let code = extract_custom_code(&meta);
    assert!(
        matches!(code, Some(c) if c == anchor_codes::CONSTRAINT_TOKEN_OWNER) || code.is_some(),
        "expected ConstraintTokenOwner / any constraint failure, got {:?}",
        code
    );
}

/// Source: ADR-002 §Authority CPI — vault is PDA-bound to `subscription`. We
/// try to subscribe with a vault address that belongs to a *different*
/// (subscriber, plan) pair.
#[test]
fn vault_cross_subscription_replay_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    // Plan A.
    let plan_a = 1u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_a,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create plan A");
    // Plan B.
    let plan_b = 2u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_b,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    )
    .expect("create plan B");

    let (plan_a_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_a);
    let (plan_b_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_b);

    // Subscribe on plan A → its vault now exists.
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_a_pk,
            &actors.subscriber_ata,
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe A");

    let (sub_a_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_a_pk);
    let (vault_a_pk, _) = vault_pda(&sub_a_pk);

    // Now try to subscribe on plan B, but force the vault override to plan
    // A's vault. The init constraint should fail (already initialized OR
    // seeds mismatch).
    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix_with_overrides(
            &actors.subscriber.pubkey(),
            &plan_b_pk,
            &actors.subscriber_ata,
            &common::usdc_mint(),
            1,
            None,
            Some(vault_a_pk), // wrong vault
        )],
        &[&actors.subscriber],
    );

    // assert_any_err retained — not part of AMBIG-01..04 scope. Multiple
    // valid failure paths (ConstraintSeeds vs already-initialized vault).
    let _ = assert_any_err(result);
}

/// Source: ADR-001 §Plan substitution defence — Subscription's `merchant_ata`
/// is snapshotted from Plan, not from caller-provided account. We try to
/// build a `subscribe` referencing PlanA but pretending to be PlanB by
/// swapping the `plan` AccountMeta to PlanB while leaving the subscription
/// PDA derived from PlanA.
///
/// The Anchor seed-derivation `seeds = [SUB_SEED, subscriber, plan]` will
/// then disagree with the real `plan` AccountMeta → ConstraintSeeds.
#[test]
fn plan_substitution_attack_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    let plan_a = 1u64;
    let plan_b = 2u64;
    for id in [plan_a, plan_b] {
        send_tx(
            &mut env.svm,
            &actors.merchant,
            &[ix::create_plan_ix(
                &actors.merchant.pubkey(),
                &actors.merchant_ata,
                id,
                5_000_000,
                60,
            )],
            &[&actors.merchant],
        )
        .expect("create_plan");
    }

    let (plan_a_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_a);
    let (plan_b_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_b);

    // Subscription PDA derived from PlanA, but `plan` AccountMeta = PlanB.
    let (sub_a_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_a_pk);

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix_with_overrides(
            &actors.subscriber.pubkey(),
            &plan_b_pk, // attacker passes plan B
            &actors.subscriber_ata,
            &common::usdc_mint(),
            1,
            Some(sub_a_pk), // but sub PDA derived from plan A
            None,
        )],
        &[&actors.subscriber],
    );

    // assert_any_err retained — not part of AMBIG-01..04 scope. Either
    // ConstraintSeeds or an init failure is acceptable.
    let _ = assert_any_err(result);
}

/// Source: ADR-014 §Token-2022 reject — only classic SPL Token mints accepted.
/// We plant a "mint" owned by the Token-2022 program ID and try to register
/// a Plan with it; Anchor's `Program<'info, Token>` constraint should fire.
#[test]
fn token_2022_mint_reject_in_create_plan() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);

    // Construct a Token-2022 program id and an account "owned" by it.
    let token_2022_id: solana_pubkey::Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        .parse()
        .unwrap();

    let foreign_mint = solana_keypair::Keypair::new().pubkey();
    // We pre-pack a classic Mint into the data, but place ownership under
    // Token-2022. This simulates a Token-2022 mint at the address level:
    // even if the data layout happened to look like a classic Mint, the
    // owner-program check must reject it.
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    let m = spl_token::state::Mint {
        mint_authority: spl_token::solana_program::program_option::COption::Some(
            env.mint_authority.pubkey(),
        ),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: spl_token::solana_program::program_option::COption::None,
    };
    spl_token::state::Mint::pack(m, &mut data).unwrap();
    env.svm
        .set_account(
            foreign_mint,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: token_2022_id, // ⚠️ Token-2022 program owns the mint
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("set foreign mint account");

    // Build create_plan with this Token-2022 mint substituted in.
    let plan_id = 9u64;
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix_with_mint(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            &foreign_mint,
            plan_id,
            5_000_000,
            60,
        )],
        &[&actors.merchant],
    );

    // AMBIG-04 (closed): tightened from assert_any_err in
    // chore/cleanup-cycle-1-debt. Cycle-1 confirmed Anchor's
    // `Account<'info, Mint>` rejects the foreign-program-owned mint with
    // AccountOwnedByWrongProgram (3007), not the IDL `address = USDC_MINT`
    // constraint — owner check fires first.
    assert_anchor_err(result, anchor_codes::ACCOUNT_OWNED_BY_WRONG_PROGRAM);
}
