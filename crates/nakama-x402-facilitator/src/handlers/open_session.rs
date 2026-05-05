//! `open_session` instruction wire helpers (ADR-x402-001 §"open_session").
//!
//! Phase 4 ships ONLY the canonical meta-builder. The full HTTP `handle()`
//! function is deferred (BLK-009-FAC-1) because the polymorphic-signer
//! flow needs either dual-keypair config or partial-tx delegation —
//! out of hackathon scope. The demo path runs through the TS SDK
//! (`clients/ts/scripts/07-x402-flow.ts`, Phase 5) which builds the same
//! meta vector via Anchor IDL and submits via wallet.
//!
//! The pure helper is exposed so the regression test in
//! `tests/x402_session_meta_order.rs` can pin the order against the IDL
//! at `nakama/target/idl/nakama.json::instructions[open_session]`.
//! Drift catches off-chain meta bugs before they hit a live submit
//! (lesson from ADR-007 cycle-4 BLK-007-CRIT-1).

use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

/// Canonical account-meta vector for `open_session`.
///
/// Order (matches ADR-x402-001 §"open_session" Accounts struct):
///   0. parent           (Subscription PDA, readonly — `has_one` reads only)
///   1. pay_session      (init, mut — pays rent on init)
///   2. subscriber       (Signer, mut — payer)
///   3. system_program   (readonly — required by `init`)
pub fn build_open_session_metas(
    parent: &Pubkey,
    pay_session: &Pubkey,
    subscriber: &Pubkey,
    system_program: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*parent, false),
        AccountMeta::new(*pay_session, false),
        AccountMeta::new(*subscriber, true),
        AccountMeta::new_readonly(*system_program, false),
    ]
}
