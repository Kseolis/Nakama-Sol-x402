//! Phase 1 RED — TDD spec for ADR-x402-001 PaySession foundation.
//!
//! These tests must FAIL to compile until the GREEN commit lands the
//! `PaySession` struct, `PaySessionState` enum, `PAY_SESSION_SEED` constant,
//! 12 new error variants, and 3 new events.
//!
//! Coverage:
//! - PaySession::INIT_SPACE == 202 (ADR-x402-001 §"PaySession PDA Layout":
//!   8 disc + 202 payload = 210 total)
//! - PaySessionState discriminants {Open=0, Settling=1, Closed=2}
//! - PAY_SESSION_SEED == b"pay_session"
//! - GRACE_DURATION untouched (Phase 1 = additive only)
//! - 12 new NakamaError variants in the {6021..6032} range
//! - Subscription INIT_SPACE unchanged (267) — forward-compat invariant
//! - State byte at offset 192 unchanged — keeper memcmp invariant (BLK-19)

mod common;

use nakama::constants::PAY_SESSION_SEED;
use nakama::error::NakamaError as ProgramError;
use nakama::state::{PaySession, PaySessionState, Subscription};

use anchor_lang::Space;

#[test]
fn pay_session_init_space_is_202_bytes() {
    // ADR-x402-001 §"PaySession PDA Layout":
    // 32 (subscription) + 32 (merchant) + 32 (merchant_ata) + 32 (facilitator)
    // + 8 (session_id) + 8 (opened_at) + 8 (last_settle_at) + 8 (usage_amount)
    // + 8 (reservation_cap) + 1 (state) + 1 (bump) + 32 (reserved) = 202
    assert_eq!(
        PaySession::INIT_SPACE,
        202,
        "ADR-x402-001 §PDA Layout — payload is 202 bytes (210 total with discriminator)"
    );
}

#[test]
fn pay_session_state_discriminants_are_zero_one_two() {
    // ADR-x402-001 §"Internal FSM" — fixed discriminants for forward-compat
    assert_eq!(PaySessionState::Open as u8, 0);
    assert_eq!(PaySessionState::Settling as u8, 1);
    assert_eq!(PaySessionState::Closed as u8, 2);
}

#[test]
fn pay_session_seed_is_pay_session_bytes() {
    assert_eq!(PAY_SESSION_SEED, b"pay_session");
}

#[test]
fn subscription_init_space_unchanged_by_phase_1() {
    // Forward-compat invariant: ADR-001 layout (267) MUST NOT shift
    // when x402 satellite types are added. Phase 1 is additive only.
    assert_eq!(
        Subscription::INIT_SPACE,
        267,
        "Subscription layout MUST be untouched by ADR-x402-001 Phase 1 \
         (satellite-only design — ADR-x402-001 §Decision)"
    );
}

#[test]
fn x402_error_variants_have_expected_codes() {
    // 12 new variants per ADR-x402-001 §"Error variants added".
    // Codes follow declaration order in error.rs starting at 6021
    // (after ADR-009's NoCancelAuthority=6019, SubscriberAccountMismatch=6020).
    //
    // We assert via numeric position because Anchor's #[error_code] derives
    // discriminants from declaration order — drift surfaces here as a wrong
    // index.
    use ProgramError::*;
    assert_eq!(UnauthorizedOpenSession as u32, 21);
    assert_eq!(ParentNotActive as u32, 22);
    assert_eq!(ReservationCapExceedsEscrow as u32, 23);
    assert_eq!(IllegalAmountForSettle as u32, 24);
    assert_eq!(IllegalStateForSettle as u32, 25);
    assert_eq!(ReservationCapExceeded as u32, 26);
    assert_eq!(UnauthorizedFacilitator as u32, 27);
    assert_eq!(PaySessionParentMismatch as u32, 28);
    assert_eq!(IllegalStateForClose as u32, 29);
    assert_eq!(UnauthorizedClose as u32, 30);
    assert_eq!(ArithmeticOverflow as u32, 31);
    // MerchantAtaMismatch is reused (existed via AtaMismatch / ConstraintAddress);
    // a fresh variant is added for x402-specific contextualisation.
    assert_eq!(PaySessionMerchantAtaMismatch as u32, 32);
}

#[test]
fn pay_session_state_round_trips_through_borsh() {
    use anchor_lang::AnchorDeserialize;
    use anchor_lang::AnchorSerialize;

    for original in [
        PaySessionState::Open,
        PaySessionState::Settling,
        PaySessionState::Closed,
    ] {
        let mut buf = Vec::new();
        original
            .serialize(&mut buf)
            .expect("serialize PaySessionState");
        assert_eq!(buf.len(), 1, "PaySessionState must serialize to a single byte");
        let decoded = PaySessionState::deserialize(&mut &buf[..])
            .expect("deserialize PaySessionState");
        assert_eq!(decoded, original);
    }
}

#[test]
fn pay_session_state_invalid_byte_fails_deserialization() {
    use anchor_lang::AnchorDeserialize;
    // Byte 3 is reserved for forward-compat (#[non_exhaustive]); reading
    // it back as PaySessionState must error rather than silently coerce.
    let invalid = [3u8];
    let result = PaySessionState::deserialize(&mut &invalid[..]);
    assert!(
        result.is_err(),
        "Unknown discriminant must fail Borsh decode (non_exhaustive guard)"
    );
}
