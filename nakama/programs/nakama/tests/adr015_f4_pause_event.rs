//! ADR-015 §F4 mirror for `pause` — analytics-only `unlocked_at_pause` event
//! field must use the precise lazy-division math, not the truncated
//! `rate_per_second * elapsed` form.
//!
//! Source of truth: ADR-015 §F4 omitted `pause` from the enumerated scope by
//! oversight (security finding 1, Low). pre-fix: pause snapshotted
//! `rate_per_second * elapsed` → off-chain analytics under-counted merchant
//! earnings by `(price mod period) / period` per second (~22-28% on USDC
//! monthly plans). post-fix: pause uses `(elapsed * price) / period` — exact
//! to the base-unit, matches `charge` / `cancel` / `settle_usage`.
//!
//! The event is analytics-only (not stored anywhere on-chain, not consumed by
//! cancel-from-Paused math which freezes at `paused_at`), so this test
//! exercises ONLY the event payload — runtime invariants are already covered
//! by `adr006_pause_resume.rs`.

mod common;

use anchor_lang::AnchorDeserialize;
use base64::{engine::general_purpose::STANDARD, Engine as _};

use common::{
    clock, fund_actors, ix, plan_pda, send_tx, setup, subscription_pda, Signer,
};

const T0: i64 = 1_700_000_000;

/// Mirror of `nakama::state::SubscriptionPaused` (Anchor `#[event]`). Defined
/// here as a borsh-deserializable shadow so we don't depend on Anchor's
/// internal event-decoder in tests.
#[derive(AnchorDeserialize, Debug)]
#[allow(dead_code)] // unused fields kept for layout fidelity + Debug on failure
struct PausedEventPayload {
    pub subscription: solana_pubkey::Pubkey,
    pub paused_at: i64,
    pub unlocked_at_pause: u64,
}

/// Decode the latest `SubscriptionPaused` event from program logs. Anchor
/// emits events as base64 `event-discriminator || borsh-payload` on a
/// "Program data:" log line. The pause handler emits exactly one event so we
/// take the last matching line. Mirrors the pattern in
/// `tests/cancel_by_merchant.rs::decode_cancelled_event`.
fn decode_paused_event(meta: &litesvm::types::TransactionMetadata) -> PausedEventPayload {
    let line = meta
        .logs
        .iter()
        .rev()
        .find(|l| l.starts_with("Program data: "))
        .expect("emit! produces 'Program data:' line");
    let b64 = line.trim_start_matches("Program data: ").trim();
    let bytes = STANDARD.decode(b64).expect("base64 decode");
    // First 8 bytes are the event discriminator; pause handler emits only
    // SubscriptionPaused so we trust source order.
    let payload = &bytes[8..];
    PausedEventPayload::deserialize(&mut &payload[..]).expect("borsh decode SubscriptionPaused")
}

/// $10/month plan paused at elapsed=1_000_000s.
///
/// Old (pre-F4) math: `rate_per_second = 10_000_000 / 2_592_000 = 3`
/// (truncated). `unlocked_at_pause = 3 * 1_000_000 = 3_000_000`.
///
/// New (F4 mirror) math: `(1_000_000 * 10_000_000) / 2_592_000 = 3_858_024`.
/// Divergence ~28.6% — the regression that bricked off-chain accounting.
#[test]
fn unlocked_at_pause_uses_precise_lazy_division() {
    let plan_price: u64 = 10_000_000; // $10 USDC
    let plan_period: i64 = 2_592_000; // 30 days
    let elapsed: i64 = 1_000_000; // chosen so price/period truncates and product is non-trivial

    let mut env = setup();
    let actors = fund_actors(&mut env, plan_price);

    send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::create_plan_ix(
            &actors.merchant.pubkey(),
            &actors.merchant_ata,
            1,
            plan_price,
            plan_period,
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
            1,
        )],
        &[&actors.subscriber],
    )
    .expect("subscribe");

    // Pause at T0 + elapsed.
    clock::set_clock(&mut env.svm, T0 + elapsed);
    env.svm.expire_blockhash();
    let meta = send_tx(
        &mut env.svm,
        &actors.merchant,
        &[ix::pause_ix(&actors.merchant.pubkey(), &sub_pk)],
        &[&actors.merchant],
    )
    .expect("pause");

    let event = decode_paused_event(&meta);

    // Precise expectation: (elapsed * price) / period.
    let expected = (elapsed as u128)
        .checked_mul(plan_price as u128)
        .and_then(|m| m.checked_div(plan_period as u128))
        .expect("math fits u128") as u64;
    // Sanity: 3_858_024 — value pinned in the ADR-015 §F4 follow-up brief.
    assert_eq!(
        expected, 3_858_024,
        "constants check: expected pinned to 3_858_024"
    );

    // Anti-regression: the pre-F4 form would have produced 3_000_000.
    let pre_f4 = ((plan_price / plan_period as u64) as u128 * elapsed as u128) as u64;
    assert_eq!(pre_f4, 3_000_000, "pre-F4 form sanity check");
    assert_ne!(
        event.unlocked_at_pause, pre_f4,
        "ADR-015 §F4 — pause must NOT use truncated rate_per_second math"
    );

    assert_eq!(
        event.unlocked_at_pause, expected,
        "ADR-015 §F4 — pause event must mirror precise charge/cancel math"
    );
    assert_eq!(event.paused_at, T0 + elapsed, "paused_at = clock at pause");
    assert_eq!(event.subscription, sub_pk, "subscription back-ref");
}
