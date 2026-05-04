//! Error-path tests for `subscribe`.
//!
//! Coverage:
//! - BLK-07 / ADR-002 §subscribe step 2 — `ZeroPeriodsToFund`
//! - BLK-02 / ADR-002 §subscribe step 4 — `ZeroRatePerSecond` (price < period)

mod common;

use common::{
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, vault_pda, Signer,
};

/// Source: ADR-002 §subscribe step 2, BLK-07 — `periods_to_prefund == 0` rejected.
#[test]
fn zero_periods_to_fund_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

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

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            0, // BLK-07: zero periods rejected
        )],
        &[&actors.subscriber],
    );

    assert_nakama_err::<()>(result, NakamaError::ZeroPeriodsToFund);
}

/// Source: ADR-002 §subscribe step 4, BLK-02 — `price < period` collapses
/// `rate_per_second` to 0 → must reject with `ZeroRatePerSecond`.
///
/// price = 1 (1 micro-USDC), period = 60 → rate = 1/60 = 0.
#[test]
fn zero_rate_per_second_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    let plan_id = 2u64;
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            plan_id,
            1,  // price < period
            60, // → rate=0
        )],
        &[&actors.merchant],
    )
    .expect("create_plan");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);

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

    assert_nakama_err::<()>(result, NakamaError::ZeroRatePerSecond);
}

/// Source: ADR-002 §subscribe Accounts struct — `subscriber_ata.mint` must
/// match `plan.token_mint`. Passing a wrong-mint TokenAccount must fail at
/// the Anchor `token::mint` constraint (BLK-09).
///
/// Black-box: we don't peek at impl, but ADR-002 §"Notes on the sketch" makes
/// `token::authority = subscriber` explicit; mint is enforced via the vault's
/// `token::mint = plan.token_mint` plus the `token_mint` account constraint.
/// Either way, the tx must fail and not silently accept a foreign mint.
#[test]
fn subscribe_with_wrong_mint_ata_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    // Create plan over USDC.
    let plan_id = 3u64;
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

    // Plant a foreign mint and a TokenAccount on that mint owned by subscriber.
    let foreign_mint = solana_keypair::Keypair::new().pubkey();
    common::install_mint(
        &mut env.svm,
        &foreign_mint,
        &env.mint_authority.pubkey(),
        6,
    );
    let bad_subscriber_ata =
        common::install_funded_ata(&mut env.svm, &actors.subscriber.pubkey(), &foreign_mint, 50_000_000);

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &bad_subscriber_ata, // wrong mint
            1,
        )],
        &[&actors.subscriber],
    );

    // AMBIG-03 (closed): tightened from assert_any_err in
    // chore/cleanup-cycle-1-debt. Cycle-1 confirmed Anchor's
    // `token::mint = plan.token_mint` constraint on `subscriber_ata` fires
    // first → ConstraintTokenMint (2014).
    common::error::assert_anchor_err(result, common::error::anchor_codes::CONSTRAINT_TOKEN_MINT);
}

/// Source: `docs/impl-cycle-1-security-audit.md` §F-2 — defense-in-depth:
/// `subscribe` must reject `subscriber_ata == vault`. Without that guard,
/// SPL Token's no-op self-transfer would pollute Subscription with
/// `deposited_amount` while the vault stays empty (subscriber pays
/// nothing yet has a "valid" subscription).
///
/// Construction (approach (a) per Phase 1c spec): pre-plant an SPL
/// TokenAccount at the deterministic vault PDA address (mint=USDC,
/// owner=subscriber so existing `token::authority` constraint cannot
/// short-circuit), then call subscribe with `subscriber_ata = vault_pda`.
///
/// Why `assert_any_err` (intentional, not AMBIG): Anchor account
/// validation (`init` on vault) runs **before** the handler body, so the
/// pre-planted account triggers System Program `AccountAlreadyInUse`
/// (`Custom(0)`) before the handler's `require_keys_neq!` ever executes.
/// The security invariant "subscriber_ata == vault is rejected" is proven
/// today via the collision path; the explicit `DuplicateAtaAndVault`
/// require! only becomes reachable when ADR-005 `top_up` lands (vault
/// already initialized → no collision → require! fires). Tightening
/// to `assert_nakama_err(_, DuplicateAtaAndVault)` is deferred to the
/// ADR-005 cycle for that reason.
#[test]
fn subscribe_with_subscriber_ata_equal_to_vault_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 50_000_000);

    let plan_id = 4u64;
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
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);

    // Plant a SPL TokenAccount at the vault PDA address with mint=USDC,
    // owner=subscriber. Both `token::mint` and `token::authority`
    // constraints on `subscriber_ata` would pass on this account.
    common::install_token_account(
        &mut env.svm,
        &vault_pk,
        &common::usdc_mint(),
        &actors.subscriber.pubkey(),
        0,
    );

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &vault_pk, // subscriber_ata == vault — F-2 attack vector
            1,
        )],
        &[&actors.subscriber],
    );

    // F-2: assert_any_err is intentional — see doc-comment above.
    common::error::assert_any_err(result);
}
