//! `close_session` wire helpers (ADR-x402-001 §"close_session").
//!
//! See `open_session.rs` for scope rationale (BLK-009-FAC-1). Pure
//! meta-builder; full HTTP handler deferred to post-hackathon partial-tx
//! flow.
//!
//! Notable invariant: NO `system_program` slot here. Anchor `close =
//! subscriber` uses runtime lamport-zero deallocation, not a
//! system-program CPI, so the meta vector contains exactly the three
//! accounts named in the Accounts struct. This matches the
//! BLK-007-CRIT-1 lesson that `system_program` must NEVER appear in a
//! non-`init` ix wire vector.

use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

/// Canonical account-meta vector for `close_session`.
///
/// Order (matches ADR-x402-001 §"close_session" Accounts struct):
///   0. parent       (readonly — `has_one` only)
///   1. pay_session  (mut — Anchor closes the account)
///   2. subscriber   (Signer, mut — receives PDA rent)
///
/// R1 closure note (ADR-x402-001): no `parent.state == Active` guard.
/// Subscriber must be able to close orphan satellites even when parent
/// is in Cancelled tombstone (post-ADR-013).
pub fn build_close_session_metas(
    parent: &Pubkey,
    pay_session: &Pubkey,
    subscriber: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*parent, false),
        AccountMeta::new(*pay_session, false),
        AccountMeta::new(*subscriber, true),
    ]
}
