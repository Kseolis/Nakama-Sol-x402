//! `settle_usage` wire helpers (ADR-x402-001 §"settle_usage").
//!
//! See `open_session.rs` for scope rationale (BLK-009-FAC-1). Pure
//! meta-builder; full HTTP handler deferred to post-hackathon partial-tx
//! flow.

use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

/// Canonical account-meta vector for `settle_usage`.
///
/// Order (matches ADR-x402-001 §"settle_usage" Accounts struct):
///   0. parent           (mut — withdrawn_amount mutated)
///   1. pay_session      (mut — usage_amount + state mutated)
///   2. vault            (mut — CPI source)
///   3. merchant_ata     (mut — CPI destination)
///   4. facilitator      (Signer, readonly — no rent payer here)
///   5. token_program    (readonly)
///
/// Composability note: this CPI advances `parent.withdrawn_amount`
/// (ADR-002 single source of truth) — same writer pattern as `charge`.
/// Composability suite at `programs/nakama/tests/x402_settle_composability.rs`
/// pins the no-double-spend invariant.
pub fn build_settle_usage_metas(
    parent: &Pubkey,
    pay_session: &Pubkey,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    facilitator: &Pubkey,
    token_program: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(*parent, false),
        AccountMeta::new(*pay_session, false),
        AccountMeta::new(*vault, false),
        AccountMeta::new(*merchant_ata, false),
        AccountMeta::new_readonly(*facilitator, true),
        AccountMeta::new_readonly(*token_program, false),
    ]
}
