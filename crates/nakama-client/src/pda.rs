//! PDA derivation helpers — off-chain mirrors of on-chain `seeds = [...]`
//! constraints. Pure functions; no RPC. Drift between these and the on-chain
//! `seeds` clauses surfaces at integration-test time as `ConstraintSeeds`
//! (Anchor 2006).

use solana_pubkey::Pubkey;

use crate::constants::{GRACE_SEED, PAY_SESSION_SEED, SUB_SEED, VAULT_SEED};

/// Derive the Subscription PDA — `[SUB_SEED, subscriber, plan]`.
/// See ADR-001 §Subscription account.
pub fn derive_subscription_pda(
    program_id: &Pubkey,
    subscriber: &Pubkey,
    plan: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[SUB_SEED, subscriber.as_ref(), plan.as_ref()], program_id)
}

/// Derive the vault PDA — `[VAULT_SEED, subscription]`. See ADR-002 §Account model.
pub fn derive_vault_pda(program_id: &Pubkey, subscription: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED, subscription.as_ref()], program_id)
}

/// Derive the GracedSubscription PDA — `[GRACE_SEED, subscription]`.
/// ADR-007 §"Storage decision"; I-GRACE-1.
pub fn derive_grace_pda(program_id: &Pubkey, subscription: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[GRACE_SEED, subscription.as_ref()], program_id)
}

/// Derive the PaySession satellite PDA —
/// `[PAY_SESSION_SEED, subscription, session_id_le]`. ADR-x402-001
/// §"PaySession PDA Layout" (Q2 — u64 nonce client-gen).
pub fn derive_pay_session_pda(
    program_id: &Pubkey,
    subscription: &Pubkey,
    session_id: u64,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            PAY_SESSION_SEED,
            subscription.as_ref(),
            &session_id.to_le_bytes(),
        ],
        program_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pda_derivation_is_deterministic() {
        let program = Pubkey::new_from_array([1u8; 32]);
        let sub = Pubkey::new_from_array([2u8; 32]);
        let (a, _) = derive_grace_pda(&program, &sub);
        let (b, _) = derive_grace_pda(&program, &sub);
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_seeds_yield_distinct_pdas() {
        let program = Pubkey::new_from_array([1u8; 32]);
        let sub = Pubkey::new_from_array([2u8; 32]);
        let (vault, _) = derive_vault_pda(&program, &sub);
        let (grace, _) = derive_grace_pda(&program, &sub);
        assert_ne!(
            vault, grace,
            "vault and grace seed namespaces must not collide"
        );
    }
}
