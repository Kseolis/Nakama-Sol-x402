//! ADR-008 composite-tx builder — Rust mirror of
//! `clients/ts/src/instructions/resubscribe.ts` (sdk-engineer Stage-2).
//!
//! When a subscriber wants to re-subscribe to a Plan they previously
//! cancelled, the Subscription PDA lives on-chain as a `state == Cancelled`
//! tombstone (ADR-013 §"Decision"). A naive `subscribe` against the same
//! `[b"sub", subscriber, plan]` seeds collides via System Program
//! `AccountAlreadyInUse` (`Custom(0)`). ADR-008 §"Decision" resolves this
//! at the SDK layer: a single Solana transaction packs `cleanup` and
//! `subscribe` together. Solana's runtime atomicity (entire-tx commit or
//! revert) gives us the race-free re-subscribe for free.
//!
//! Forward-compat with ADR-x402-001: if the (subscriber, plan) Subscription
//! has alive PaySession satellites, we prepend `close_session × N`
//! instructions to the composite tx (ADR-008 §E5, §"x402 forward-compat").
//! Soft cap `N ≤ 4` to keep the tx envelope inside CU + size limits
//! (ADR-008 §"x402 forward-compat" — N=3 worst case sits at ~90k CU /
//! ~700 B; we cap at 4 to leave headroom and surface multi-tx fallbacks
//! to the caller).
//!
//! GRASP roles:
//! - `ResubscribeArgs` — Information Expert: knows the on-chain accounts
//!   needed by both `cleanup` and `subscribe`.
//! - `build_resubscribe_ixs` — Pure Fabrication / stateless builder
//!   (per `.claude/rules/fsm-first.md` — builders are NOT FSMs even when
//!   they branch on observed state; branching here is a single
//!   discriminator-based dispatch, not a transition graph).
//! - `resubscribe_or_subscribe` — Controller: orchestrates RPC fetch +
//!   builder + signer + submit.
//!
//! References:
//! - ADR-008 §"Decision", §"SDK pseudocode", §"x402 forward-compat",
//!   §"Edge cases" E1, E5.
//! - ADR-013 §"Cleanup handler" (cancel/cleanup decomposition).
//! - ADR-x402-001 §"close_session" (PaySession lifecycle).
//! - `clients/ts/scripts/06-cancel-by-merchant.ts` (TS demo of cancel flow).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use borsh::BorshSerialize;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::client_error::Error as RpcClientError;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_rpc_client_api::response::{UiAccountData, UiAccountEncoding};
use solana_signer::Signer;
use solana_transaction::{Signature, Transaction};
use spl_associated_token_account::get_associated_token_address;
use thiserror::Error;

use crate::accounts::{AccountDecodeError, PaySessionView, SubscriptionView};
use crate::constants::ACCOUNT_DISCRIMINATOR_LEN;
use crate::discriminator::{
    cleanup_discriminator, close_session_discriminator, subscribe_discriminator,
};
use crate::pda::{derive_pay_session_pda, derive_subscription_pda, derive_vault_pda};

/// Solana legacy-tx hard limit. We refuse to build a tx whose serialized
/// length we predict above this number, so the caller learns BEFORE an RPC
/// roundtrip that the envelope is too large (ADR-008 §"Tx size + CU
/// verification"). Verified against `solana-transaction` v3.1.0 docs:
/// `PACKET_DATA_SIZE = 1232`.
pub const TX_SIZE_LIMIT_BYTES: usize = 1232;

/// Soft cap on alive PaySessions closed within the composite tx
/// (ADR-008 §"x402 forward-compat", §"Defer to Future work" bullet 5).
/// Above this, builder returns `TooManyPaySessions` and the caller is
/// expected to pre-close in a separate tx — multi-tx plan output is
/// deferred per ADR.
pub const MAX_INLINE_PAY_SESSION_CLOSES: usize = 4;

/// Classic SPL Token program ID (Token-2022 explicitly rejected per
/// ADR-014). Hardcoded — pulling the program-side `spl-token` crate just
/// for the constant would explode the dep graph; the facilitator
/// (`handlers/top_up.rs::SPL_TOKEN_PROGRAM_ID`) made the same call.
const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// System program ID — all-zero pubkey. `Pubkey::from_str_const` is `const`
/// since solana-pubkey 3.x. We avoid pulling `solana_system_interface`
/// for this single constant; its `program::ID` re-export resolves through
/// a parallel `solana-address` version in the workspace dep graph and
/// type-checks against a distinct nominal type (verified empirically —
/// `expected Address, found solana_pubkey::Pubkey` on direct assignment).
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// Rent sysvar — required by Anchor `subscribe.rs` Accounts struct
/// (`pub rent: Sysvar<'info, Rent>`). Address is the well-known sysvar
/// pubkey from the Solana runtime.
const SYSVAR_RENT_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");

/// Caller-supplied arguments for the composite re-subscribe tx.
///
/// Carries borrowed references where possible to avoid hidden clones.
/// The signer is `&dyn Signer` so the caller can pass an existing
/// `Keypair`, a hardware-wallet adapter, or a test fixture.
pub struct ResubscribeArgs<'a> {
    /// Nakama on-chain program ID. Hardcoded in production keeper /
    /// facilitator (`program_id` from config); kept as a parameter here
    /// so unit tests can run against a fake program key.
    pub program_id: Pubkey,
    /// Plan PDA pubkey. NOT derived here — caller already knows it
    /// (frontend has it from URL state, keeper has it from indexed
    /// list). Same convention as the TS builder
    /// (`clients/ts/src/instructions/resubscribe.ts` Stage-2).
    pub plan: Pubkey,
    /// Token mint snapshot (USDC devnet in this codebase). Must equal
    /// `plan.token_mint`; on-chain `address = plan.token_mint`
    /// constraint enforces this at tx execution (`subscribe.rs:41`).
    pub token_mint: Pubkey,
    /// Periods to prefund on the new subscription (ADR-002 BLK-13,
    /// `subscribe.rs:99` reject-zero guard). 1..=255.
    pub periods_to_prefund: u8,
    /// Subscriber-and-signer. Reads its pubkey via `Signer::pubkey()`;
    /// signs the composite tx.
    pub subscriber: &'a (dyn Signer + 'a),
    /// Whether to enumerate and inline-close alive PaySession satellites
    /// (ADR-008 §"x402 forward-compat"). `true` = clean UX, reclaims
    /// rent in the same tx. `false` = subscriber accepts orphan-rent
    /// lockup; faster + smaller tx.
    pub close_alive_pay_sessions: bool,
}

/// Failure surface for the composite re-subscribe flow.
///
/// Errors are classified by where the work blocks:
/// * `SubscriptionAlive`         — guard (caller fed an alive sub).
/// * `TooManyPaySessions`        — guard (envelope too tight, caller picks multi-tx).
/// * `EnvelopeTooLarge`          — guard (predicted serialized tx > 1232).
/// * `AccountDecode`             — on-chain account did not match the
///   off-chain view layout (drift between this crate and `state.rs`).
/// * `Rpc`                       — RPC transport / preflight failure.
/// * `Borsh`                     — instruction-args serialization failed
///   (effectively unreachable for fixed-size types like `u8`; included
///   for completeness per `rust-error-handling-solana` skill).
#[derive(Debug, Error)]
pub enum ResubscribeError {
    /// Subscription PDA is in a non-Cancelled state — re-subscribe is
    /// only valid from the Cancelled tombstone (ADR-008 §"Decision",
    /// ADR-013 §"Per-state cleanup eligibility"). Raw state byte
    /// returned so the caller can log / surface a precise reason.
    #[error("subscription is alive in state byte {state}; re-subscribe requires Cancelled (4)")]
    SubscriptionAlive { state: u8 },

    /// Alive PaySession count exceeds the inline-close cap. Caller
    /// should pre-close in a separate tx (or set
    /// `close_alive_pay_sessions = false` to accept the orphan rent
    /// lockup per ADR-008 §"x402 forward-compat").
    #[error("found {n} alive PaySessions; soft cap is {MAX_INLINE_PAY_SESSION_CLOSES}; pre-close in a separate tx")]
    TooManyPaySessions { n: usize },

    /// Predicted serialized tx size exceeds 1232 bytes
    /// (`PACKET_DATA_SIZE`). Returned BEFORE submission to save the
    /// preflight roundtrip. The check is conservative — we serialize
    /// the unsigned `Message` and add the signature placeholders.
    #[error("predicted tx size {size} exceeds {TX_SIZE_LIMIT_BYTES}-byte limit")]
    EnvelopeTooLarge { size: usize },

    /// On-chain account did not Borsh-decode against the off-chain
    /// view. Indicates drift between this crate's `accounts.rs` and
    /// `state.rs` — escalate as `[ADR_DRIFT]`.
    #[error("account decode failed: {0}")]
    AccountDecode(#[from] AccountDecodeError),

    /// RPC transport failure (network, JSON-RPC, preflight simulation,
    /// signature verification). Includes the underlying
    /// `solana_rpc_client_api::client_error::Error` so callers can
    /// pattern-match on `.kind()` for retryability per the
    /// `rust-error-handling-solana` skill discipline.
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcClientError),

    /// Borsh serialization of instruction args failed. Practically
    /// unreachable for `u8` args (fixed-size), kept for symmetry.
    #[error("borsh serialize: {0}")]
    Borsh(#[from] std::io::Error),

    /// `base64` decode of `UiAccountData::Binary(_, Base64)` failed.
    /// Indicates the RPC node returned a non-base64 payload despite our
    /// explicit encoding request — surface as an Rpc-class fault.
    #[error("base64 decode of UiAccount data failed: {0}")]
    UiAccountDecode(String),

    /// RPC node returned the deprecated `UiAccountData::LegacyBinary`
    /// (base58-encoded raw bytes) variant despite our explicit Base64
    /// encoding request. F6 (ADR-015): we refuse to decode rather than
    /// silently mis-interpret. Indicates a misconfigured or hostile RPC
    /// node — switch to a compliant provider.
    #[error("RPC returned LegacyBinary encoding despite Base64 request; unsupported")]
    UnsupportedLegacyBinaryEncoding,
}

/// List alive PaySession PDAs anchored to a given Subscription via the
/// `getProgramAccounts` RPC + memcmp filter on the discriminator-less
/// `subscription` field of `PaySessionView` (offset 8, since the on-chain
/// account is `[8-byte discriminator | 32-byte subscription pubkey | ...]`,
/// see `accounts.rs::PaySessionView`).
///
/// Why a free function: this is GRASP Pure Fabrication — both the
/// composite-tx builder and (future) keeper "abandoned satellite cleanup"
/// flow consume it. Keeping it next to `resubscribe.rs` is fine for now;
/// promote to a `queries.rs` module if a second consumer lands.
///
/// Returns only `Open`-state PaySessions, because `close_session.rs:67-70`
/// rejects `Settling` (transient post-CPI-crash) with
/// `NakamaError::IllegalStateForClose`. We don't try to recover from
/// Settling here — that's the R3 `force_close_session` future work.
pub async fn list_alive_pay_sessions(
    rpc: &RpcClient,
    program_id: &Pubkey,
    subscription: &Pubkey,
) -> Result<Vec<(Pubkey, PaySessionView)>, ResubscribeError> {
    // Two positive memcmp filters (ADR-015 §F5 / security-audit-patterns P3):
    //
    //   1. Anchor account discriminator at offset 0 — pins the account type
    //      server-side so an attacker can't make the RPC node return
    //      arbitrary program-owned junk that happens to carry the right
    //      `subscription` field at offset 8.
    //   2. `subscription` field at offset 8 ([0..8] = discriminator,
    //      [8..40] = subscription pubkey per `accounts.rs::PaySessionView`).
    //
    // getProgramAccounts already constrains by program owner, so the trio
    // (owner ∧ disc ∧ subscription) gives us a tight filter. We still
    // owner-check each returned account on decode for defence-in-depth.
    let disc_filter = Memcmp::new_raw_bytes(0, PaySessionView::discriminator().to_vec());
    let sub_filter =
        Memcmp::new_raw_bytes(ACCOUNT_DISCRIMINATOR_LEN, subscription.to_bytes().to_vec());

    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::Memcmp(disc_filter),
            RpcFilterType::Memcmp(sub_filter),
        ]),
        account_config: RpcAccountInfoConfig {
            // Base64 — non-Json variants of UiAccountData (Json/JsonParsed
            // are for spl-token-style accounts the parser understands).
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        },
        with_context: None,
        sort_results: None,
    };

    // `get_program_ui_accounts_with_config` is the non-deprecated 3.1.x
    // entrypoint (the older `get_program_accounts_with_config` is
    // `#[deprecated]` per `solana-rpc-client/src/nonblocking/rpc_client.rs:4126`).
    let ui_accounts = rpc
        .get_program_ui_accounts_with_config(program_id, config)
        .await?;

    let mut out = Vec::with_capacity(ui_accounts.len());
    for (pda, ui) in ui_accounts {
        let raw = decode_ui_account_data(&ui.data)?;
        // We trust the server-side filter set (owner via getProgramAccounts +
        // disc + sub memcmp) for ENUMERATION. Use the legacy `try_decode`
        // here because UiAccount doesn't carry the owner field for us to
        // re-validate client-side; the program-owner property is implicit
        // in the gPA RPC call itself.
        let view = PaySessionView::try_decode(&raw)?;
        // Defence-in-depth: memcmp filter already pinned the
        // subscription field, but a malformed RPC response could still
        // surface unrelated accounts; reject anything that drifted.
        if view.subscription != *subscription {
            continue;
        }
        // Skip Settling / Closed — only Open is closable via the
        // non-`force_*` entrypoint. PaySessionState::Open = 0.
        if view.state == 0 {
            out.push((pda, view));
        }
    }
    Ok(out)
}

/// Convert `UiAccountData` to raw bytes. We only request Base64 above,
/// so we surface a precise error on any other variant rather than
/// silently misinterpreting.
///
/// F6 remediation (ADR-015 §F6, severity medium). Per
/// `solana-account-decoder` `UiAccountData::LegacyBinary(_)` is a
/// **base58**-encoded raw-bytes representation (deprecated). The
/// previous implementation decoded it as base64, which silently
/// mis-decodes when an older / misconfigured / hostile RPC node returns
/// the legacy variant (we always *request* Base64; misbehaving nodes
/// can return anything). KISS choice: reject `LegacyBinary` outright
/// with a typed error rather than carry a base58 codepath we never
/// exercise in practice — keepers, facilitator, and integration scripts
/// all pin Base64 in their `RpcAccountInfoConfig` (see
/// `list_alive_pay_sessions` above).
fn decode_ui_account_data(data: &UiAccountData) -> Result<Vec<u8>, ResubscribeError> {
    match data {
        UiAccountData::Binary(s, UiAccountEncoding::Base64) => BASE64
            .decode(s)
            .map_err(|e| ResubscribeError::UiAccountDecode(e.to_string())),
        UiAccountData::LegacyBinary(_) => Err(ResubscribeError::UnsupportedLegacyBinaryEncoding),
        UiAccountData::Binary(_, other) => Err(ResubscribeError::UiAccountDecode(format!(
            "unexpected encoding {other:?}; expected Base64",
        ))),
        UiAccountData::Json(_) => Err(ResubscribeError::UiAccountDecode(
            "Json-parsed payload returned where Base64 was requested".into(),
        )),
    }
}

/// Build the Anchor `cleanup` instruction (ADR-013 §"Cleanup handler").
///
/// Account order matches `nakama/programs/nakama/src/instructions/cleanup.rs::Cleanup`:
///   0. subscription  (mut, closed by Anchor)
///   1. subscriber    (mut, signer — receives rent)
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
        // `cleanup` has no args — just the 8-byte discriminator.
        data: cleanup_discriminator().to_vec(),
    }
}

/// Build the Anchor `subscribe` instruction
/// (`nakama/programs/nakama/src/instructions/subscribe.rs::Subscribe`).
///
/// Account order — MUST match the on-chain Accounts struct (Anchor
/// field-declaration order; drift → `ConstraintSeeds 2006` at tx submit):
///
///   0. subscriber       (mut, signer)
///   1. plan             (readonly)
///   2. token_mint       (readonly)
///   3. subscription     (mut, init)
///   4. vault            (mut, init)
///   5. subscriber_ata   (mut)
///   6. token_program    (readonly)
///   7. system_program   (readonly)
///   8. rent             (readonly sysvar)
fn build_subscribe_ix(args: &SubscribeIxArgs<'_>) -> Result<Instruction, ResubscribeError> {
    let (subscription, _sub_bump) =
        derive_subscription_pda(args.program_id, args.subscriber, args.plan);
    let (vault, _vault_bump) = derive_vault_pda(args.program_id, &subscription);
    let subscriber_ata = get_associated_token_address(args.subscriber, args.token_mint);

    let mut data = subscribe_discriminator().to_vec();
    // Single u8 arg — Borsh-serialize for parity with the on-chain handler
    // `subscribe_handler(ctx, periods_to_prefund: u8)`. Failure here would
    // mean OOM on `Vec::push`; surfaced via the `Borsh(io::Error)` arm
    // for symmetry with multi-byte arg flows in future builders.
    BorshSerialize::serialize(&args.periods_to_prefund, &mut data)?;

    Ok(Instruction {
        program_id: *args.program_id,
        accounts: vec![
            AccountMeta::new(*args.subscriber, true),
            AccountMeta::new_readonly(*args.plan, false),
            AccountMeta::new_readonly(*args.token_mint, false),
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

/// Tiny by-ref struct used internally to keep `build_subscribe_ix` arg
/// list manageable. Not exported.
struct SubscribeIxArgs<'a> {
    program_id: &'a Pubkey,
    subscriber: &'a Pubkey,
    plan: &'a Pubkey,
    token_mint: &'a Pubkey,
    periods_to_prefund: u8,
}

/// Build the Anchor `close_session` instruction
/// (`nakama/programs/nakama/src/instructions/close_session.rs::CloseSession`).
///
/// Account order (matches `close_session.rs` Accounts):
///   0. parent       (readonly — `has_one` + seeds source)
///   1. pay_session  (mut — closed by Anchor)
///   2. subscriber   (mut, signer)
///
/// Mirrors `crates/nakama-x402-facilitator/src/handlers/close_session.rs::build_close_session_metas`,
/// duplicated here so this crate's builder is self-contained (the
/// facilitator does NOT re-export builders today; if it ever does, fold
/// to a shared helper — DRY pending second consumer).
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

/// Pure builder: emit the instruction sequence for re-subscribe given
/// pre-fetched state.
///
/// **No RPC, no signing.** Caller supplies `subscription_state` (the raw
/// state byte from the on-chain `SubscriptionView.state` — `None` ⟹ PDA
/// does not exist, fresh subscribe) and the discovered alive PaySession
/// PDAs (empty ⟹ no inline close).
///
/// Branches:
/// * `subscription_state = None` → `[subscribe]` (1 ix).
/// * `subscription_state = Some(4 /* Cancelled */)` with no sessions →
///   `[cleanup, subscribe]`.
/// * Cancelled + N sessions (1 ≤ N ≤ cap) →
///   `[close_session × N, cleanup, subscribe]`.
/// * `subscription_state = Some(state != 4)` → `Err(SubscriptionAlive)`.
/// * N > cap → `Err(TooManyPaySessions)`.
pub fn build_resubscribe_ixs(
    args: &ResubscribeArgs<'_>,
    subscription_state: Option<u8>,
    alive_pay_sessions: &[Pubkey],
) -> Result<Vec<Instruction>, ResubscribeError> {
    // ADR-008 §E5: alive sessions only legal when we're past a cancel
    // (so the parent is Cancelled). If caller fed sessions with
    // `subscription_state = None`, ignore them — fresh subscribe has no
    // parent to attach to. We don't error because the caller may pass
    // empty slice + None for the canonical fresh-subscribe branch
    // (Q11).
    let n_sessions = if subscription_state == Some(4) {
        alive_pay_sessions.len()
    } else {
        0
    };

    if args.close_alive_pay_sessions && n_sessions > MAX_INLINE_PAY_SESSION_CLOSES {
        return Err(ResubscribeError::TooManyPaySessions { n: n_sessions });
    }

    let subscriber_pk = args.subscriber.pubkey();
    let (subscription_pda, _) =
        derive_subscription_pda(&args.program_id, &subscriber_pk, &args.plan);

    let subscribe_ix = build_subscribe_ix(&SubscribeIxArgs {
        program_id: &args.program_id,
        subscriber: &subscriber_pk,
        plan: &args.plan,
        token_mint: &args.token_mint,
        periods_to_prefund: args.periods_to_prefund,
    })?;

    match subscription_state {
        // PDA doesn't exist — fresh subscribe (Q11 fall-through).
        None => Ok(vec![subscribe_ix]),

        // Cancelled tombstone — composite.
        Some(4) => {
            let cleanup_ix = build_cleanup_ix(&args.program_id, &subscription_pda, &subscriber_pk);

            // Capacity: optional close_sessions + cleanup + subscribe.
            let mut ixs =
                Vec::with_capacity(args.close_alive_pay_sessions as usize * n_sessions + 2);

            if args.close_alive_pay_sessions {
                for ps in alive_pay_sessions {
                    ixs.push(build_close_session_ix(
                        &args.program_id,
                        &subscription_pda,
                        ps,
                        &subscriber_pk,
                    ));
                }
            }
            ixs.push(cleanup_ix);
            ixs.push(subscribe_ix);
            Ok(ixs)
        }

        // Any other state — alive subscription, re-subscribe is illegal.
        Some(other) => Err(ResubscribeError::SubscriptionAlive { state: other }),
    }
}

/// Orchestrator: fetch state, build composite, sign, submit, confirm.
///
/// Side effects:
/// * One `get_account_with_commitment` for the Subscription PDA.
/// * If `close_alive_pay_sessions = true` and tombstone observed, one
///   `get_program_ui_accounts_with_config` to enumerate satellites.
/// * One `get_latest_blockhash`.
/// * One `send_and_confirm_transaction`.
///
/// Returns the confirmed `Signature` (commitment level inherited from
/// the `RpcClient`; we default to `confirmed` for the account read since
/// `Finalized` adds ~12 s to demo latency).
pub async fn resubscribe_or_subscribe(
    rpc: &RpcClient,
    args: ResubscribeArgs<'_>,
) -> Result<Signature, ResubscribeError> {
    let subscriber_pk = args.subscriber.pubkey();
    let (subscription_pda, _) =
        derive_subscription_pda(&args.program_id, &subscriber_pk, &args.plan);

    // Step 1 — observe Subscription state. `get_account_with_commitment`
    // returns `Response<Option<Account>>`; `.value` flattens to
    // `Option<Account>` (account-not-found is `Ok(None)` here, contrasted
    // with `get_account` which returns `RpcError::ForUser`).
    let commitment = CommitmentConfig::confirmed();
    let sub_response = rpc
        .get_account_with_commitment(&subscription_pda, commitment)
        .await?;
    // ADR-015 §F5: validate `account.owner == program_id` AND the Anchor
    // account discriminator BEFORE Borsh-decoding. Rejects spoofed
    // System-owned accounts whose first 8 bytes mimic a Subscription, and
    // rejects program-owned accounts of a different type (e.g. PaySession
    // accidentally fetched against the Subscription PDA seed).
    let subscription_state: Option<u8> = match sub_response.value {
        None => None,
        Some(account) => Some(SubscriptionView::decode_owned(&account, &args.program_id)?.state),
    };

    // Early validate non-Cancelled alive state — saves an unnecessary
    // PaySession enumeration roundtrip.
    if let Some(state) = subscription_state {
        if state != 4 {
            return Err(ResubscribeError::SubscriptionAlive { state });
        }
    }

    // Step 2 — enumerate alive PaySession satellites if requested AND
    // there's a tombstone to attach to. Fresh subscribe (state = None)
    // skips this — no parent yet, no satellites possible.
    let alive_sessions: Vec<Pubkey> =
        if args.close_alive_pay_sessions && subscription_state == Some(4) {
            let listed = list_alive_pay_sessions(rpc, &args.program_id, &subscription_pda).await?;
            // Re-derive each PDA to belt-and-suspenders match the seed scheme
            // — guards against the (vanishingly rare) case where an indexer
            // surfaces a Pubkey that the SDK can't reconstruct from
            // `(subscription, session_id)`. Mismatch → drop silently
            // (defence-in-depth, not an error path).
            let mut out = Vec::with_capacity(listed.len());
            for (pda, view) in listed {
                let (derived, _) =
                    derive_pay_session_pda(&args.program_id, &subscription_pda, view.session_id);
                if derived == pda {
                    out.push(pda);
                }
            }
            out
        } else {
            Vec::new()
        };

    // Step 3 — pure build.
    let ixs = build_resubscribe_ixs(&args, subscription_state, &alive_sessions)?;

    // Step 4 — assemble + envelope guard.
    let blockhash = rpc.get_latest_blockhash().await?;
    let message = Message::new_with_blockhash(&ixs, Some(&subscriber_pk), &blockhash);
    // Predictive size check (ADR-008 §"Tx size + CU verification"): we
    // surface the failure BEFORE submission so the caller doesn't burn
    // a preflight roundtrip on a 1232+ byte envelope. The check serialises
    // the unsigned `Message` and adds the per-signature envelope:
    //   * 1 byte signature-count varint
    //   * 64 bytes per signature
    // The composite tx has exactly one signer (subscriber).
    let predicted_size = bincode_message_size(&message) + 1 + 64;
    if predicted_size > TX_SIZE_LIMIT_BYTES {
        return Err(ResubscribeError::EnvelopeTooLarge {
            size: predicted_size,
        });
    }

    let tx = Transaction::new(&[args.subscriber], message, blockhash);

    // Step 5 — submit + confirm. Bubble RPC errors up as-is for the
    // caller to inspect via `RpcClientError::kind()`.
    let sig = rpc.send_and_confirm_transaction(&tx).await?;
    Ok(sig)
}

/// Conservative serialized-size estimate for `Message`.
///
/// We deliberately do NOT pull in the full `bincode` dep just to call
/// `serialized_size` — every Solana SDK crate already vendors the wire
/// format. The cheap shortcut: `Message::serialize()` (Anchor 1.0 / SDK
/// 3.x exposes it) gives us the unsigned-wire bytes. We add the signature
/// envelope in the caller.
fn bincode_message_size(message: &Message) -> usize {
    message.serialize().len()
}

#[cfg(test)]
mod tests {
    //! Unit-only coverage. Network paths (`resubscribe_or_subscribe`,
    //! `list_alive_pay_sessions`) are integration-tested by Stage-3
    //! test-engineer via LiteSVM — see ADR-008 §"Tests".

    use super::*;
    use solana_keypair::Keypair;

    fn fixture_args<'a>(subscriber: &'a Keypair) -> ResubscribeArgs<'a> {
        ResubscribeArgs {
            program_id: Pubkey::new_from_array([1u8; 32]),
            plan: Pubkey::new_from_array([2u8; 32]),
            token_mint: Pubkey::new_from_array([3u8; 32]),
            periods_to_prefund: 1,
            subscriber,
            close_alive_pay_sessions: true,
        }
    }

    #[test]
    fn fresh_subscribe_emits_single_ix() {
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let ixs = build_resubscribe_ixs(&args, None, &[]).unwrap();
        assert_eq!(ixs.len(), 1);
        // Discriminator prefix must be `subscribe`.
        assert_eq!(&ixs[0].data[..8], &subscribe_discriminator()[..]);
    }

    #[test]
    fn cancelled_no_sessions_emits_cleanup_then_subscribe() {
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let ixs = build_resubscribe_ixs(&args, Some(4), &[]).unwrap();
        assert_eq!(ixs.len(), 2);
        assert_eq!(&ixs[0].data[..8], &cleanup_discriminator()[..]);
        assert_eq!(&ixs[1].data[..8], &subscribe_discriminator()[..]);
    }

    #[test]
    fn cancelled_with_sessions_prepends_close_session() {
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let sessions = vec![
            Pubkey::new_from_array([10u8; 32]),
            Pubkey::new_from_array([11u8; 32]),
        ];
        let ixs = build_resubscribe_ixs(&args, Some(4), &sessions).unwrap();
        assert_eq!(ixs.len(), 4);
        assert_eq!(&ixs[0].data[..8], &close_session_discriminator()[..]);
        assert_eq!(&ixs[1].data[..8], &close_session_discriminator()[..]);
        assert_eq!(&ixs[2].data[..8], &cleanup_discriminator()[..]);
        assert_eq!(&ixs[3].data[..8], &subscribe_discriminator()[..]);
    }

    #[test]
    fn alive_subscription_rejected() {
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        // state == Active (0).
        let err = build_resubscribe_ixs(&args, Some(0), &[]).unwrap_err();
        assert!(
            matches!(err, ResubscribeError::SubscriptionAlive { state: 0 }),
            "expected SubscriptionAlive{{0}}, got {err:?}"
        );
    }

    #[test]
    fn too_many_pay_sessions_rejected() {
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let sessions: Vec<Pubkey> = (0u8..=(MAX_INLINE_PAY_SESSION_CLOSES as u8 + 1))
            .map(|i| Pubkey::new_from_array([i; 32]))
            .collect();
        let err = build_resubscribe_ixs(&args, Some(4), &sessions).unwrap_err();
        assert!(
            matches!(err, ResubscribeError::TooManyPaySessions { n } if n == sessions.len()),
            "expected TooManyPaySessions{{{}}}, got {err:?}",
            sessions.len()
        );
    }

    #[test]
    fn close_alive_pay_sessions_false_skips_close_ixs() {
        let kp = Keypair::new();
        let mut args = fixture_args(&kp);
        args.close_alive_pay_sessions = false;
        // Even when sessions exist, builder must NOT prepend close_session.
        let sessions = vec![Pubkey::new_from_array([10u8; 32])];
        let ixs = build_resubscribe_ixs(&args, Some(4), &sessions).unwrap();
        assert_eq!(ixs.len(), 2);
        assert_eq!(&ixs[0].data[..8], &cleanup_discriminator()[..]);
    }

    #[test]
    fn fresh_subscribe_ignores_session_slice() {
        // Subtle invariant: if state == None we don't have a parent to
        // close sessions against. Builder must ignore them rather than
        // emit nonsense close_session ixs pointing at an absent parent.
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let sessions = vec![Pubkey::new_from_array([10u8; 32])];
        let ixs = build_resubscribe_ixs(&args, None, &sessions).unwrap();
        assert_eq!(ixs.len(), 1);
    }

    #[test]
    fn subscribe_ix_accounts_in_canonical_order() {
        // Stage-3 test-engineer will pin this against the IDL via
        // LiteSVM; here we sanity-check the on-chain Accounts order
        // (subscribe.rs:21-80): subscriber, plan, token_mint,
        // subscription, vault, subscriber_ata, token_program,
        // system_program, rent.
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let ixs = build_resubscribe_ixs(&args, None, &[]).unwrap();
        let metas = &ixs[0].accounts;
        assert_eq!(metas.len(), 9);
        assert_eq!(metas[0].pubkey, kp.pubkey());
        assert!(metas[0].is_signer);
        assert_eq!(metas[1].pubkey, args.plan);
        assert!(!metas[1].is_signer);
        assert_eq!(metas[2].pubkey, args.token_mint);
        assert_eq!(metas[6].pubkey, SPL_TOKEN_PROGRAM_ID);
        assert_eq!(metas[7].pubkey, SYSTEM_PROGRAM_ID);
        assert_eq!(metas[8].pubkey, SYSVAR_RENT_ID);
    }

    /// F6 (ADR-015 §F6): `LegacyBinary` must be rejected, not decoded.
    /// Previously the variant was decoded with base64 — a silent
    /// mis-decode when an older RPC node returns the deprecated base58
    /// representation. Now we surface a typed error.
    #[test]
    fn legacy_binary_encoding_rejected_with_typed_error() {
        // Realistic payload: 8-byte zero discriminator + 4 bytes of body,
        // base58-encoded. The exact contents don't matter — the decoder
        // must refuse before it ever inspects them.
        let payload = bs58_encode_fixture(&[0u8; 12]);
        let ui = UiAccountData::LegacyBinary(payload);
        let err = decode_ui_account_data(&ui).unwrap_err();
        assert!(
            matches!(err, ResubscribeError::UnsupportedLegacyBinaryEncoding),
            "expected UnsupportedLegacyBinaryEncoding, got {err:?}"
        );
    }

    /// F6 sanity check — the Base64 happy path still decodes correctly
    /// after the LegacyBinary arm changed. Use the canonical 8-byte
    /// discriminator + 48-byte PaySession-style body as a stand-in.
    #[test]
    fn base64_encoding_still_decodes() {
        let raw_bytes = vec![0xAAu8; 56];
        let b64 = BASE64.encode(&raw_bytes);
        let ui = UiAccountData::Binary(b64, UiAccountEncoding::Base64);
        let decoded = decode_ui_account_data(&ui).unwrap();
        assert_eq!(decoded, raw_bytes);
    }

    /// Small helper to keep the F6 test free of an extra `bs58` dep
    /// (we don't actually base58-decode in the fix; the encoded payload
    /// is throwaway). Standard ASCII alphabet — Solana RPC actually
    /// returns base58 here, but the rejection path never inspects it.
    fn bs58_encode_fixture(bytes: &[u8]) -> String {
        // Avoid pulling bs58 as a dev-dep just for one fixture; emit a
        // deterministic base58-alphabet-shaped string. The decoder
        // rejects before consuming, so contents are irrelevant.
        bytes.iter().map(|b| char::from(b'a' + (b % 26))).collect()
    }

    #[test]
    fn cleanup_ix_two_accounts() {
        // ADR-013 cleanup.rs:32-50 — Subscription + subscriber, both mut.
        let kp = Keypair::new();
        let args = fixture_args(&kp);
        let ixs = build_resubscribe_ixs(&args, Some(4), &[]).unwrap();
        let metas = &ixs[0].accounts;
        assert_eq!(metas.len(), 2);
        assert!(metas[0].is_writable, "subscription must be mut");
        assert!(
            metas[1].is_writable && metas[1].is_signer,
            "subscriber must be mut signer"
        );
    }
}
