//! ADR-005 SDK helper — Rust mirror of (forthcoming)
//! `clients/ts/src/instructions/changeRate.ts`.
//!
//! ADR-005 §"Decision": rate migration is composition, not mutation. We
//! pack `[close_session × N?, cancel(old_sub), cleanup(old_sub), subscribe(plan_v2)]`
//! into a single atomic Transaction. Solana's entire-tx commit-or-revert
//! guarantee gives subscribers a clean "either I'm on Plan v2 or I never
//! left Plan v1" outcome (ADR-005 §E2).
//!
//! Q-resolution coverage (ADR-005 §Q-resolution):
//! - Q1 — subscriber-initiated only; helper takes `subscriber` signer pubkey.
//! - Q5 — same-mint guard: fetch old + new Plan, refuse on
//!   `token_mint` mismatch.
//! - Q7 — pre-scan x402 PaySessions parented to the old Subscription and
//!   inline `close_session` (reuses `list_alive_pay_sessions` from ADR-008).
//! - Q11 — fresh-subscribe fallback: if `old_sub` does not exist on chain
//!   (AccountDecodeError::WrongOwner via `decode_program_owned` on a
//!   System-owned/missing slot is mapped via the RPC `Option<Account>`
//!   short-circuit), helper degrades to a plain `subscribe(plan_v2)` tx.
//!
//! Envelope guard: enforces `alive_sessions ≤ 4` per the 1232-byte
//! `PACKET_DATA_SIZE` limit (ADR-008 §"x402 forward-compat"). The full
//! resubscribe builder also caps at 4 — we reuse the constant to keep both
//! flows visible at the same size budget.
//!
//! GRASP roles:
//! - `ChangeRateOptions` — Information Expert: caller-supplied inputs.
//! - `build_change_rate_tx` — Controller: orchestrates fetch + same-mint
//!   guard + satellite enumeration + builder + sign-less Transaction
//!   assembly. The caller signs and submits (mirrors TS contract of
//!   returning an unsigned Transaction).
//!
//! References:
//! - ADR-005 §"SDK composition contract", §Q5, §Q7, §Q11.
//! - ADR-008 §"Decision" (composite-tx atomicity baseline).
//! - ADR-013 §"Cancel handler" (cancel ix accounts).
//! - ADR-009 §"Constraint shape" (polymorphic signer — we sign as
//!   subscriber per ADR-005 Q1).
//! - ADR-015 §F5 (`decode_program_owned` for every RPC fetch).

use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::client_error::Error as RpcClientError;
use solana_transaction::Transaction;
use spl_associated_token_account::get_associated_token_address;
use thiserror::Error;

use crate::accounts::{AccountDecodeError, PlanView, SubscriptionView};
use crate::discriminator::{
    cancel_discriminator, cleanup_discriminator, close_session_discriminator,
    subscribe_discriminator,
};
use crate::pda::{
    derive_grace_pda, derive_paused_sub_pda, derive_pay_session_pda, derive_subscription_pda,
    derive_vault_pda,
};
use crate::resubscribe::{list_alive_pay_sessions, MAX_INLINE_PAY_SESSION_CLOSES};

/// SPL Token classic program ID. Same hardcode as `resubscribe.rs` —
/// avoid pulling `spl-token` just for the constant.
const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const SYSVAR_RENT_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");

/// Caller inputs for the ADR-005 rate-change composite tx.
///
/// `subscriber` is the signer's pubkey only (we return an unsigned
/// `Transaction` per the TS mirror's contract — caller wires their own
/// `Keypair` / hardware wallet at submit time). ADR-005 Q1: subscriber-
/// initiated only; merchant-side migration is explicitly out of scope.
pub struct ChangeRateOptions {
    /// Nakama program ID.
    pub program_id: Pubkey,
    /// Subscriber pubkey — pays for the tx, signs cancel + cleanup +
    /// subscribe. Must equal `old_sub.subscriber` (cancel handler enforces
    /// via the ADR-009 polymorphic guard).
    pub subscriber: Pubkey,
    /// Old Plan PDA (the rate the subscriber is migrating away from).
    pub old_plan: Pubkey,
    /// New Plan PDA (Plan v2 — fresh `create_plan` per ADR-005 §"Decision").
    pub new_plan: Pubkey,
    /// Periods to prefund on the new subscription. Mirrors `subscribe`'s
    /// `periods_to_prefund: u8` arg (ADR-002 BLK-13). 1..=255.
    pub new_deposit_periods: u8,
    /// Whether to enumerate + inline-close alive PaySessions on the old
    /// subscription (ADR-005 Q7 / ADR-008 §"x402 forward-compat").
    /// `true` = clean UX + rent reclaim; `false` = subscriber accepts
    /// orphan-rent lockup but gets a smaller tx envelope.
    pub close_alive_pay_sessions: bool,
}

impl Default for ChangeRateOptions {
    fn default() -> Self {
        Self {
            program_id: Pubkey::default(),
            subscriber: Pubkey::default(),
            old_plan: Pubkey::default(),
            new_plan: Pubkey::default(),
            new_deposit_periods: 1,
            close_alive_pay_sessions: true,
        }
    }
}

/// Failure surface for ADR-005 change-rate composition.
#[derive(Debug, Error)]
pub enum ChangeRateError {
    /// ADR-005 Q5 same-mint guard fired — old and new Plan reference
    /// different `token_mint` snapshots. Cross-mint migration is its own
    /// future ADR (Token-2022 envelope) and explicitly deferred.
    #[error("cross-mint migration not supported (ADR-005 Q5)")]
    CrossMintMigrationUnsupported,

    /// More alive PaySessions than the 1232-byte tx envelope can fit
    /// inline (ADR-008 §"x402 forward-compat", soft cap 4). Caller should
    /// pre-close in a separate tx OR set `close_alive_pay_sessions = false`.
    #[error("too many alive PaySessions: {count} > {cap} (1232-byte envelope, ADR-008)", cap = MAX_INLINE_PAY_SESSION_CLOSES)]
    TooManyAliveSessions { count: usize },

    /// Old subscription not found — surfaced as info, not error, when the
    /// helper falls back to plain `subscribe(plan_v2)` (Q11). Retained as
    /// a variant so SDK consumers can distinguish "I asked to migrate but
    /// there was nothing to migrate from" if they wish; the orchestrator
    /// returns it only when the caller has opted out of fallback (future
    /// extension).
    #[error("old subscription not found — falling back to fresh subscribe")]
    OldSubscriptionNotFound,

    /// RPC transport / preflight failure. Wraps the underlying
    /// `solana_rpc_client_api::client_error::Error` for `.kind()` inspection.
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcClientError),

    /// Account decode failed — owner mismatch, wrong discriminator, or
    /// truncated body. ADR-015 §F5 chokepoint.
    #[error("decode error: {0}")]
    Decode(#[from] AccountDecodeError),

    /// Borsh serialization of instruction args failed. Practically
    /// unreachable for a single-`u8` arg; kept for symmetry with
    /// resubscribe.rs error taxonomy.
    #[error("borsh serialize: {0}")]
    Borsh(#[from] std::io::Error),
}

/// Build the `cancel` ix per ADR-013 + ADR-009 Accounts layout.
///
/// Account order (matches `nakama/programs/nakama/src/instructions/cancel.rs::Cancel`):
///   0. signer            (mut, signer — subscriber in our flow per Q1)
///   1. subscription      (mut)
///   2. subscriber        (mut, UncheckedAccount, address-pinned to snapshot)
///   3. vault             (mut)
///   4. merchant_ata      (mut)
///   5. subscriber_ata    (mut)
///   6. token_program     (readonly)
///   7. graced_subscription (Option, trailing)
///   8. paused_subscription (Option, trailing)
///
/// `allow-missing-optionals` is enabled on the program crate, so trailing
/// `None`s collapse off the wire. For `Some` mid-list (graced=Some,
/// paused=None) we omit only paused; for graced=None+paused=Some we pass
/// the program_id sentinel for graced (Anchor encodes None-non-trailing
/// as the program_id pubkey — verified via `anchor-lang-1.0.2/src/accounts/option.rs`).
#[allow(clippy::too_many_arguments)]
fn build_cancel_ix(
    program_id: &Pubkey,
    signer: &Pubkey,
    subscription: &Pubkey,
    subscriber: &Pubkey,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
    graced: Option<Pubkey>,
    paused: Option<Pubkey>,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(*signer, true),
        AccountMeta::new(*subscription, false),
        AccountMeta::new(*subscriber, false),
        AccountMeta::new(*vault, false),
        AccountMeta::new(*merchant_ata, false),
        AccountMeta::new(*subscriber_ata, false),
        AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
    ];
    // Trailing-optional packing rules (Anchor 1.0 + allow-missing-optionals):
    //   - If both are None → omit both, accounts.len() == 7.
    //   - If graced=Some, paused=None → push graced only, len == 8.
    //   - If paused=Some (any graced) → push graced slot (sentinel if None)
    //     then paused. We mirror TS `cancel.ts` which only ever passes
    //     graced; the paused path is reserved for the FSM=Paused branch.
    match (graced, paused) {
        (None, None) => {}
        (Some(g), None) => {
            accounts.push(AccountMeta::new(g, false));
        }
        (g_opt, Some(p)) => {
            // Graced slot must be present (sentinel = program_id when None)
            // because paused is a later-declared field.
            let g_meta = match g_opt {
                Some(g) => AccountMeta::new(g, false),
                None => AccountMeta::new_readonly(*program_id, false),
            };
            accounts.push(g_meta);
            accounts.push(AccountMeta::new(p, false));
        }
    }
    Instruction {
        program_id: *program_id,
        accounts,
        data: cancel_discriminator().to_vec(),
    }
}

/// Build the `subscribe(plan_v2)` ix. Identical account order to
/// `resubscribe.rs::build_subscribe_ix` — duplicated here as a private
/// helper to keep this module self-contained. DRY pressure is low: if a
/// third consumer appears, fold to a shared builder module.
fn build_subscribe_ix(
    program_id: &Pubkey,
    subscriber: &Pubkey,
    plan: &Pubkey,
    token_mint: &Pubkey,
    periods_to_prefund: u8,
) -> Result<Instruction, ChangeRateError> {
    let (subscription, _) = derive_subscription_pda(program_id, subscriber, plan);
    let (vault, _) = derive_vault_pda(program_id, &subscription);
    let subscriber_ata = get_associated_token_address(subscriber, token_mint);
    let mut data = subscribe_discriminator().to_vec();
    borsh::BorshSerialize::serialize(&periods_to_prefund, &mut data)?;
    Ok(Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*subscriber, true),
            AccountMeta::new_readonly(*plan, false),
            AccountMeta::new_readonly(*token_mint, false),
            AccountMeta::new(subscription, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(subscriber_ata, false),
            AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSVAR_RENT_ID, false),
        ],
        data,
    })
}

/// Build the `cleanup` ix (ADR-013). Two accounts: subscription (mut,
/// closed by Anchor) + subscriber (mut, signer — rent recipient).
fn build_cleanup_ix(
    program_id: &Pubkey,
    subscription: &Pubkey,
    subscriber: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*subscription, false),
            AccountMeta::new(*subscriber, true),
        ],
        data: cleanup_discriminator().to_vec(),
    }
}

/// Build the `close_session` ix (ADR-x402-001). Mirrors
/// `resubscribe.rs::build_close_session_ix`.
fn build_close_session_ix(
    program_id: &Pubkey,
    parent: &Pubkey,
    pay_session: &Pubkey,
    subscriber: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*parent, false),
            AccountMeta::new(*pay_session, false),
            AccountMeta::new(*subscriber, true),
        ],
        data: close_session_discriminator().to_vec(),
    }
}

/// Build the ADR-005 change-rate composite Transaction (unsigned).
///
/// The returned `Transaction` has the subscriber wired as fee payer; the
/// caller signs by calling `tx.sign(&[&subscriber_keypair], blockhash)`
/// or `tx.try_sign(...)` and submits via their preferred path. Mirrors the
/// TS helper's "build but don't sign" contract.
///
/// Q11 fallback semantics: when the old Subscription PDA doesn't exist
/// (RPC returns `None`), we emit a single `subscribe(new_plan)` ix and
/// return success. The caller still gets a usable migration tx — they
/// just paid for one less ix than they expected. We do NOT surface
/// `OldSubscriptionNotFound` as an `Err` here because the SDK contract is
/// "give me a migration tx", and a fresh subscribe IS the migration when
/// there was nothing to migrate from.
pub async fn build_change_rate_tx(
    rpc: &RpcClient,
    opts: &ChangeRateOptions,
    recent_blockhash: Hash,
) -> Result<Transaction, ChangeRateError> {
    let commitment = CommitmentConfig::confirmed();

    // Step 1 — fetch both Plans, owner-check, same-mint guard (Q5).
    // We deliberately use two roundtrips rather than `get_multiple_accounts`
    // because the helper is invoked once per migration (not in a hot loop)
    // and the simpler call site is worth the latency budget.
    let old_plan_acct = rpc
        .get_account_with_commitment(&opts.old_plan, commitment)
        .await?
        .value
        .ok_or(AccountDecodeError::TooShort)?;
    let new_plan_acct = rpc
        .get_account_with_commitment(&opts.new_plan, commitment)
        .await?
        .value
        .ok_or(AccountDecodeError::TooShort)?;
    let old_plan_view = PlanView::decode_owned(&old_plan_acct, &opts.program_id)?;
    let new_plan_view = PlanView::decode_owned(&new_plan_acct, &opts.program_id)?;
    if old_plan_view.token_mint != new_plan_view.token_mint {
        return Err(ChangeRateError::CrossMintMigrationUnsupported);
    }

    // Step 2 — observe old Subscription. Q11 fall-through: PDA absent →
    // emit fresh subscribe only.
    let (old_sub_pda, _) =
        derive_subscription_pda(&opts.program_id, &opts.subscriber, &opts.old_plan);
    let sub_response = rpc
        .get_account_with_commitment(&old_sub_pda, commitment)
        .await?;
    let old_sub_view: Option<SubscriptionView> = match sub_response.value {
        None => None,
        Some(account) => Some(SubscriptionView::decode_owned(&account, &opts.program_id)?),
    };

    // Compose the ix sequence. Capacity: worst case 4 close_session + cancel
    // + cleanup + subscribe = 7 entries.
    let mut ixs: Vec<Instruction> = Vec::with_capacity(7);

    if let Some(sub) = old_sub_view {
        // Step 3 — enumerate alive PaySessions (Q7) if requested.
        if opts.close_alive_pay_sessions {
            let alive = list_alive_pay_sessions(rpc, &opts.program_id, &old_sub_pda)
                .await
                .map_err(|e| match e {
                    // Bridge the two error enums: resubscribe's Rpc/Decode
                    // collapse to our equivalents; the resubscribe-specific
                    // envelope guard and SubscriptionAlive paths are not
                    // reachable from `list_alive_pay_sessions`.
                    crate::resubscribe::ResubscribeError::Rpc(r) => ChangeRateError::Rpc(r),
                    crate::resubscribe::ResubscribeError::AccountDecode(d) => {
                        ChangeRateError::Decode(d)
                    }
                    other => ChangeRateError::Decode(AccountDecodeError::Borsh(
                        std::io::Error::other(other.to_string()),
                    )),
                })?;
            if alive.len() > MAX_INLINE_PAY_SESSION_CLOSES {
                return Err(ChangeRateError::TooManyAliveSessions { count: alive.len() });
            }
            for (pda, view) in alive {
                // Belt-and-suspenders PDA re-derivation (mirrors resubscribe).
                let (derived, _) =
                    derive_pay_session_pda(&opts.program_id, &old_sub_pda, view.session_id);
                if derived == pda {
                    ixs.push(build_close_session_ix(
                        &opts.program_id,
                        &old_sub_pda,
                        &pda,
                        &opts.subscriber,
                    ));
                }
            }
        }

        // Step 4 — cancel(old_sub). Account derivations come from the
        // snapshotted Subscription fields (ADR-001: merchant_ata is the
        // canonical authoritative source post-subscribe).
        let (vault_pda, _) = derive_vault_pda(&opts.program_id, &old_sub_pda);
        let subscriber_ata = get_associated_token_address(&opts.subscriber, &sub.token_mint);
        // ADR-006/007: optional satellites driven by current FSM byte.
        // State byte: 0=Active 1=Paused 2=GracePeriod (3=Exhausted /
        // 4=Cancelled are not legal cancel sources per cancel handler).
        let graced = if sub.state == 2 {
            Some(derive_grace_pda(&opts.program_id, &old_sub_pda).0)
        } else {
            None
        };
        let paused = if sub.state == 1 {
            Some(derive_paused_sub_pda(&opts.program_id, &old_sub_pda).0)
        } else {
            None
        };
        ixs.push(build_cancel_ix(
            &opts.program_id,
            &opts.subscriber,
            &old_sub_pda,
            &opts.subscriber,
            &vault_pda,
            &sub.merchant_ata,
            &subscriber_ata,
            graced,
            paused,
        ));

        // Step 5 — cleanup(old_sub) tombstone (ADR-013).
        ixs.push(build_cleanup_ix(
            &opts.program_id,
            &old_sub_pda,
            &opts.subscriber,
        ));
    }
    // Step 6 — subscribe(new_plan). Always emitted, even on Q11 fallback.
    ixs.push(build_subscribe_ix(
        &opts.program_id,
        &opts.subscriber,
        &opts.new_plan,
        &new_plan_view.token_mint,
        opts.new_deposit_periods,
    )?);

    // Assemble unsigned Transaction.
    let message = Message::new_with_blockhash(&ixs, Some(&opts.subscriber), &recent_blockhash);
    Ok(Transaction::new_unsigned(message))
}

#[cfg(test)]
mod tests {
    //! Unit-only coverage. End-to-end LiteSVM coverage is sdk-engineer +
    //! test-engineer territory per ADR-005 §"Tests".

    use super::*;
    use borsh::BorshDeserialize;

    #[test]
    fn default_options_close_pay_sessions_true() {
        let opts = ChangeRateOptions::default();
        assert!(opts.close_alive_pay_sessions);
        assert_eq!(opts.new_deposit_periods, 1);
    }

    #[test]
    fn cancel_ix_omits_both_optionals_in_active_case() {
        let pk = Pubkey::new_from_array([1u8; 32]);
        let ix = build_cancel_ix(&pk, &pk, &pk, &pk, &pk, &pk, &pk, None, None);
        assert_eq!(ix.accounts.len(), 7, "Active: trailing optionals dropped");
    }

    #[test]
    fn cancel_ix_packs_graced_only() {
        let pk = Pubkey::new_from_array([1u8; 32]);
        let g = Pubkey::new_from_array([2u8; 32]);
        let ix = build_cancel_ix(&pk, &pk, &pk, &pk, &pk, &pk, &pk, Some(g), None);
        assert_eq!(ix.accounts.len(), 8);
        assert_eq!(ix.accounts[7].pubkey, g);
    }

    #[test]
    fn cancel_ix_packs_paused_with_graced_sentinel() {
        let pk = Pubkey::new_from_array([1u8; 32]); // doubles as program_id sentinel
        let p = Pubkey::new_from_array([3u8; 32]);
        let ix = build_cancel_ix(&pk, &pk, &pk, &pk, &pk, &pk, &pk, None, Some(p));
        assert_eq!(ix.accounts.len(), 9);
        // Graced slot is program_id sentinel (None-non-trailing encoding).
        assert_eq!(ix.accounts[7].pubkey, pk);
        assert_eq!(ix.accounts[8].pubkey, p);
    }

    #[test]
    fn subscribe_ix_arg_borsh_round_trip() {
        // Sanity: discriminator + single u8 arg = 9 bytes data.
        let pk = Pubkey::new_from_array([1u8; 32]);
        let ix = build_subscribe_ix(&pk, &pk, &pk, &pk, 3).unwrap();
        assert_eq!(ix.data.len(), 8 + 1);
        assert_eq!(ix.data[8], 3);
        assert_eq!(&ix.data[..8], &subscribe_discriminator()[..]);
    }

    /// Sanity: error display strings cite the ADR for grep-ability.
    #[test]
    fn cross_mint_error_message_cites_adr() {
        let e = ChangeRateError::CrossMintMigrationUnsupported;
        assert!(e.to_string().contains("ADR-005"));
    }

    /// Compile-time guard: `BorshDeserialize` is the trait actually used by
    /// PlanView. If `accounts::PlanView` ever loses the derive, this fails to
    /// compile rather than at first RPC fetch.
    #[test]
    fn plan_view_decodes_minimal_body() {
        use borsh::BorshSerialize;
        let pk = Pubkey::new_from_array([7u8; 32]);
        let mut body = Vec::new();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&1u64, &mut body).unwrap();
        BorshSerialize::serialize(&100u64, &mut body).unwrap();
        BorshSerialize::serialize(&86_400i64, &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&0u8, &mut body).unwrap();
        BorshSerialize::serialize(&[0u8; 32], &mut body).unwrap();
        assert_eq!(body.len(), 153, "Plan body must be 153 bytes per ADR-001");
        let decoded = PlanView::try_from_slice(&body).expect("decode");
        assert_eq!(decoded.token_mint.to_bytes(), pk.to_bytes());
        assert_eq!(decoded.period, 86_400);
    }
}
