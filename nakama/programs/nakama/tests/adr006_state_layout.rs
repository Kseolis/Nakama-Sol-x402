//! Phase 1 RED — TDD spec for ADR-006 PausedSubscription foundation.
//!
//! Coverage:
//! - `PausedSubscription::INIT_SPACE == 41` — 32 sub + 8 paused_at + 1 bump
//!   (8 disc + 41 = 49 bytes total per ADR-006 §"Storage layout")
//! - `PAUSED_SUB_SEED == b"paused_sub"`
//! - 4 new NakamaError variants in {6033..6036} range:
//!   UnauthorizedPause, IllegalStateForPause, UnauthorizedResume,
//!   IllegalStateForResume
//! - Subscription INIT_SPACE unchanged (267) — forward-compat invariant
//! - x402 reserve [u8;32] untouched (ADR-006 §"Storage layout" — does NOT
//!   carve into reserved[])

mod common;

use anchor_lang::Space;

#[test]
fn paused_subscription_init_space_is_41_bytes() {
    use nakama::state::PausedSubscription;
    assert_eq!(
        PausedSubscription::INIT_SPACE,
        41,
        "ADR-006 §Storage layout — 41 bytes payload (49 total with discriminator)"
    );
}

#[test]
fn paused_sub_seed_constant() {
    use nakama::constants::PAUSED_SUB_SEED;
    assert_eq!(PAUSED_SUB_SEED, b"paused_sub");
}

#[test]
fn subscription_init_space_unchanged_by_phase_1() {
    use nakama::state::Subscription;
    assert_eq!(
        Subscription::INIT_SPACE,
        267,
        "Subscription layout MUST be untouched by ADR-006 (satellite-only)"
    );
}

#[test]
fn adr006_error_variants_have_expected_codes() {
    use nakama::error::NakamaError::*;
    // Codes follow declaration order. ADR-x402-001 ended at code 32
    // (PaySessionMerchantAtaMismatch); ADR-006 appends 33..36.
    assert_eq!(UnauthorizedPause as u32, 33);
    assert_eq!(IllegalStateForPause as u32, 34);
    assert_eq!(UnauthorizedResume as u32, 35);
    assert_eq!(IllegalStateForResume as u32, 36);
}
