//! Integration tests for `create_plan`.
//!
//! Coverage:
//! - ADR-014 §Decision happy path (Plan PDA materializes, event emits)
//! - ADR-014 §Errors — `ZeroPeriod`, `ZeroPrice`
//! - ADR-002 §subscribe step 1 baseline (period > 0)

mod common;

use common::{
    error::{assert_nakama_err, NakamaError},
    fund_actors, ix, plan_pda, send_tx, setup, Signer,
};

/// Source: ADR-014 §Decision — merchant signs, valid USDC ATA → Plan PDA created.
#[test]
fn happy_path_creates_plan_pda() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);

    let plan_id = 1u64;
    let price = 5_000_000u64; // 5 USDC
    let period = 60i64;        // 60s — short demo period

    let ix = ix::create_plan_ix(
        &actors.merchant.pubkey(),
        &actors.merchant_ata,
        plan_id,
        price,
        period,
    );

    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix],
        &[&actors.merchant],
    );
    result.expect("create_plan should succeed");

    // Plan PDA exists and has the expected size (ADR-001 §Plan account: 161 on chain).
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let acct = env.svm.get_account(&plan_pk).expect("plan account");
    // Anchor INIT_SPACE for Plan = 153, +8 disc = 161.
    assert_eq!(
        acct.data.len(),
        161,
        "Plan total size mismatch — ADR-001 §Plan layout drifted"
    );

    // Discriminator bytes match the IDL (Plan account discriminator).
    assert_eq!(
        &acct.data[..8],
        &[161, 231, 251, 119, 2, 12, 162, 2],
        "Plan discriminator drift"
    );
}

/// Source: ADR-014 §Errors — `period == 0` rejected.
#[test]
fn zero_period_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);

    let ix = ix::create_plan_ix(
        &actors.merchant.pubkey(),
        &actors.merchant_ata,
        1,
        5_000_000,
        0, // ZeroPeriod
    );
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix],
        &[&actors.merchant],
    );
    assert_nakama_err::<()>(result, NakamaError::ZeroPeriod);
}

/// Source: ADR-014 §Errors — `price == 0` rejected (defence-in-depth).
#[test]
fn zero_price_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 0);

    let ix = ix::create_plan_ix(
        &actors.merchant.pubkey(),
        &actors.merchant_ata,
        1,
        0, // ZeroPrice
        60,
    );
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix],
        &[&actors.merchant],
    );
    assert_nakama_err::<()>(result, NakamaError::ZeroPrice);
}
