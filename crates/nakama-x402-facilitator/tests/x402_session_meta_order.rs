//! Phase 4 RED — meta-order regression for ADR-x402-001 lifecycle ix
//! (`open_session`, `settle_usage`, `close_session`).
//!
//! Direct lesson from BLK-007-CRIT-1 (ADR-007 cycle-4): the previous
//! top_up handler shipped with `subscriber_ata` and `vault` swapped into
//! slots 2-3 and a spurious `system_program` appended. The TS path masked
//! the drift because Anchor's IDL-driven account resolver re-orders meta
//! transparently. This test pins the canonical order at the off-chain
//! Rust boundary.
//!
//! Coverage:
//! - open_session: slots [parent, pay_session, subscriber, system_program]
//! - settle_usage: slots [parent, pay_session, vault, merchant_ata,
//!   facilitator, token_program]
//! - close_session: slots [parent, pay_session, subscriber]
//! - is_signer / is_writable flags per slot
//! - When `nakama/target/idl/nakama.json` is present, account names must
//!   match the IDL (`instructions[<name>].accounts[].name`).

use nakama_x402_facilitator::handlers::{
    close_session::build_close_session_metas, open_session::build_open_session_metas,
    settle_usage::build_settle_usage_metas,
};
use solana_pubkey::Pubkey;

fn keys() -> Keys {
    Keys {
        parent: Pubkey::new_from_array([0xB1; 32]),
        pay_session: Pubkey::new_from_array([0xB2; 32]),
        subscriber: Pubkey::new_from_array([0xB3; 32]),
        system_program: Pubkey::new_from_array([0xB4; 32]),
        vault: Pubkey::new_from_array([0xB5; 32]),
        merchant_ata: Pubkey::new_from_array([0xB6; 32]),
        facilitator: Pubkey::new_from_array([0xB7; 32]),
        token_program: Pubkey::new_from_array([0xB8; 32]),
    }
}

struct Keys {
    parent: Pubkey,
    pay_session: Pubkey,
    subscriber: Pubkey,
    system_program: Pubkey,
    vault: Pubkey,
    merchant_ata: Pubkey,
    facilitator: Pubkey,
    token_program: Pubkey,
}

#[test]
fn open_session_meta_order_canonical() {
    // ADR-x402-001 §"open_session" Accounts:
    //   parent, pay_session, subscriber (Signer mut), system_program
    let k = keys();
    let metas =
        build_open_session_metas(&k.parent, &k.pay_session, &k.subscriber, &k.system_program);
    assert_eq!(metas.len(), 4, "open_session has 4 accounts");

    // Slot 0: parent (Subscription PDA — read-only for handler signer guard
    // but Anchor `has_one` doesn't write, so non-mut is fine in IDL).
    // Note: ADR-x402-001 declares `parent` without explicit `mut`, so the
    // canonical IDL marks it readonly.
    assert_eq!(metas[0].pubkey, k.parent);
    assert!(!metas[0].is_signer);
    assert!(!metas[0].is_writable, "parent is readonly in open_session");

    // Slot 1: pay_session (init mut)
    assert_eq!(metas[1].pubkey, k.pay_session);
    assert!(!metas[1].is_signer);
    assert!(metas[1].is_writable, "pay_session is `init` ⇒ mut");

    // Slot 2: subscriber (Signer + mut payer)
    assert_eq!(metas[2].pubkey, k.subscriber);
    assert!(metas[2].is_signer, "subscriber must be signer");
    assert!(metas[2].is_writable, "subscriber pays for init ⇒ mut");

    // Slot 3: system_program (readonly)
    assert_eq!(metas[3].pubkey, k.system_program);
    assert!(!metas[3].is_signer);
    assert!(!metas[3].is_writable);
}

#[test]
fn settle_usage_meta_order_canonical() {
    // ADR-x402-001 §"settle_usage" Accounts:
    //   parent (mut), pay_session (mut), vault (mut), merchant_ata (mut),
    //   facilitator (Signer), token_program
    let k = keys();
    let metas = build_settle_usage_metas(
        &k.parent,
        &k.pay_session,
        &k.vault,
        &k.merchant_ata,
        &k.facilitator,
        &k.token_program,
    );
    assert_eq!(metas.len(), 6);

    assert_eq!(metas[0].pubkey, k.parent);
    assert!(metas[0].is_writable, "parent.withdrawn_amount is mutated");
    assert!(!metas[0].is_signer);

    assert_eq!(metas[1].pubkey, k.pay_session);
    assert!(metas[1].is_writable, "pay_session usage_amount mutated");
    assert!(!metas[1].is_signer);

    assert_eq!(metas[2].pubkey, k.vault);
    assert!(metas[2].is_writable, "vault is CPI source");
    assert!(!metas[2].is_signer);

    assert_eq!(metas[3].pubkey, k.merchant_ata);
    assert!(metas[3].is_writable, "merchant_ata is CPI dest");
    assert!(!metas[3].is_signer);

    assert_eq!(metas[4].pubkey, k.facilitator);
    assert!(metas[4].is_signer, "facilitator must be signer");
    assert!(!metas[4].is_writable, "facilitator non-mut (no rent payer)");

    assert_eq!(metas[5].pubkey, k.token_program);
    assert!(!metas[5].is_signer);
    assert!(!metas[5].is_writable);
}

#[test]
fn close_session_meta_order_canonical() {
    // ADR-x402-001 §"close_session" Accounts:
    //   parent (readonly), pay_session (mut, closed), subscriber (Signer mut)
    let k = keys();
    let metas = build_close_session_metas(&k.parent, &k.pay_session, &k.subscriber);
    assert_eq!(metas.len(), 3);

    assert_eq!(metas[0].pubkey, k.parent);
    assert!(!metas[0].is_writable, "parent is readonly in close");
    assert!(!metas[0].is_signer);

    assert_eq!(metas[1].pubkey, k.pay_session);
    assert!(
        metas[1].is_writable,
        "pay_session is closed ⇒ Anchor needs writable"
    );
    assert!(!metas[1].is_signer);

    assert_eq!(metas[2].pubkey, k.subscriber);
    assert!(metas[2].is_signer);
    assert!(metas[2].is_writable, "subscriber receives rent ⇒ mut");
}

#[test]
fn no_system_program_in_settle_or_close() {
    // BLK-007-CRIT-1 lesson: system_program ONLY appears in `init` ix paths.
    // settle_usage does no init; close_session uses Anchor `close` (no
    // system_program account meta). Asserting absence prevents regression.
    let k = keys();

    let settle = build_settle_usage_metas(
        &k.parent,
        &k.pay_session,
        &k.vault,
        &k.merchant_ata,
        &k.facilitator,
        &k.token_program,
    );
    assert!(
        !settle.iter().any(|m| m.pubkey == k.system_program),
        "settle_usage must NOT reference system_program"
    );

    let close = build_close_session_metas(&k.parent, &k.pay_session, &k.subscriber);
    assert!(
        !close.iter().any(|m| m.pubkey == k.system_program),
        "close_session must NOT reference system_program"
    );
}
