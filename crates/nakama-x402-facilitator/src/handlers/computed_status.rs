//! `GET /subscriptions/{sub_pda}/computed-status`.
//!
//! Boundary contract from ADR-007 §"Off-chain ComputedStatus derive". Reads
//! the parent Subscription account + (optional) GracedSubscription satellite,
//! delegates to `nakama_client::derive_status`, returns the JSON variant.

use std::str::FromStr;

use axum::{extract::Path, extract::State, Json};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;

use nakama_client::{derive_grace_pda, derive_status, ComputedStatus};

use crate::{error::ApiError, state::AppState};

pub async fn handle(
    State(state): State<AppState>,
    Path(sub_pda): Path<String>,
) -> Result<Json<ComputedStatus>, ApiError> {
    let sub_pda =
        Pubkey::from_str(&sub_pda).map_err(|e| ApiError::BadRequest(format!("sub_pda: {e}")))?;

    let rpc = &state.inner.rpc;
    let commitment = CommitmentConfig::confirmed();

    // Read parent Subscription. `get_account_with_commitment` returns
    // `Response<Option<Account>>`; verified via `cargo info solana-rpc-client`
    // for the 3.1.x family and via the on-chain test harness usage pattern.
    let sub_resp = rpc
        .get_account_with_commitment(&sub_pda, commitment)
        .await?;
    let sub_account = sub_resp
        .value
        .ok_or_else(|| ApiError::NotFound(format!("subscription account not found: {sub_pda}")))?;

    let subscription = nakama_client::SubscriptionView::try_decode(&sub_account.data)?;

    // Pre-derive the GracedSubscription PDA. We always look it up — if the
    // satellite doesn't exist (state != GracePeriod), `value` is None and
    // we pass None to `derive_status`. This is the discriminator-based
    // dispatch the agent rules mandate.
    let (grace_pda, _grace_bump) = derive_grace_pda(&state.inner.config.program_id, &sub_pda);
    let grace_account = rpc
        .get_account_with_commitment(&grace_pda, commitment)
        .await?
        .value;

    let graced_view = match grace_account {
        Some(acc) => Some(nakama_client::GracedSubscriptionView::try_decode(
            &acc.data,
        )?),
        None => None,
    };

    // Wall clock: use the same `now` the on-chain Clock sysvar would see.
    // For demo simplicity we rely on the host clock; an indexer-grade
    // implementation reads `Clock::unix_timestamp` via RPC `getSlot` +
    // `getBlockTime`, but that's out of scope for ADR-007 (no retry-with-
    // backoff per agent rules).
    let now = chrono_now_unix();

    let status = derive_status(&subscription, graced_view.as_ref(), None, now);
    tracing::info!(
        %sub_pda,
        state_byte = subscription.state,
        ?status,
        "computed-status derived"
    );
    Ok(Json(status))
}

/// Wall clock as `i64` unix seconds. We avoid pulling in the `chrono` crate
/// for one call; `SystemTime` is enough.
fn chrono_now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
