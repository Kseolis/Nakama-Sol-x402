//! ADR-005 — Variable Rate via Composition (off-chain helper, zero on-chain change).
//!
//! Source: `docs/architecture/adr-005-variable-rate.md` (Accepted, post-hackathon).
//!
//! Methodology: black-box LiteSVM. The ADR explicitly states "On-chain changes:
//! ZERO." Therefore these tests do NOT assert any new on-chain symbol; instead
//! they prove the **composite-tx outcome** that the SDK helpers
//! (`crates/nakama-client/src/change_rate.rs`,
//! `clients/ts/src/instructions/changeRate.ts`) rely on. Every ix used here is
//! an existing primitive (`cancel`, `cleanup`, `subscribe`, `close_session`,
//! `top_up`, `charge`) — ADR-005 is a composition of these primitives, nothing
//! more.
//!
//! ## ADR-005 invariant coverage matrix
//!
//! | ADR-005 reference | Invariant | Test |
//! |---|---|---|
//! | §Decision step 2 (canonical) | composite `[cancel, cleanup, subscribe(plan_v2)]` atomic → fresh Active sub on plan_v2 with `rate_per_second_v2` snapshot; old PDA closed | `migrate_active_no_satellites_happy_path` |
//! | §Decision "Plan layout unchanged" | Plan v1 and Plan v2 account bytes unaffected by migration tx | `migrate_active_no_satellites_happy_path` (subassert) |
//! | §Q7 + ADR-x402-001 §R1 closure | alive PaySession satellites closed in composite prefix | `migrate_with_alive_paysessions_happy` |
//! | §Q8 GracePeriod row | migration from GracePeriod technically works; refund == 0 (economically odd, structurally allowed) | `migrate_from_graceperiod_happy` |
//! | §Q5 boundary (same-mint only) | cross-mint subscribe(plan_v2) inside composite rejected on-chain | `cross_mint_migration_rejected` |
//! | §E2 atomicity / §Q4 Solana tx atomicity | partial failure in composite reverts entirely — old sub survives | `composite_tx_partial_failure_atomic_revert` |
//! | §Q1 + ADR-013 §Q1 | merchant cannot force-migrate — cleanup is subscriber-only signer | `merchant_force_migrate_blocked` |
//! | §Q8 Cancelled row + ADR-008 | from Cancelled tombstone → `[cleanup, subscribe(plan_v2)]` (no cancel ix needed) | `migrate_from_cancelled_falls_back_to_fresh_subscribe` |
//! | §Q4 tx-size 1232-byte envelope | composite with N=4 PaySessions fits envelope and succeeds | `migrate_with_4_paysessions_at_envelope_limit` |
//!
//! Forward-deferred (per ADR-005 §"Implementation impact" `migrate_paused_to_active.rs`
//! and `migrate_concurrent_with_merchant_cancel.rs`): the Paused-source migration
//! and the merchant-cancel race scenarios. Paused requires the trailing optional
//! PausedSubscription slot inside the cancel ix; the helper for that path
//! (`cancel_ix_with_paused`) exists in `common::ix` but is exercised by
//! `adr006_cancel_from_paused.rs`. Adding it here would duplicate that file's
//! coverage; ADR-005 Q8 explicitly inherits the Paused cancel semantics from
//! ADR-006/013 unchanged. The concurrency race test is a runtime/RPC-ordering
//! property that LiteSVM (single-threaded SVM, no mempool) cannot reproduce.
//!
//! Layout offsets used (sourced from `tests/state_layout.rs` and the
//! `STATE_OFFSET = 192` comment in `common/mod.rs`):
//!
//! - `price`  at byte 80  (u64 LE), inside `Subscription.data`.
//! - `period` at byte 88  (i64 LE).
//! - `state`  at byte 192 (u8).
//!
//! These are observable via `read_account_data` — black-box.

mod common;

use common::{
    clock,
    error::{anchor_codes, assert_nakama_err, extract_custom_code, NakamaError},
    fund_actors, install_funded_ata, ix, pay_session_pda, plan_pda, send_tx, setup,
    subscription_pda, token_balance, vault_pda, Signer, STATE_OFFSET,
};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;

// Common test constants. Plan v1 vs v2 have *different* (price, period) so we
// can prove the rate snapshot differs between the two Subscription accounts.
const T0: i64 = 1_700_000_000;

const PLAN_V1_ID: u64 = 1;
const PLAN_V1_PRICE: u64 = 600;
const PLAN_V1_PERIOD: i64 = 60;

const PLAN_V2_ID: u64 = 2;
const PLAN_V2_PRICE: u64 = 1_500; // 2.5× — simulates inflation / pricing-tier change
const PLAN_V2_PERIOD: i64 = 60;

// Subscription layout offsets (ADR-001 revised; cross-checked against
// `state_layout.rs::subscription_account_layout_offsets`).
const SUB_PRICE_OFFSET: usize = 80;
const SUB_PERIOD_OFFSET: usize = 88;

// State byte values per ADR-003 FSM (verified in cancel_from_grace.rs and
// resubscribe.rs).
const STATE_ACTIVE: u8 = 0;
const STATE_GRACE: u8 = 2;
const STATE_CANCELLED: u8 = 4;

// ── Fixture helpers ────────────────────────────────────────────────────────

/// Create Plan v1 and Plan v2 (same merchant, same mint). Returns
/// `(plan_v1_pk, plan_v2_pk)`. Both plans use the same `merchant_ata`.
fn create_two_plans(env: &mut common::TestEnv, actors: &common::Actors) -> (Pubkey, Pubkey) {
    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            PLAN_V1_ID,
            PLAN_V1_PRICE,
            PLAN_V1_PERIOD,
        )],
        &[&actors.merchant],
    )
    .expect("create plan_v1");

    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            PLAN_V2_ID,
            PLAN_V2_PRICE,
            PLAN_V2_PERIOD,
        )],
        &[&actors.merchant],
    )
    .expect("create plan_v2");

    let (plan_v1, _) = plan_pda(&actors.merchant.pubkey(), PLAN_V1_ID);
    let (plan_v2, _) = plan_pda(&actors.merchant.pubkey(), PLAN_V2_ID);
    (plan_v1, plan_v2)
}

/// Subscribe to `plan_v1` with `periods_to_prefund` periods of deposit.
/// Returns `sub_v1_pk`. Clock set to T0 just before subscribe so the stream
/// starts at a known anchor.
fn subscribe_to_plan_v1(
    env: &mut common::TestEnv,
    actors: &common::Actors,
    plan_v1: &Pubkey,
    periods_to_prefund: u8,
) -> Pubkey {
    clock::set_clock(&mut env.svm, T0);
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            plan_v1,
            &actors.subscriber_ata,
            periods_to_prefund,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe plan_v1");

    let (sub_v1, _) = subscription_pda(&actors.subscriber.pubkey(), plan_v1);
    sub_v1
}

/// Snapshot the byte-equal Plan account data so we can prove ADR-005's
/// "Plan layout immutable" invariant later. Returns Plan v1's `account.data`
/// before the migration tx.
fn snapshot_plan_data(svm: &litesvm::LiteSVM, plan: &Pubkey) -> Vec<u8> {
    svm.get_account(plan).expect("plan account must exist").data
}

// ── §Decision step 2 — happy path, no satellites ─────────────────────────

/// ADR-005 §Decision step 2 (canonical):
/// > tx = [cancel(old_sub), cleanup(old_sub), subscribe(plan_v2, ...)]
///
/// Active subscription on Plan v1 → composite `[cancel, cleanup, subscribe(plan_v2)]`
/// commits atomically. Post-state:
/// - old sub PDA closed (rent returned to subscriber);
/// - new sub PDA initialised at the deterministic `(subscriber, plan_v2)` seeds
///   with state == Active and `price` snapshot == PLAN_V2_PRICE (the migration
///   actually changed the on-chain rate snapshot, not just the seed pubkey);
/// - Plan v1 and Plan v2 account bytes byte-equal pre/post (ADR-005 §Decision
///   "Plan layout immutable" + Q9 rejected alternative F).
#[test]
fn migrate_active_no_satellites_happy_path() {
    let mut env = setup();
    // Subscriber needs >=2 periods of plan_v1 (initial deposit) +1 period of
    // plan_v2 (post-migration deposit). 2*600 + 1500 = 2700 < 10_000_000.
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 2);

    // Snapshot Plan accounts pre-migration for byte-equality assertion.
    let plan_v1_data_pre = snapshot_plan_data(&env.svm, &plan_v1);
    let plan_v2_data_pre = snapshot_plan_data(&env.svm, &plan_v2);

    // Sanity: old sub stores price = PLAN_V1_PRICE.
    let sub_v1_data_pre = env.svm.get_account(&sub_v1_pk).expect("alive").data;
    let v1_price_observed = u64::from_le_bytes(
        sub_v1_data_pre[SUB_PRICE_OFFSET..SUB_PRICE_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        v1_price_observed, PLAN_V1_PRICE,
        "precondition: old subscription snapshots plan_v1 price"
    );

    // Advance clock 30s into the period (some streaming has accrued).
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();

    // ── Composite tx: cancel + cleanup + subscribe(plan_v2). ──
    let composite = [
        ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        ),
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            1,
        ),
    ];
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("composite [cancel, cleanup, subscribe(plan_v2)] must succeed atomically");

    // ── Post 1: old sub PDA closed (rent reclaimed). ──
    match env.svm.get_account(&sub_v1_pk) {
        None => {}
        Some(a) => assert_eq!(
            a.lamports, 0,
            "old subscription PDA must be closed after cleanup"
        ),
    }

    // ── Post 2: new sub PDA initialised at the canonical (subscriber, plan_v2) seeds. ──
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    assert_ne!(
        sub_v1_pk, sub_v2_pk,
        "ADR-001 seeds invariant: different plan ⇒ different Subscription PDA"
    );
    let sub_v2_data = env
        .svm
        .get_account(&sub_v2_pk)
        .expect("plan_v2 subscription must be initialised by composite tx")
        .data;
    assert_eq!(
        sub_v2_data[STATE_OFFSET], STATE_ACTIVE,
        "new subscription state byte must be Active (=0)"
    );

    // ── Post 3: new sub's price snapshot == PLAN_V2_PRICE. ──
    // Critical assertion: this is the load-bearing observable of ADR-005 — the
    // composite tx actually changed the rate-per-second snapshot.
    let v2_price_observed = u64::from_le_bytes(
        sub_v2_data[SUB_PRICE_OFFSET..SUB_PRICE_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        v2_price_observed, PLAN_V2_PRICE,
        "ADR-005 invariant: new subscription must snapshot plan_v2 rate, not plan_v1"
    );
    assert_ne!(
        v2_price_observed, PLAN_V1_PRICE,
        "rate change demonstrably occurred (v2 != v1)"
    );
    let v2_period_observed = i64::from_le_bytes(
        sub_v2_data[SUB_PERIOD_OFFSET..SUB_PERIOD_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(v2_period_observed, PLAN_V2_PERIOD);

    // ── Post 4: Plan v1 and Plan v2 account bytes unchanged by migration. ──
    // ADR-005 §Decision "Plan layout: unchanged" + §Q9-F rejected alternative
    // ("no Plan-versioning lineage on-chain").
    let plan_v1_data_post = snapshot_plan_data(&env.svm, &plan_v1);
    let plan_v2_data_post = snapshot_plan_data(&env.svm, &plan_v2);
    assert_eq!(
        plan_v1_data_pre, plan_v1_data_post,
        "Plan v1 account bytes must be untouched by migration (ADR-001 immutability)"
    );
    assert_eq!(
        plan_v2_data_pre, plan_v2_data_post,
        "Plan v2 account bytes must be untouched by migration"
    );

    // ── Post 5: new vault holds 1 × PLAN_V2_PRICE prefund. ──
    let (vault_v2_pk, _) = vault_pda(&sub_v2_pk);
    assert_eq!(
        token_balance(&env.svm, &vault_v2_pk),
        PLAN_V2_PRICE,
        "new vault must hold exactly 1 period of plan_v2 prefund"
    );
}

// ── §Q7 — composite with alive PaySession satellites ─────────────────────

/// ADR-005 §Q7 — when migrating with alive x402 PaySession satellites, the
/// composite tx MUST prefix N `close_session` instructions before
/// `cancel/cleanup/subscribe`. After commit, every satellite is closed and
/// the new Subscription has zero PaySessions (clean state per ADR-005).
#[test]
fn migrate_with_alive_paysessions_happy() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 4);

    // Open two PaySessions on the Active plan_v1 subscription.
    let facilitator = Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 1_000_000_000)
        .expect("airdrop facilitator");

    let session_a: u64 = 0xAA;
    let session_b: u64 = 0xBB;
    let (pay_a_pk, _) = pay_session_pda(&sub_v1_pk, session_a);
    let (pay_b_pk, _) = pay_session_pda(&sub_v1_pk, session_b);

    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[
            ix::open_session_ix(
                &actors.subscriber.pubkey(),
                &sub_v1_pk,
                session_a,
                &facilitator.pubkey(),
                100,
            ),
            ix::open_session_ix(
                &actors.subscriber.pubkey(),
                &sub_v1_pk,
                session_b,
                &facilitator.pubkey(),
                100,
            ),
        ],
        &[&actors.subscriber],
    )
    .expect("open_session × 2");

    assert!(env.svm.get_account(&pay_a_pk).is_some());
    assert!(env.svm.get_account(&pay_b_pk).is_some());

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();

    // Composite per ADR-005 §Q7 prescription.
    let composite = [
        ix::close_session_ix(&actors.subscriber.pubkey(), &sub_v1_pk, session_a),
        ix::close_session_ix(&actors.subscriber.pubkey(), &sub_v1_pk, session_b),
        ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        ),
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            1,
        ),
    ];
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("5-ix composite must succeed atomically (ADR-005 §Q7)");

    // PaySession satellites closed.
    for (label, pk) in [("session_a", pay_a_pk), ("session_b", pay_b_pk)] {
        match env.svm.get_account(&pk) {
            None => {}
            Some(a) => assert_eq!(
                a.lamports, 0,
                "{} must be closed by close_session in composite",
                label
            ),
        }
    }

    // New subscription Active on plan_v2.
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    let sub_v2 = env.svm.get_account(&sub_v2_pk).expect("plan_v2 sub alive");
    assert_eq!(sub_v2.data[STATE_OFFSET], STATE_ACTIVE);
}

// ── §Q8 GracePeriod row — migration from Grace ───────────────────────────

/// ADR-005 §Q8 GracePeriod row — "technically possible, economically nonsense"
/// (escrow already exhausted → refund == 0). ADR-005 explicitly documents this
/// as a structurally allowed path: composite tx still works, subscriber simply
/// pays the full new deposit on plan_v2 with no offset from old escrow.
#[test]
fn migrate_from_graceperiod_happy() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);

    // Subscribe with 1 period prefund so the stream exhausts within one period.
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 1);
    let (vault_v1_pk, _) = vault_pda(&sub_v1_pk);
    let (graced_v1_pk, _) = common::grace_pda(&sub_v1_pk);

    // Drive into Grace via charge tail at T0 + PLAN_V1_PERIOD.
    let keeper = Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 1_000_000_000)
        .expect("airdrop keeper");
    clock::set_clock(&mut env.svm, T0 + PLAN_V1_PERIOD);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &keeper,
        &[ix::charge_ix_full(
            &sub_v1_pk,
            &plan_v1,
            &vault_v1_pk,
            &actors.merchant_ata,
            &keeper.pubkey(),
            &common::token_program_id(),
            Some(graced_v1_pk),
        )],
        &[&keeper],
    )
    .expect("charge tail into Grace");

    // Sanity: state == GracePeriod and Grace satellite alive.
    let pre = env.svm.get_account(&sub_v1_pk).expect("alive");
    assert_eq!(pre.data[STATE_OFFSET], STATE_GRACE);
    assert!(env.svm.get_account(&graced_v1_pk).is_some());

    let pre_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);

    // Stay within grace window so cancel takes the pre-expiry branch.
    clock::set_clock(&mut env.svm, T0 + PLAN_V1_PERIOD + 100);
    env.svm.expire_blockhash();

    // Composite: cancel passes the Grace satellite via the full builder;
    // cleanup + subscribe(plan_v2) follow the canonical shape.
    let composite = [
        ix::cancel_ix_full(
            &actors.subscriber.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            None,
            &actors.merchant_ata,
            &actors.subscriber_ata,
            Some(graced_v1_pk),
        ),
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            1,
        ),
    ];
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("composite from GracePeriod must succeed (ADR-005 §Q8 Grace row)");

    // Grace satellite closed; old sub PDA closed.
    match env.svm.get_account(&graced_v1_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0),
    }
    match env.svm.get_account(&sub_v1_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0),
    }

    // ── Economic invariant: refund from cancel == 0 (stream exhausted at
    // grace entry; deposited == withdrawn). The only outflow from subscriber's
    // ATA is the new deposit of PLAN_V2_PRICE.
    let post_subscriber_usdc = token_balance(&env.svm, &actors.subscriber_ata);
    assert_eq!(
        pre_subscriber_usdc - post_subscriber_usdc,
        PLAN_V2_PRICE,
        "ADR-005 §Q8 Grace row: refund == 0; subscriber pays full plan_v2 deposit"
    );

    // New subscription on plan_v2 fresh-Active.
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    let sub_v2 = env.svm.get_account(&sub_v2_pk).expect("plan_v2 alive");
    assert_eq!(sub_v2.data[STATE_OFFSET], STATE_ACTIVE);
}

// ── §Q5 — cross-mint migration boundary ──────────────────────────────────

/// ADR-005 §Q5 — "ADR-005 covers ONLY same-mint rate changes. SDK helper
/// rejects if plan_v1.token_mint != plan_v2.token_mint."
///
/// In the current deploy, the ADR-005 boundary is in fact enforced **one layer
/// deeper** than the SDK helper: ADR-014's `create_plan` instruction pins
/// `token_mint` via `#[account(address = USDC_MINT)]`. Therefore *every* Plan
/// on chain has the same mint, and cross-mint migration is structurally
/// impossible — the SDK helper's pre-flight check is vacuously satisfied.
///
/// This test proves the **load-bearing on-chain constraint** that makes
/// ADR-005 §Q5 a free lunch: an attempt to create plan_v2 on a different mint
/// is rejected at `create_plan` time with `CONSTRAINT_ADDRESS` (Anchor 2012).
/// If a future ADR ever loosens `create_plan` to accept other mints (e.g.
/// PYUSD per Token-2022 future-work), ADR-005's same-mint boundary will need
/// to be re-enforced inside `subscribe` and at the SDK helper level — and
/// this test must flip into a multi-mint composite-tx check.
///
/// Reference: `tests/adversarial.rs::create_plan_with_token_2022_mint_rejected`
/// covers the Token-2022 ownership-mismatch path; this test exercises the
/// SAME constraint via a classic-SPL mint at a non-USDC address.
#[test]
fn cross_mint_migration_rejected() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    // Install a SECOND classic-SPL mint at a fresh address (simulates PYUSD).
    let other_mint = Keypair::new().pubkey();
    common::install_mint(&mut env.svm, &other_mint, &env.mint_authority.pubkey(), 6);

    // Merchant ATA on the other mint — what a real PYUSD merchant would hold.
    let merchant_other_ata =
        install_funded_ata(&mut env.svm, &actors.merchant.pubkey(), &other_mint, 0);

    // Attempt to create plan_v2 on the non-USDC mint. Per ADR-014's
    // `address = USDC_MINT` constraint, the program must reject this with
    // Anchor's `CONSTRAINT_ADDRESS` (2012).
    let result = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix_with_mint(
            &actors.merchant.pubkey(),
            &merchant_other_ata,
            &other_mint,
            PLAN_V2_ID,
            PLAN_V2_PRICE,
            PLAN_V2_PERIOD,
        )],
        &[&actors.merchant],
    );

    let meta = match result {
        Ok(_) => panic!("ADR-005 §Q5 / ADR-014: create_plan on a non-USDC mint must be rejected"),
        Err(m) => m,
    };
    let code = extract_custom_code(&meta).unwrap_or_else(|| {
        panic!(
            "expected Custom(CONSTRAINT_ADDRESS), got non-Custom: {:?}",
            meta.err
        )
    });
    assert_eq!(
        code,
        anchor_codes::CONSTRAINT_ADDRESS,
        "expected Anchor CONSTRAINT_ADDRESS ({}), got {} — cross-mint Plan creation must be rejected at create_plan",
        anchor_codes::CONSTRAINT_ADDRESS,
        code
    );

    // ── Structural assertion: since no second-mint Plan can be created, the
    // ADR-005 cross-mint migration scenario cannot be constructed. The SDK
    // helper's pre-flight check is therefore vacuously satisfied. ──
    let (plan_v2_other, _) = plan_pda(&actors.merchant.pubkey(), PLAN_V2_ID);
    assert!(
        env.svm.get_account(&plan_v2_other).is_none(),
        "non-USDC Plan PDA must not exist after rejected create_plan"
    );
}

// ── §E2 — composite partial failure reverts atomically ───────────────────

/// ADR-005 §E2 + §Q4 (Solana atomicity).
///
/// Build a composite where the FINAL ix (`subscribe(plan_v2)`) is guaranteed
/// to fail (`periods_to_prefund = 0` triggers `ZeroPeriodsToFund`). The runtime
/// must revert `cancel` and `cleanup` along with it. Post-state: the old
/// Subscription is byte-equal to its pre-tx state.
///
/// Mirrors `resubscribe.rs::composite_subscribe_failure_reverts_atomic` but
/// against the full 3-ix ADR-005 composite (cancel + cleanup + subscribe).
#[test]
fn composite_tx_partial_failure_atomic_revert() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 2);

    let pre_data = env.svm.get_account(&sub_v1_pk).expect("alive").data.clone();
    let pre_lamports = env.svm.get_account(&sub_v1_pk).unwrap().lamports;
    let (vault_v1_pk, _) = vault_pda(&sub_v1_pk);
    let pre_vault_usdc = token_balance(&env.svm, &vault_v1_pk);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();

    let composite = [
        ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        ),
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            0, // ZeroPeriodsToFund → tx-wide revert
        ),
    ];

    let result = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    );
    assert_nakama_err::<()>(result, NakamaError::ZeroPeriodsToFund);

    // ── Old subscription byte-equal pre/post. ──
    let post = env.svm.get_account(&sub_v1_pk).expect("must survive");
    assert_eq!(
        post.data, pre_data,
        "ADR-005 §E2: subscription account bytes must be unchanged on tx revert"
    );
    assert_eq!(
        post.lamports, pre_lamports,
        "subscription lamports must be unchanged (cancel + cleanup reverted)"
    );

    // ── Vault preserved. ──
    assert_eq!(
        token_balance(&env.svm, &vault_v1_pk),
        pre_vault_usdc,
        "vault USDC must be unchanged (cancel CPI reverted)"
    );

    // ── plan_v2 subscription PDA must NOT exist. ──
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    assert!(
        env.svm.get_account(&sub_v2_pk).is_none(),
        "plan_v2 Subscription PDA must NOT be initialised on reverted tx"
    );
}

// ── §Q1 — merchant cannot force-migrate ──────────────────────────────────

/// ADR-005 §Q1: "Merchant has zero ability to forcibly migrate."
/// ADR-013 §Q1: "cleanup is subscriber-only signer."
///
/// We construct a composite where the MERCHANT signs everything, attempting to
/// drag the subscriber onto plan_v2. The composite has two layers of defence:
/// 1. cancel ix is polymorphic per ADR-009 (merchant CAN cancel), so this step
///    PASSES.
/// 2. cleanup is subscriber-only (ADR-013 has_one); signed by merchant it must
///    fail with either `UnauthorizedCleanup` or Anchor `ConstraintHasOne`.
/// 3. Even if cleanup passed (it won't), subscribe is signed by merchant, so
///    Anchor's `Signer<'info>` on the `subscriber` slot would reject — but the
///    test does not need to reach that step.
///
/// We accept either `UnauthorizedCleanup` (Nakama 6015) or `ConstraintHasOne`
/// (Anchor 2001) since `cleanup_invariants.rs::cleanup_unauthorized_signer_rejected`
/// documents both as correct outcomes for this guard.
#[test]
fn merchant_force_migrate_blocked() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 2);

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();

    // Merchant signs the entire composite. The subscribe ix has subscriber as
    // the AccountMeta signer slot — that mismatch alone would also raise an
    // error; we want cleanup's signer guard to fire first (it's the second ix).
    // To prevent the tx-level "missing signature" error from blocking us, we
    // sign with BOTH merchant and subscriber, but supply the merchant as the
    // cleanup signer pubkey. Per the cleanup builder, the merchant goes into
    // the AccountMeta with is_signer=true; the program then compares it
    // against the snapshotted subscription.subscriber and rejects.
    let composite = [
        ix::cancel_ix_by_merchant(
            &actors.merchant.pubkey(),
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        ),
        ix::cleanup_ix_with_signer(
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant.pubkey(),
        ),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            1,
        ),
    ];

    // Provide both signatures so the runtime doesn't reject early on a
    // missing-signature error. The on-chain guard still fires.
    let result = send_tx(
        &mut env.svm,
        &actors.merchant, // fee payer
        &composite,
        &[&actors.merchant, &actors.subscriber],
    );

    let meta = match result {
        Ok(_) => panic!("merchant-forced migration must fail (ADR-005 §Q1)"),
        Err(m) => m,
    };
    let code = extract_custom_code(&meta).unwrap_or_else(|| {
        panic!(
            "expected Custom(UnauthorizedCleanup or ConstraintHasOne), got non-Custom: {:?}",
            meta.err
        )
    });
    let unauthorized = NakamaError::UnauthorizedCleanup.code();
    assert!(
        code == unauthorized || code == anchor_codes::CONSTRAINT_HAS_ONE,
        "expected UnauthorizedCleanup ({}) or ConstraintHasOne ({}), got {}",
        unauthorized,
        anchor_codes::CONSTRAINT_HAS_ONE,
        code
    );

    // ── Old subscription survives intact (atomicity); plan_v2 sub never inited. ──
    let post = env.svm.get_account(&sub_v1_pk).expect("must survive");
    assert_eq!(
        post.data[STATE_OFFSET], STATE_ACTIVE,
        "old subscription must remain Active after blocked force-migrate"
    );
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    assert!(
        env.svm.get_account(&sub_v2_pk).is_none(),
        "plan_v2 subscription must NOT be initialised by blocked force-migrate"
    );
}

// ── §Q8 Cancelled row — fall back to ADR-008 [cleanup, subscribe] shape ──

/// ADR-005 §Q8 Cancelled row + ADR-008 §Q11.
///
/// When the old subscription is already in Cancelled (dormant tombstone),
/// the composite collapses to `[cleanup, subscribe(plan_v2)]` — no cancel ix
/// needed. This is the exact ADR-008 re-subscribe pattern, just with a
/// different Plan pubkey. ADR-005 explicitly states this case "inherits
/// unchanged" from ADR-008.
///
/// Coverage: prove a Cancelled-state migration to a *different* plan_v2 works
/// via the 2-ix composite. (ADR-008's `composite_resubscribe_different_plan`
/// already covers same-plan resubscribe; this is the ADR-005-specific
/// variant: rate actually changes.)
#[test]
fn migrate_from_cancelled_falls_back_to_fresh_subscribe() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 2);

    // Drive to Cancelled.
    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_v1_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    )
    .expect("cancel to Cancelled tombstone");
    let pre = env.svm.get_account(&sub_v1_pk).expect("alive");
    assert_eq!(
        pre.data[STATE_OFFSET], STATE_CANCELLED,
        "precondition: tombstone in Cancelled state"
    );

    // The ADR-005 Q8 Cancelled row composite — 2 ix, NO cancel.
    clock::set_clock(&mut env.svm, T0 + 1_000);
    env.svm.expire_blockhash();
    let composite = [
        ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk),
        ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_v2,
            &actors.subscriber_ata,
            1,
        ),
    ];
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &composite,
        &[&actors.subscriber],
    )
    .expect("Cancelled-source migration must succeed via [cleanup, subscribe(plan_v2)]");

    // Old PDA closed.
    match env.svm.get_account(&sub_v1_pk) {
        None => {}
        Some(a) => assert_eq!(a.lamports, 0),
    }

    // New PDA fresh-Active with plan_v2 rate snapshot.
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    let sub_v2 = env.svm.get_account(&sub_v2_pk).expect("plan_v2 alive");
    assert_eq!(sub_v2.data[STATE_OFFSET], STATE_ACTIVE);
    let v2_price = u64::from_le_bytes(
        sub_v2.data[SUB_PRICE_OFFSET..SUB_PRICE_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        v2_price, PLAN_V2_PRICE,
        "Cancelled-row migration must still snapshot plan_v2 rate (not plan_v1)"
    );
}

// ── §Q4 tx-size — N=4 PaySessions at envelope limit ──────────────────────

/// ADR-005 §Q4 — "Tx fits up to ~5 active satellites" before approaching the
/// 1232-byte limit. The Rust SDK helper caps at N=4 (the test scope note in
/// the user prompt). This test asserts the canonical 4-satellite composite
/// (4 × close_session + cancel + cleanup + subscribe = 7 ix total) both
/// SERIALIZES under 1232 bytes AND commits on-chain.
///
/// Why both checks: tx-size check protects the SDK helper's envelope budget;
/// on-chain success proves Solana runtime doesn't truncate or partial-commit
/// large composites.
#[test]
fn migrate_with_4_paysessions_at_envelope_limit() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);
    let (plan_v1, plan_v2) = create_two_plans(&mut env, &actors);
    // 5 periods of plan_v1 so opening 4 sessions doesn't deplete reservation.
    let sub_v1_pk = subscribe_to_plan_v1(&mut env, &actors, &plan_v1, 5);

    let facilitator = Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 1_000_000_000)
        .expect("airdrop facilitator");

    // Open 4 sessions in a single tx to mirror SDK helper assumptions.
    let session_ids: [u64; 4] = [0x01, 0x02, 0x03, 0x04];
    let open_ixs: Vec<solana_instruction::Instruction> = session_ids
        .iter()
        .map(|&id| {
            ix::open_session_ix(
                &actors.subscriber.pubkey(),
                &sub_v1_pk,
                id,
                &facilitator.pubkey(),
                100,
            )
        })
        .collect();
    env.svm.expire_blockhash();
    send_tx(
        &mut env.svm,
        &actors.subscriber,
        &open_ixs,
        &[&actors.subscriber],
    )
    .expect("open 4 PaySessions");

    // Verify all 4 satellites are alive.
    for id in session_ids {
        let (pk, _) = pay_session_pda(&sub_v1_pk, id);
        assert!(
            env.svm.get_account(&pk).is_some(),
            "PaySession {} must be alive pre-migration",
            id
        );
    }

    clock::set_clock(&mut env.svm, T0 + 30);
    env.svm.expire_blockhash();

    // Build the full 7-ix composite per ADR-005 §Q4 with N=4 satellites.
    let mut composite: Vec<solana_instruction::Instruction> = session_ids
        .iter()
        .map(|&id| ix::close_session_ix(&actors.subscriber.pubkey(), &sub_v1_pk, id))
        .collect();
    composite.push(ix::cancel_ix(
        &actors.subscriber.pubkey(),
        &sub_v1_pk,
        &actors.merchant_ata,
        &actors.subscriber_ata,
    ));
    composite.push(ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_v1_pk));
    composite.push(ix::subscribe_ix(
        &actors.subscriber.pubkey(),
        &plan_v2,
        &actors.subscriber_ata,
        1,
    ));
    assert_eq!(
        composite.len(),
        7,
        "expected 7-ix composite (4 × close_session + cancel + cleanup + subscribe)"
    );

    // ── Envelope-budget assertion: serialize the signed tx and assert < 1232. ──
    //
    // Wire size formula (legacy tx, Solana docs §"Transaction Wire Format"):
    //   shortvec(num_sigs)  +  64 × num_sigs  +  message.serialize().len()
    // For 1 signer: 1 + 64 + |message|. shortvec encodes a single-byte length
    // for any count ≤ 127 so the +1 is exact here.
    let blockhash = env.svm.latest_blockhash();
    let msg =
        Message::new_with_blockhash(&composite, Some(&actors.subscriber.pubkey()), &blockhash);
    let msg_bytes = msg.serialize();
    let wire_len = 1 + 64 + msg_bytes.len();
    assert!(
        wire_len <= 1232,
        "ADR-005 §Q4: 7-ix composite must fit in 1232-byte tx envelope, got {} bytes",
        wire_len
    );
    let tx = Transaction::new(&[&actors.subscriber], msg, blockhash);

    // ── Submit and assert atomic success. ──
    env.svm
        .send_transaction(tx)
        .expect("7-ix composite must commit (ADR-005 §Q4 envelope budget)");

    // All 4 satellites closed.
    for id in session_ids {
        let (pk, _) = pay_session_pda(&sub_v1_pk, id);
        match env.svm.get_account(&pk) {
            None => {}
            Some(a) => assert_eq!(a.lamports, 0, "PaySession {} must be closed", id),
        }
    }

    // New subscription on plan_v2 Active.
    let (sub_v2_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_v2);
    let sub_v2 = env.svm.get_account(&sub_v2_pk).expect("plan_v2 alive");
    assert_eq!(sub_v2.data[STATE_OFFSET], STATE_ACTIVE);
}
