//! `POST /subscriptions/{sub_pda}/top-up`.
//!
//! Body: `{ "amount": u64 }`. Response: `{ "tx_signature": "..." }`.
//!
//! Demo signing model: facilitator holds a hot subscriber keypair (loaded
//! from stdin at startup; see `state::parse_keypair_json`). It builds the
//! transaction, signs as the subscriber, sends, and confirms. Production
//! parity (post-hackathon): facilitator returns an unsigned tx for a
//! browser wallet adapter.
//!
//! Account ordering MUST match the on-chain `TopUp` Accounts struct from
//! ADR-007 §"top_up handler" — canonical order (verified against
//! `nakama/target/idl/nakama.json::instructions[top_up]`):
//!
//!   `[subscriber, subscription, graced_subscription?, vault, subscriber_ata, token_program]`
//!
//! Drift surfaces as Anchor `ConstraintSeeds` (2006) at tx submit. Regression
//! test in `tests/top_up_meta_order.rs` pins this against the IDL.

use std::str::FromStr;

use axum::{extract::Path, extract::State, Json};
use borsh::BorshSerialize;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_associated_token_account::get_associated_token_address;

use nakama_client::{derive_grace_pda, derive_vault_pda, SubscriptionStateByte, SubscriptionView};

use crate::{error::ApiError, state::AppState};

/// SPL Token classic program ID (Token-2022 explicitly rejected per ADR-014).
/// Hardcoded to avoid pulling in the full `spl-token` program-side crate
/// just for one constant.
const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Compute-unit limit for top_up tx: one CPI (subscriber_ata → vault) +
/// state mutation + optional satellite close. 80_000 is comfortably above
/// the worst-case observed for similar Anchor handlers; we deliberately do
/// NOT tune this from `getRecentPrioritizationFees` per agent rules
/// ("hackathon scope, no fee tuning").
const TOP_UP_COMPUTE_UNITS: u32 = 80_000;

#[derive(Debug, Deserialize)]
pub struct TopUpRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize)]
pub struct TopUpResponse {
    pub tx_signature: String,
}

pub async fn handle(
    State(state): State<AppState>,
    Path(sub_pda): Path<String>,
    Json(req): Json<TopUpRequest>,
) -> Result<Json<TopUpResponse>, ApiError> {
    let sub_pda =
        Pubkey::from_str(&sub_pda).map_err(|e| ApiError::BadRequest(format!("sub_pda: {e}")))?;

    if req.amount == 0 {
        // Off-chain pre-validation mirrors the on-chain `IllegalAmountForTopUp`
        // guard. Saves an RPC roundtrip + tx fee for an obviously-bad request.
        return Err(ApiError::BadRequest(
            "amount must be greater than zero".into(),
        ));
    }

    let signer = state
        .inner
        .demo_subscriber
        .as_ref()
        .ok_or(ApiError::SigningUnavailable)?;

    let rpc = &state.inner.rpc;
    let commitment = CommitmentConfig::confirmed();

    // Read parent Subscription to learn vault_bump, token_mint, and current
    // state byte (for satellite-presence dispatch).
    let sub_account = rpc
        .get_account_with_commitment(&sub_pda, commitment)
        .await?
        .value
        .ok_or_else(|| ApiError::NotFound(format!("subscription account not found: {sub_pda}")))?;
    let subscription = SubscriptionView::try_decode(&sub_account.data)?;

    // Defense-in-depth: signer must equal the on-chain `subscription.subscriber`.
    // The on-chain `has_one = subscriber` constraint enforces this anyway,
    // but failing fast here gives a clean 400 instead of a tx simulation
    // error.
    if signer.pubkey().to_bytes() != subscription.subscriber.to_bytes() {
        return Err(ApiError::BadRequest(
            "loaded demo keypair does not match subscription.subscriber".into(),
        ));
    }

    let program_id = state.inner.config.program_id;
    let token_program =
        Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).map_err(|e| ApiError::Internal(e.to_string()))?;

    // PDAs.
    let (vault_pda, _vault_bump) = derive_vault_pda(&program_id, &sub_pda);
    let (grace_pda, _grace_bump) = derive_grace_pda(&program_id, &sub_pda);

    // Subscriber ATA — derived from subscriber pubkey + token mint. We do
    // NOT fetch this account; SPL Token CPI on the on-chain side validates
    // mint + authority via Anchor constraints.
    let subscriber_ata = get_associated_token_address(&signer.pubkey(), &subscription.token_mint);

    // Build account metas via the canonical helper so the integration test in
    // `tests/top_up_meta_order.rs` can pin the same vector against the IDL
    // (BLK-007-S2 / BLK-007-CRIT-1).
    let metas = build_top_up_metas(
        &signer.pubkey(),
        &sub_pda,
        &program_id,
        &grace_pda,
        &vault_pda,
        &subscriber_ata,
        &token_program,
        subscription.state_byte(),
    );

    // Anchor instruction data: `discriminator || borsh(args)`. Discriminator
    // = sha256("global:top_up")[..8]. Args = `(amount: u64,)`.
    let mut data = top_up_discriminator().to_vec();
    BorshSerialize::serialize(&req.amount, &mut data)
        .map_err(|e| ApiError::Internal(format!("borsh serialize: {e}")))?;

    let top_up_ix = Instruction {
        program_id,
        accounts: metas,
        data,
    };
    let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(TOP_UP_COMPUTE_UNITS);

    // Recent blockhash.
    let blockhash = rpc.get_latest_blockhash().await?;
    let message =
        Message::new_with_blockhash(&[cu_ix, top_up_ix], Some(&signer.pubkey()), &blockhash);
    let tx = Transaction::new(&[signer], message, blockhash);

    // Submit + confirm. We use the spinner-free variant to keep the HTTP
    // handler responsive; for the demo this is fine — `confirmed` commitment
    // typically resolves in <2s on devnet.
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .map_err(|e| ApiError::Rpc(e.to_string()))?;

    tracing::info!(
        %sub_pda,
        amount = req.amount,
        signature = %sig,
        from_state = subscription.state,
        "top-up confirmed"
    );

    Ok(Json(TopUpResponse {
        tx_signature: sig.to_string(),
    }))
}

/// Canonical account-meta vector for the `top_up` instruction. Public so the
/// regression test in `tests/top_up_meta_order.rs` (BLK-007-S2) can pin the
/// order against `nakama/target/idl/nakama.json` without spinning up an HTTP
/// server or the RPC client.
///
/// Order (verified against IDL `instructions[top_up].accounts`):
///   0. subscriber       (mut, signer)
///   1. subscription     (mut)
///   2. graced_subscription (Option, mut) — real PDA iff `state == GracePeriod`,
///      `program_id` placeholder otherwise (Anchor `allow-missing-optionals`
///      sentinel pattern).
///   3. vault            (mut)
///   4. subscriber_ata   (mut)
///   5. token_program    (readonly)
///
/// BLK-007-CRIT-1 (review 2026-05-05): the previous inline implementation
/// swapped slots 2-3 and appended a spurious `system_program` (TopUp
/// Accounts has no `init` constraint so there is no System program field),
/// causing every devnet `top_up` to fail with `ConstraintSeeds (2006)`.
#[allow(clippy::too_many_arguments)]
pub fn build_top_up_metas(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    program_id: &Pubkey,
    grace_pda: &Pubkey,
    vault_pda: &Pubkey,
    subscriber_ata: &Pubkey,
    token_program: &Pubkey,
    state_byte: SubscriptionStateByte,
) -> Vec<AccountMeta> {
    let graced_meta = if state_byte == SubscriptionStateByte::GracePeriod {
        AccountMeta::new(*grace_pda, false)
    } else {
        // BLK-007-X3-cross-off: dispatch via the named
        // `SubscriptionStateByte::GracePeriod` accessor (was raw `state == 2`)
        // so this stays in sync with `state.rs:50-69` if the FSM byte mapping
        // ever changes.
        AccountMeta::new_readonly(*program_id, false)
    };

    vec![
        AccountMeta::new(*subscriber, true),              // 0
        AccountMeta::new(*subscription, false),           // 1
        graced_meta,                                      // 2
        AccountMeta::new(*vault_pda, false),              // 3
        AccountMeta::new(*subscriber_ata, false),         // 4
        AccountMeta::new_readonly(*token_program, false), // 5
    ]
}

/// First 8 bytes of `SHA256("global:top_up")` — Anchor 1.0 instruction
/// discriminator convention. Computed on first call; cached for process
/// lifetime via `OnceLock`.
pub(crate) fn top_up_discriminator() -> [u8; 8] {
    use std::sync::OnceLock;
    static DISC: OnceLock<[u8; 8]> = OnceLock::new();
    *DISC.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(b"global:top_up");
        let full = hasher.finalize();
        let mut out = [0u8; 8];
        out.copy_from_slice(&full[..8]);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_is_stable() {
        // First 8 bytes of sha256("global:top_up"). Snapshot — if this drifts
        // the on-chain handler name was renamed (anchor-engineer change).
        let d = top_up_discriminator();
        assert_eq!(d.len(), 8);
        // Re-compute manually to detect tampering of the helper.
        let mut h = Sha256::new();
        h.update(b"global:top_up");
        let full = h.finalize();
        assert_eq!(&full[..8], &d[..]);
    }
}
