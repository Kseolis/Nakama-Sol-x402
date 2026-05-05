//! Charge-tail auto-transition tests — ADR-007 §"charge handler tail".
//!
//! Black-box: written from ADR-007 §I-CHARGE-1/2/4 + §I-GRACE-1/2/5 +
//! §"Storage decision", NOT from `instructions/charge.rs` internals.
//!
//! Coverage:
//! - I-CHARGE-1: charge fully unlocking (`withdrawn_amount ==
//!   deposited_amount`) flips state to GracePeriod and inits
//!   GracedSubscription with `entered_grace_at == now`,
//!   `grace_until == now + GRACE_DURATION`. `GraceEntered` event implicit
//!   (we read post-state from accounts, which is the load-bearing surface).
//! - I-CHARGE-2: `grace_until - entered_grace_at == 604_800` (= I-CONST-1
//!   runtime cross-check).
//! - I-CHARGE-4: charge mid-stream (claimable < deposited) does NOT init
//!   the satellite — state stays Active, satellite account does not exist.
//! - I-GRACE-2: GracedSubscription on-chain account body length == 56
//!   (8 disc + 48 borsh).
//! - I-GRACE-5: rent payer = keeper (the `payer: Signer` on Charge). After
//!   charge-into-grace, keeper lamports decrease by ~rent for a 56-byte
//!   account + tx fees. We use a pre/post lamport delta with a generous
//!   floor since Anchor / Solana rent magnitude is version-sensitive.
//! - I-LAYOUT-2: `Subscription.reserved[32]` byte-equal `[0; 32]` post
//!   charge-into-grace.
//! - I-LAYOUT-3: `Subscription.vault_bump` byte-equal pre/post.

mod common;

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, token_balance, vault_pda,
    Signer, GRACE_DURATION, STATE_OFFSET,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
/// Byte offset of `vault_bump` inside Subscription account data
/// (8 disc + 184 pre-state + 1 state + 1 bump = 194).
const VAULT_BUMP_OFFSET: usize = 194;
/// Byte offset of `Subscription.reserved` (last 32 bytes).
const RESERVED_LEN: usize = 32;

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

/// Source: ADR-007 §I-CHARGE-1 + §I-CHARGE-2 + §I-GRACE-2 + §I-GRACE-5 +
/// §I-LAYOUT-2/3.
///
/// At t = T0 + 2*period the post-CPI math leaves
/// `withdrawn_amount == deposited_amount` → grace tail fires:
/// - state byte at offset 192 == 2 (GracePeriod).
/// - GracedSubscription PDA account exists with body length == 56 bytes
///   (I-GRACE-2: 8 disc + 48 borsh) and lamports > 0 (rent-exempt).
/// - GracedSubscription decode: `subscription` = sub_pk, `entered_grace_at`
///   = now, `grace_until` = now + 604_800.
/// - I-CONST-1 / I-CHARGE-2: `grace_until - entered_grace_at` == 604_800.
/// - I-LAYOUT-2: `Subscription.reserved` == [0; 32] byte-for-byte.
/// - I-LAYOUT-3: `vault_bump` byte-equal pre/post.
#[test]
fn charge_tail_enters_grace() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let pre_sub_data = env.svm.get_account(&sub_pk).expect("alive").data.clone();
    let pre_vault_bump = pre_sub_data[VAULT_BUMP_OFFSET];

    let (graced_pk, _) = common::grace_pda(&sub_pk);
    assert!(
        env.svm.get_account(&graced_pk).is_none(),
        "satellite must not exist before charge-tail"
    );

    let keeper = fresh_keeper(&mut env);

    // advance to T = stream_start + 2*period to fully exhaust the 2-period
    // prefund (deposited_amount == 2*price == rate * 2*period).
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
    .expect("charge tail enters grace");

    // ── Subscription post-state ────────────────────────────────────────
    let post_sub_data = env.svm.get_account(&sub_pk).expect("alive").data.clone();

    // I-CHARGE-1: state byte flipped to GracePeriod.
    assert_eq!(
        post_sub_data[STATE_OFFSET], 2,
        "I-CHARGE-1: state byte must be GracePeriod (=2) after exhausting charge"
    );

    // I-LAYOUT-3: vault_bump unchanged.
    assert_eq!(
        post_sub_data[VAULT_BUMP_OFFSET], pre_vault_bump,
        "I-LAYOUT-3: vault_bump must be byte-equal pre/post charge-tail"
    );

    // I-LAYOUT-2: reserved[32] is the trailing slot, byte-equal [0; 32].
    let post_reserved = &post_sub_data[post_sub_data.len() - RESERVED_LEN..];
    assert_eq!(
        post_reserved, &[0u8; RESERVED_LEN],
        "I-LAYOUT-2: Subscription.reserved must remain [0; 32] post charge-tail"
    );

    // ── GracedSubscription satellite ──────────────────────────────────
    let graced_acct = env
        .svm
        .get_account(&graced_pk)
        .expect("I-CHARGE-1: GracedSubscription must be inited by charge-tail");

    // I-GRACE-2: on-chain body length 56 (8 disc + 48 borsh).
    assert_eq!(
        graced_acct.data.len(),
        56,
        "I-GRACE-2: GracedSubscription body must be 56 bytes (8 disc + 48 borsh)"
    );
    assert!(graced_acct.lamports > 0, "satellite rent-exempt");

    // Decode body via Borsh-equivalent layout (after 8-byte disc):
    //   subscription: Pubkey [32] | entered_grace_at: i64 [8] | grace_until: i64 [8]
    let body = &graced_acct.data[8..];
    let satellite_subscription =
        solana_pubkey::Pubkey::try_from(&body[0..32]).expect("decode subscription pubkey");
    let entered_grace_at = i64::from_le_bytes(
        body[32..40]
            .try_into()
            .expect("8 bytes for entered_grace_at"),
    );
    let grace_until = i64::from_le_bytes(body[40..48].try_into().expect("8 bytes for grace_until"));

    // I-CHARGE-1: back-ref points at parent Subscription.
    assert_eq!(
        satellite_subscription, sub_pk,
        "satellite.subscription must match parent Subscription PDA"
    );
    // I-CHARGE-1: entered_grace_at == clock at the charge tx.
    assert_eq!(
        entered_grace_at, exhaust_at,
        "I-CHARGE-1: entered_grace_at must equal Clock::unix_timestamp at charge time"
    );
    // I-CHARGE-2 + I-CONST-1 runtime cross-check: grace_until is exactly +604_800.
    assert_eq!(
        grace_until - entered_grace_at,
        GRACE_DURATION,
        "I-CHARGE-2 + I-CONST-1: grace_until - entered_grace_at must equal 604_800"
    );
    assert_eq!(
        grace_until,
        exhaust_at + GRACE_DURATION,
        "I-CHARGE-2: grace_until must equal entered_grace_at + GRACE_DURATION"
    );
}

/// Source: ADR-007 §I-GRACE-5 — keeper (the `payer: Signer`) pays rent for
/// the satellite. We assert the keeper's lamport balance dropped after the
/// charge-into-grace tx by at least the rent floor for a 56-byte account.
///
/// Anchor / Solana rent magnitudes are version-sensitive; we use an
/// inclusive lower bound (rent for a 56-byte account ≈ 1 002 240 lamports;
/// we assert delta > 100k to keep the test robust to validator-side
/// rent-rate changes and tx fees).
#[test]
fn charge_tail_keeper_pays_rent() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = fresh_keeper(&mut env);
    let pre_keeper_lamports = env
        .svm
        .get_account(&keeper.pubkey())
        .expect("keeper exists")
        .lamports;

    let (graced_pk, _) = common::grace_pda(&sub_pk);

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
    .expect("charge tail enters grace");

    let post_keeper_lamports = env
        .svm
        .get_account(&keeper.pubkey())
        .expect("keeper alive")
        .lamports;
    assert!(
        post_keeper_lamports < pre_keeper_lamports,
        "I-GRACE-5: keeper lamports must decrease (rent + fees paid by keeper)"
    );

    // Lower-bound the delta: rent for 56 bytes is ~10^6 lamports on default
    // rent-rate. Assert at least 100k to absorb LiteSVM rent-table tweaks.
    let delta = pre_keeper_lamports - post_keeper_lamports;
    assert!(
        delta > 100_000,
        "I-GRACE-5: keeper lamport delta {} must be at least 100k (rent floor)",
        delta
    );

    // Sanity: satellite is rent-exempt.
    let graced_acct = env.svm.get_account(&graced_pk).expect("satellite alive");
    assert!(graced_acct.lamports > 0);
}

/// Source: ADR-007 §I-CHARGE-4 — charge that does NOT exhaust the stream
/// must NOT init the satellite.
///
/// Setup: at t = T0 + period (1 period worth unlocked of 2 prefunded),
/// claimable = 1 period × rate = 600. Post-charge `withdrawn_amount == 600
/// < deposited_amount == 1200` → no grace tail. Satellite must not exist
/// post-tx; state stays Active.
#[test]
fn charge_no_grace_when_funds_remain() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);
    let (plan_pk, sub_pk, vault_pk) = create_and_subscribe(&mut env, &actors);

    let keeper = fresh_keeper(&mut env);
    let (graced_pk, _) = common::grace_pda(&sub_pk);
    assert!(
        env.svm.get_account(&graced_pk).is_none(),
        "no satellite pre-charge"
    );

    // advance to T = stream_start + period (mid-stream, NOT exhausting).
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);

    send_tx(
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
    )
    .expect("charge mid-period");

    // I-CHARGE-4: state stays Active.
    let sub_acct = env.svm.get_account(&sub_pk).expect("alive");
    assert_eq!(
        sub_acct.data[STATE_OFFSET], 0,
        "I-CHARGE-4: state must remain Active when claimable < deposited"
    );

    // I-CHARGE-4: satellite NOT inited.
    assert!(
        env.svm.get_account(&graced_pk).is_none(),
        "I-CHARGE-4: GracedSubscription must NOT exist after non-exhausting charge"
    );

    // Sanity: vault has 1 period left.
    assert_eq!(
        token_balance(&env.svm, &vault_pk),
        PLAN_PRICE,
        "vault must hold deposited - claimable = 600 µUSDC"
    );
}
