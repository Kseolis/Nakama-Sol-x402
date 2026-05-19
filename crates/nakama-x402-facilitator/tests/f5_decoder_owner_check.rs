//! ADR-015 §F5 regression: facilitator-level proof that the owner-check
//! helper returns a typed error AND that the HTTP layer maps that error to
//! `404 Not Found` rather than `502 Bad Gateway` / `500 Internal`.
//!
//! Why this lives in the facilitator crate (not nakama-client): the
//! `decode_program_owned` helper is already covered by unit tests inside
//! `accounts.rs::tests::decode_owned_*`. The integration concern at the
//! facilitator boundary is *status-code mapping*: a spoofed System-owned
//! account masquerading as a Subscription must result in the same HTTP
//! shape as "PDA doesn't exist", not in a 5xx that leaks internal state.

use axum::{http::StatusCode, response::IntoResponse};
use nakama_client::{AccountDecodeError, SubscriptionView};
use nakama_x402_facilitator::ApiError;
use solana_account::Account;
use solana_pubkey::Pubkey;

fn make_account_with_owner(owner: Pubkey, discriminator: [u8; 8]) -> Account {
    // 8-byte discriminator + zeros for the body. Body length is whatever —
    // the owner check fires BEFORE the Borsh decode, so we never reach the
    // body parser when the owner is wrong. For the wrong-disc case we add
    // a few bytes so the discriminator slice is well-formed.
    let mut data = discriminator.to_vec();
    data.extend(std::iter::repeat(0u8).take(300));
    Account {
        lamports: 1,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

#[tokio::test]
async fn wrong_owner_decodes_to_api_error_not_found() {
    let program_id = Pubkey::new_from_array([0xAA; 32]);
    let foreign = Pubkey::new_from_array([0xBB; 32]);
    let account = make_account_with_owner(foreign, SubscriptionView::discriminator());

    let err = SubscriptionView::decode_owned(&account, &program_id)
        .expect_err("foreign-owned account must reject");
    assert!(matches!(err, AccountDecodeError::WrongOwner { .. }));

    // ApiError surface — the handler converts via `?`, so we exercise the
    // same `From` impl plus the response mapping.
    let api_err: ApiError = err.into();
    let response = api_err.into_response();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "WrongOwner must map to 404, not 5xx — would otherwise leak that the PDA address exists but is owned by something else (probing oracle)"
    );
}

#[tokio::test]
async fn wrong_discriminator_decodes_to_api_error_not_found() {
    let program_id = Pubkey::new_from_array([0xAA; 32]);
    // Owner matches but disc is e.g. a PaySession one — same program, wrong type.
    let account = make_account_with_owner(program_id, [0xFF; 8]);

    let err = SubscriptionView::decode_owned(&account, &program_id)
        .expect_err("wrong discriminator must reject");
    assert!(matches!(err, AccountDecodeError::WrongDiscriminator { .. }));

    let api_err: ApiError = err.into();
    let response = api_err.into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn correct_owner_and_discriminator_passes_through_to_borsh() {
    // Sanity: with the right owner + disc, the helper proceeds to Borsh
    // decode. Body is zeroed and shorter than INIT_SPACE, so Borsh will
    // fail — but the failure mode is `Borsh`, not `WrongOwner` /
    // `WrongDiscriminator`. Confirms guard order: owner → disc → borsh.
    let program_id = Pubkey::new_from_array([0xAA; 32]);
    let mut data = SubscriptionView::discriminator().to_vec();
    data.extend([0u8; 16]); // intentionally too short for the real layout
    let account = Account {
        lamports: 1,
        data,
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    let err = SubscriptionView::decode_owned(&account, &program_id).unwrap_err();
    assert!(
        matches!(err, AccountDecodeError::Borsh(_)),
        "guard order regression — expected Borsh failure after passing owner+disc, got {err:?}"
    );
}
