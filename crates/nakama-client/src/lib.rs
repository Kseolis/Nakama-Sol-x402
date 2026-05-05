//! Off-chain typed bindings for the Nakama on-chain program.
//!
//! Phase 1 (this stage): hand-rolled Borsh views mirroring `state.rs`. We
//! intentionally do NOT pull in `anchor-client` or `declare_program!` here —
//! the goal is a tiny, dependency-light decoder that the keeper, indexer, and
//! x402 facilitator can all share. Phase 2 (post-IDL-stable) may swap to
//! `declare_program!` codegen; the public API of this crate is the boundary.
//!
//! References:
//! - ADR-001 §Subscription account (revised 2026-04-27 BLK-01/03/05)
//! - ADR-003 §State enum
//! - ADR-007 §"Storage decision" (GracedSubscription layout)
//! - ADR-007 §"Off-chain ComputedStatus derive" (boundary contract)
//! - `nakama/programs/nakama/src/state.rs` (canonical on-chain layout)

pub mod accounts;
pub mod computed_status;
pub mod constants;
pub mod pda;

pub use accounts::{
    AccountDecodeError, GracedSubscriptionView, PausedSubscriptionView, PaySessionView,
    SubscriptionStateByte, SubscriptionView,
};
pub use computed_status::{derive_status, ComputedStatus, ACTIVE_LOW_FUNDS_DAYS};
pub use constants::{
    ACCOUNT_DISCRIMINATOR_LEN, GRACE_DURATION, GRACE_SEED, PAY_SESSION_SEED, SUB_SEED, VAULT_SEED,
};
pub use pda::{
    derive_grace_pda, derive_pay_session_pda, derive_subscription_pda, derive_vault_pda,
};
