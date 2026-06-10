//! Compute-unit + account-size benchmark — feeds the README §Benchmarks
//! section.
//!
//! Measured on LiteSVM 0.10.0 (deterministic, in-process sBPF VM — no
//! network, no validator, no wall-clock). Every figure here is reproducible
//! bit-for-bit from a clean `anchor build` because LiteSVM replays the same
//! program `.so` against a fixed clock we set explicitly.
//!
//! Background: ADR-015 §F4 ("Rate truncation") discusses the per-charge CU
//! budget — the F4 lazy-precise-unlock math adds ~+50 CU vs the old
//! truncating math, with the charge happy path measured at ~18 050 CU,
//! "well under the 200k limit and within the 100k self-imposed budget".
//! This test pins that discussion empirically across ALL 11 instructions so
//! a regression that blows the per-ix CU budget fails CI loosely (we assert
//! `0 < cu < 200_000`, the Solana per-ix compute limit) rather than flakily.
//!
//! Output contract (parseable, one line per row):
//!   `CU_BENCH   <instruction> <cu>`
//!   `SIZE_BENCH <account> <bytes> <rent_lamports>`
//! Harvest the table with:
//!   `cargo test --test cu_benchmarks -- --nocapture | grep -E 'CU_BENCH|SIZE_BENCH'`
//!
//! Black-box: drives only the public ABI via the shared `common::ix`
//! builders. No `nakama::state` / `nakama::instructions` introspection — we
//! read raw `account.data.len()` and `account.lamports` off-chain, exactly
//! like an indexer would.

mod common;

use common::{
    clock, fund_actors, grace_pda, ix, paused_sub_pda, pay_session_pda, plan_pda, send_tx, setup,
    subscription_pda, token_program_id, vault_pda, Signer,
};

const T0: i64 = 1_700_000_000;
const PLAN_PRICE: u64 = 600;
const PLAN_PERIOD: i64 = 60;
/// Solana per-instruction compute limit. CU consumed must stay strictly
/// below this; we assert it as the loose upper regression bound.
const PER_IX_CU_LIMIT: u64 = 200_000;

fn record_cu(label: &str, cu: u64) {
    assert!(cu > 0, "{label}: compute_units_consumed must be > 0");
    assert!(
        cu < PER_IX_CU_LIMIT,
        "{label}: {cu} CU exceeds the {PER_IX_CU_LIMIT} per-ix budget; escalate \
         [ADR_DRIFT] per ADR-015 §F4"
    );
    println!("CU_BENCH   {label} {cu}");
}

fn record_size(svm: &litesvm::LiteSVM, label: &str, address: &solana_pubkey::Pubkey) {
    let acct = svm
        .get_account(address)
        .unwrap_or_else(|| panic!("{label}: account {address} must exist to size it"));
    let bytes = acct.data.len();
    let rent = acct.lamports;
    assert!(bytes > 0, "{label}: account data must be non-empty");
    assert!(rent > 0, "{label}: account must be rent-funded");
    println!("SIZE_BENCH {label} {bytes} {rent}");
}

/// Single `#[test]`: one full lifecycle through all 11 instructions for the
/// CU table + most account sizes, then a short second scenario that drives a
/// subscription to GracePeriod so we can size the `GracedSubscription`
/// satellite (unreachable in the main lifecycle without exhausting the
/// stream, which would block the pause/resume/cancel steps).
#[test]
fn cu_and_size_benchmarks() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 10_000_000);

    let plan_id = 1u64;
    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);

    // ── 1. create_plan ──
    let meta = send_tx(
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
    record_cu("create_plan", meta.compute_units_consumed);
    record_size(&env.svm, "Plan", &plan_pk);

    // ── 2. subscribe (prefund 4 periods so later charges never exhaust) ──
    clock::set_clock(&mut env.svm, T0);
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::subscribe_ix(
            &actors.subscriber.pubkey(),
            &plan_pk,
            &actors.subscriber_ata,
            4,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");
    record_cu("subscribe", meta.compute_units_consumed);
    record_size(&env.svm, "Subscription", &sub_pk);
    record_size(&env.svm, "vault_TokenAccount", &vault_pk);

    // ── 3. top_up (from Active; satellite absent) ──
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::top_up_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.subscriber_ata,
            PLAN_PRICE,
        )],
        &[&actors.subscriber],
    )
    .expect("top_up");
    record_cu("top_up", meta.compute_units_consumed);

    // ── 4. charge (mid-stream — does NOT exhaust, stays Active) ──
    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper");
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD);
    env.svm.expire_blockhash();
    let meta = send_tx(
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
    .expect("charge");
    record_cu("charge", meta.compute_units_consumed);

    // ── 5. open_session ──
    let facilitator = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&facilitator.pubkey(), 5_000_000_000)
        .expect("airdrop facilitator");
    let session_id = 7u64;
    let (pay_session_pk, _) = pay_session_pda(&sub_pk, session_id);
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::open_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
            &facilitator.pubkey(),
            300,
        )],
        &[&actors.subscriber],
    )
    .expect("open_session");
    record_cu("open_session", meta.compute_units_consumed);
    record_size(&env.svm, "PaySession", &pay_session_pk);

    // ── 6. settle_usage (facilitator signs) ──
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD + 10);
    env.svm.expire_blockhash();
    let meta = send_tx(
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
    )
    .expect("settle_usage");
    record_cu("settle_usage", meta.compute_units_consumed);

    // ── 7. close_session ──
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::close_session_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            session_id,
        )],
        &[&actors.subscriber],
    )
    .expect("close_session");
    record_cu("close_session", meta.compute_units_consumed);

    // ── 8. pause (merchant signs; inits PausedSubscription satellite) ──
    let (paused_pk, _) = paused_sub_pda(&sub_pk);
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD + 20);
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");
    record_cu("pause", meta.compute_units_consumed);
    record_size(&env.svm, "PausedSubscription", &paused_pk);

    // ── 9. resume (closes the satellite, shifts stream_start) ──
    clock::set_clock(&mut env.svm, T0 + PLAN_PERIOD + 50);
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::resume_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("resume");
    record_cu("resume", meta.compute_units_consumed);

    // ── 10. cancel (subscriber signs; from Active → Cancelled) ──
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cancel_ix(
            &actors.subscriber.pubkey(),
            &sub_pk,
            &actors.merchant_ata,
            &actors.subscriber_ata,
        )],
        &[&actors.subscriber],
    )
    .expect("cancel");
    record_cu("cancel", meta.compute_units_consumed);

    // ── 11. cleanup (subscriber signs; closes the Subscription account) ──
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.subscriber,
        &[ix::cleanup_ix(&actors.subscriber.pubkey(), &sub_pk)],
        &[&actors.subscriber],
    )
    .expect("cleanup");
    record_cu("cleanup", meta.compute_units_consumed);

    // ── Second scenario: drive a fresh subscription into GracePeriod so we
    // can size the GracedSubscription satellite (charge-to-exhaustion path,
    // ADR-007 §I-CHARGE-1). Kept separate because exhausting the stream is
    // terminal for the streaming-math state and cannot coexist with the
    // pause/resume/cancel steps above. ──
    size_graced_subscription();
}

fn size_graced_subscription() {
    let mut env = setup();
    let actors = fund_actors(&mut env, 1_000_000);

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
    .expect("create_plan (grace scenario)");

    let (plan_pk, _) = plan_pda(&actors.merchant.pubkey(), plan_id);
    let (sub_pk, _) = subscription_pda(&actors.subscriber.pubkey(), &plan_pk);
    let (vault_pk, _) = vault_pda(&sub_pk);
    let (graced_pk, _) = grace_pda(&sub_pk);

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
    .expect("subscribe (grace scenario)");

    let keeper = solana_keypair::Keypair::new();
    env.svm
        .airdrop(&keeper.pubkey(), 5_000_000_000)
        .expect("airdrop keeper (grace scenario)");

    // Warp to stream_start + 2*period — exhausts the 2-period prefund, so the
    // charge tail flips state → GracePeriod and inits GracedSubscription.
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
            &token_program_id(),
            Some(graced_pk),
        )],
        &[&keeper],
    )
    .expect("charge into grace");

    record_size(&env.svm, "GracedSubscription", &graced_pk);
}
