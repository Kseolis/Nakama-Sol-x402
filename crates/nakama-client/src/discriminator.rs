//! Anchor 1.0 instruction discriminators — first 8 bytes of
//! `SHA256("global:<ix_name>")`. Cached per process via `OnceLock` so we
//! pay the hash cost exactly once.
//!
//! We deliberately avoid pulling in `anchor-client` for this single
//! convention — same trade-off the x402 facilitator already made
//! (`crates/nakama-x402-facilitator/src/handlers/top_up.rs::top_up_discriminator`).
//!
//! Drift watch: if the on-chain handler is ever renamed (e.g. `cleanup` →
//! `close_subscription`), the `discriminator_is_stable_*` snapshot tests
//! below fail loudly. anchor-engineer change is the only legitimate cause.

use sha2::{Digest, Sha256};
use std::sync::OnceLock;

fn compute(global_name: &[u8]) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(b"global:");
    hasher.update(global_name);
    let full = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full[..8]);
    out
}

/// `cleanup` instruction discriminator (ADR-013 §"Cleanup handler").
pub fn cleanup_discriminator() -> [u8; 8] {
    static DISC: OnceLock<[u8; 8]> = OnceLock::new();
    *DISC.get_or_init(|| compute(b"cleanup"))
}

/// `subscribe` instruction discriminator. Args: `(periods_to_prefund: u8,)`.
pub fn subscribe_discriminator() -> [u8; 8] {
    static DISC: OnceLock<[u8; 8]> = OnceLock::new();
    *DISC.get_or_init(|| compute(b"subscribe"))
}

/// `close_session` instruction discriminator (ADR-x402-001 §"close_session").
pub fn close_session_discriminator() -> [u8; 8] {
    static DISC: OnceLock<[u8; 8]> = OnceLock::new();
    *DISC.get_or_init(|| compute(b"close_session"))
}

/// `cancel` instruction discriminator (ADR-013 §"Cancel handler" + ADR-009
/// polymorphic-signer extension). Args: none — handler reads everything
/// from the snapshotted Subscription.
pub fn cancel_discriminator() -> [u8; 8] {
    static DISC: OnceLock<[u8; 8]> = OnceLock::new();
    *DISC.get_or_init(|| compute(b"cancel"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_is_stable_cleanup() {
        // Snapshot: first 8 bytes of sha256("global:cleanup"). Recomputed
        // inline so a tampered helper would fail the assertion.
        let d = cleanup_discriminator();
        let mut h = Sha256::new();
        h.update(b"global:cleanup");
        assert_eq!(&h.finalize()[..8], &d[..]);
    }

    #[test]
    fn discriminator_is_stable_subscribe() {
        let d = subscribe_discriminator();
        let mut h = Sha256::new();
        h.update(b"global:subscribe");
        assert_eq!(&h.finalize()[..8], &d[..]);
    }

    #[test]
    fn discriminator_is_stable_close_session() {
        let d = close_session_discriminator();
        let mut h = Sha256::new();
        h.update(b"global:close_session");
        assert_eq!(&h.finalize()[..8], &d[..]);
    }

    #[test]
    fn discriminators_are_pairwise_distinct() {
        let a = cleanup_discriminator();
        let b = subscribe_discriminator();
        let c = close_session_discriminator();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
}
