//! Off-chain mirrors of on-chain constants.
//!
//! Source of truth lives in `nakama/programs/nakama/src/constants.rs`. We do
//! NOT import the on-chain crate (it builds for the sBPF target with Anchor's
//! `idl-build` feature graph; pulling it into off-chain code drags Solana
//! program-runtime deps unnecessarily). Drift between the two files is
//! caught by integration tests that re-derive PDAs and compare against the
//! on-chain handler's `seeds = [...]` constraint.

/// Anchor account discriminator length — first 8 bytes of every `#[account]`-
/// decorated struct on the wire. See Anchor 1.0 docs §"Account discriminator".
pub const ACCOUNT_DISCRIMINATOR_LEN: usize = 8;

/// Subscription PDA seed. Mirrors `nakama::constants::SUB_SEED`.
pub const SUB_SEED: &[u8] = b"sub";

/// Vault PDA seed. Mirrors `nakama::constants::VAULT_SEED`.
pub const VAULT_SEED: &[u8] = b"vault";

/// GracedSubscription satellite PDA seed (ADR-007 §"Storage decision").
/// Mirrors `nakama::constants::GRACE_SEED`.
pub const GRACE_SEED: &[u8] = b"grace";

/// Grace period duration, seconds (ADR-007 Decision; I-CONST-1).
pub const GRACE_DURATION: i64 = 7 * 24 * 60 * 60;
