//! Phase 4 RED — PaySessionView byte-equivalence + PDA derivation.
//!
//! Failing until GREEN lands:
//! - `PaySessionView` Borsh decoder matching on-chain layout (210 bytes total)
//! - `derive_pay_session_pda(program, subscription, session_id)` helper
//! - `PAY_SESSION_SEED` constant exported from `nakama_client::constants`
//!
//! Drift catcher: if on-chain `PaySession` layout shifts, the
//! `pay_session_view_decodes_canonical_layout` test fails to deserialize a
//! 202-byte body. Same protection as `subscription_view_decodes_canonical_layout`
//! (impl-sign-off-2026-04-27 §F-3).

use nakama_client::{
    accounts::{strip_discriminator, PaySessionView},
    constants::PAY_SESSION_SEED,
    pda::derive_pay_session_pda,
};
use solana_pubkey::Pubkey;

#[test]
fn pay_session_seed_constant_matches_on_chain() {
    // Mirror of nakama::constants::PAY_SESSION_SEED. Drift caught by
    // PDA derivation diverging from on-chain seeds = [...] constraint.
    assert_eq!(PAY_SESSION_SEED, b"pay_session");
}

#[test]
fn derive_pay_session_pda_is_deterministic_and_distinct_from_grace() {
    let program = Pubkey::new_from_array([7u8; 32]);
    let sub = Pubkey::new_from_array([8u8; 32]);

    let (a1, _) = derive_pay_session_pda(&program, &sub, 42);
    let (a2, _) = derive_pay_session_pda(&program, &sub, 42);
    assert_eq!(a1, a2, "deterministic for same inputs");

    let (b, _) = derive_pay_session_pda(&program, &sub, 43);
    assert_ne!(a1, b, "different session_id ⇒ distinct PDA");

    // Cross-namespace: pay_session and grace must not collide for the same
    // subscription. (Different seed prefixes guarantee this; assertion just
    // pins the expectation.)
    let (grace, _) = nakama_client::pda::derive_grace_pda(&program, &sub);
    assert_ne!(a1, grace);
}

#[test]
fn pay_session_view_decodes_canonical_layout() {
    // Construct a wire-shaped 210-byte buffer matching on-chain Borsh order:
    // [8 disc][32 sub][32 merchant][32 merchant_ata][32 facilitator]
    // [8 session_id][8 opened_at][8 last_settle_at][8 usage_amount]
    // [8 reservation_cap][1 state][1 bump][32 reserved]
    let mut buf = Vec::with_capacity(210);
    // discriminator (any 8 bytes — strip_discriminator doesn't validate)
    buf.extend_from_slice(&[1u8; 8]);

    let sub_pk = Pubkey::new_from_array([0xa1; 32]);
    let merchant = Pubkey::new_from_array([0xa2; 32]);
    let merchant_ata = Pubkey::new_from_array([0xa3; 32]);
    let facilitator = Pubkey::new_from_array([0xa4; 32]);

    buf.extend_from_slice(sub_pk.as_ref());
    buf.extend_from_slice(merchant.as_ref());
    buf.extend_from_slice(merchant_ata.as_ref());
    buf.extend_from_slice(facilitator.as_ref());

    buf.extend_from_slice(&123u64.to_le_bytes()); // session_id
    buf.extend_from_slice(&1_700_000_000i64.to_le_bytes()); // opened_at
    buf.extend_from_slice(&1_700_000_500i64.to_le_bytes()); // last_settle_at
    buf.extend_from_slice(&750u64.to_le_bytes()); // usage_amount
    buf.extend_from_slice(&5_000u64.to_le_bytes()); // reservation_cap
    buf.push(0u8); // state = Open
    buf.push(254u8); // bump
    buf.extend_from_slice(&[0u8; 32]); // reserved

    assert_eq!(buf.len(), 210, "wire size invariant");

    // Decode via try_decode (strips discriminator).
    let view = PaySessionView::try_decode(&buf).expect("decode 210-byte canonical buffer");

    assert_eq!(view.subscription, sub_pk);
    assert_eq!(view.merchant, merchant);
    assert_eq!(view.merchant_ata, merchant_ata);
    assert_eq!(view.facilitator, facilitator);
    assert_eq!(view.session_id, 123);
    assert_eq!(view.opened_at, 1_700_000_000);
    assert_eq!(view.last_settle_at, 1_700_000_500);
    assert_eq!(view.usage_amount, 750);
    assert_eq!(view.reservation_cap, 5_000);
    assert_eq!(view.state, 0);
    assert_eq!(view.bump, 254);
    assert_eq!(view.reserved, [0u8; 32]);
}

#[test]
fn pay_session_view_too_short_errors_cleanly() {
    // 8 disc + 100 body — body is 102 bytes short of expected 202.
    let buf = vec![0u8; 108];
    let body = strip_discriminator(&buf).expect("strip ok");
    assert_eq!(body.len(), 100);
    assert!(
        PaySessionView::try_decode(&buf).is_err(),
        "truncated body must error"
    );
}
