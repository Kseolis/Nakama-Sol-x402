//! On-chain constants per ADR-001 (account model) revised 2026-04-27.
//!
//! Seed bytes are deliberately NOT marked `#[constant]` — they are used only
//! in on-chain seeds derivation. Off-chain clients (keeper, SDK) hold their
//! own copies (see `crates/nakama-client`, `clients/ts/`).

use anchor_lang::prelude::*;

/// PDA seed for `Plan` accounts.
/// Seeds: `[PLAN_SEED, merchant.key().as_ref(), &plan_id.to_le_bytes()]`.
/// See ADR-001 §Plan account.
pub const PLAN_SEED: &[u8] = b"plan";

/// PDA seed for `Subscription` accounts.
/// Seeds: `[SUB_SEED, subscriber.key().as_ref(), plan.key().as_ref()]`.
/// See ADR-001 §Subscription account.
pub const SUB_SEED: &[u8] = b"sub";

/// PDA seed for the per-subscription token vault (non-ATA, owner = Subscription PDA).
/// Seeds: `[VAULT_SEED, subscription.key().as_ref()]`.
/// See ADR-002 §Account model and authority.
pub const VAULT_SEED: &[u8] = b"vault";

// Forward-compat seed namespaces reserved for x402 layer (day 8 GO).
// Documented here so anchor-engineer / sdk-engineer share a single source of truth.
// pub const PAY_SESSION_SEED: &[u8] = b"pay_session";
// pub const MERCHANT_SEED:    &[u8] = b"merchant";

/// USDC mint — cluster-conditional per ADR-001 §USDC mint constant (BLK-11).
///
/// `feature = "mainnet"` → real mainnet USDC mint.
/// otherwise (devnet / LiteSVM tests) → Coinbase devnet USDC.
///
/// `Plan.token_mint` is the on-chain source of truth per Subscription;
/// `USDC_MINT` is consulted only by `create_plan` for defence-in-depth
/// whitelist (see ADR-014 §Decision).
#[cfg(feature = "mainnet")]
pub const USDC_MINT: Pubkey = anchor_lang::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

#[cfg(not(feature = "mainnet"))]
pub const USDC_MINT: Pubkey = anchor_lang::pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
