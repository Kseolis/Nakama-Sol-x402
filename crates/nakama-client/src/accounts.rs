//! Borsh-deserializable views of on-chain accounts.
//!
//! These structs mirror byte-for-byte the layout of the corresponding
//! `#[account]` types in `nakama/programs/nakama/src/state.rs`. Field order
//! is part of the on-chain ABI (ADR-001 §Field-order rationale, BLK-18/19);
//! never reorder. Reserved padding stays explicit so that any layout drift
//! shows up as a Borsh size mismatch at decode time, not a silent corruption.
//!
//! The on-chain account layout is preceded by an 8-byte Anchor discriminator
//! (see `ACCOUNT_DISCRIMINATOR_LEN`); call sites strip it before invoking
//! `try_from_slice` on the views.

use borsh::BorshDeserialize;
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_pubkey::Pubkey;
use thiserror::Error;

use crate::constants::ACCOUNT_DISCRIMINATOR_LEN;

/// Errors surfacing during raw account-data decode. Distinguishes the failure
/// modes a caller may want to handle differently:
/// * the account didn't exist on RPC,
/// * the on-chain owner field didn't match the expected program (spoofed
///   System-owned account with a plausible-looking discriminator prefix —
///   ADR-015 §F5, security-audit-patterns.md P3),
/// * the discriminator was the wrong account type (forward-compat dispatch /
///   P3 defence-in-depth — `strip_discriminator` historically only checked
///   length, not value),
/// * the body was truncated / malformed.
#[derive(Debug, Error)]
pub enum AccountDecodeError {
    #[error("account data shorter than discriminator length")]
    TooShort,
    /// Owner field on the fetched account is not the expected Nakama program.
    /// An attacker can create a System-owned PDA-shaped account whose first
    /// 8 bytes match an Anchor discriminator; without this guard the decoder
    /// would happily Borsh-decode arbitrary bytes as `SubscriptionView`.
    /// Surface to HTTP as 404, not 500 — the requested object doesn't exist
    /// as far as the program is concerned.
    #[error("account owner mismatch: expected {expected}, got {actual}")]
    WrongOwner { expected: Pubkey, actual: Pubkey },
    /// Discriminator bytes [0..8] do not match the expected Anchor account
    /// discriminator for the requested type. Surfaces forward-compat drift
    /// (caller asked for `Subscription`, on-chain bytes are a `PaySession`)
    /// and rejects same-program junk accounts that survive the owner check
    /// but encode a different account variant.
    #[error("wrong account discriminator: expected {expected:?}, got {actual:?}")]
    WrongDiscriminator { expected: [u8; 8], actual: [u8; 8] },
    #[error("borsh decode failed: {0}")]
    Borsh(#[from] std::io::Error),
}

/// Compute the Anchor account discriminator for a named on-chain account type
/// — first 8 bytes of `SHA256("account:<Name>")`. Mirrors Anchor 1.0
/// `#[account]` codegen.
///
/// We compute on demand rather than caching: the function is only called once
/// per account fetch, and Sha256 of a 24-byte input is negligible relative to
/// the surrounding RPC roundtrip.
pub(crate) fn compute_account_discriminator(name: &str) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(b"account:");
    hasher.update(name.as_bytes());
    let full = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full[..8]);
    out
}

/// Strict decode helper: validate `account.owner == expected_owner` AND that
/// the leading discriminator bytes match `expected_discriminator` BEFORE
/// invoking Borsh. This is the single chokepoint for off-chain reads of
/// program-owned data — ADR-015 §F5, `security-audit-patterns.md` P3.
///
/// GRASP role: Information Expert — owns the contract "what does it mean to
/// trust an RPC-returned account". The keeper, facilitator, indexer, and SDK
/// composite-tx builders all route program-owned reads through this function
/// instead of inlining the owner check.
///
/// Returns:
/// * `WrongOwner` — account exists but is owned by another program (spoofed
///   junk OR caller passed a wrong PDA).
/// * `WrongDiscriminator` — account is program-owned but its 8-byte type tag
///   mismatches the requested type. Forward-compat dispatch falls through
///   this arm too; callers that want to peek at other variants must decode
///   discriminator explicitly.
/// * `TooShort` / `Borsh` — body invalid.
pub fn decode_program_owned<T: BorshDeserialize>(
    account: &Account,
    expected_owner: &Pubkey,
    expected_discriminator: [u8; 8],
) -> Result<T, AccountDecodeError> {
    if account.owner != *expected_owner {
        return Err(AccountDecodeError::WrongOwner {
            expected: *expected_owner,
            actual: account.owner,
        });
    }
    let data = account.data.as_slice();
    if data.len() < ACCOUNT_DISCRIMINATOR_LEN {
        return Err(AccountDecodeError::TooShort);
    }
    let actual: [u8; 8] = data[..ACCOUNT_DISCRIMINATOR_LEN]
        .try_into()
        .expect("slice length checked");
    if actual != expected_discriminator {
        return Err(AccountDecodeError::WrongDiscriminator {
            expected: expected_discriminator,
            actual,
        });
    }
    let body = &data[ACCOUNT_DISCRIMINATOR_LEN..];
    Ok(T::try_from_slice(body)?)
}

/// Strip the Anchor discriminator. Returns the body slice or `TooShort`.
///
/// We deliberately do NOT verify the discriminator value here — that is a
/// per-account-type concern. Callers that need forward-compat dispatch do
/// `&data[..8]` matching against the expected SHA256("account:Subscription")
/// prefix (post-MVP; ADR-001 reserves the namespace).
pub fn strip_discriminator(data: &[u8]) -> Result<&[u8], AccountDecodeError> {
    if data.len() < ACCOUNT_DISCRIMINATOR_LEN {
        return Err(AccountDecodeError::TooShort);
    }
    Ok(&data[ACCOUNT_DISCRIMINATOR_LEN..])
}

/// Off-chain mirror of `SubscriptionState` (`state.rs:50-69`). We hold the
/// raw byte rather than an enum so unknown discriminants from a future
/// redeploy do NOT panic during Borsh decode (the on-chain enum is
/// `#[non_exhaustive]` plus has only 0..=4 today; a 6th variant lands in a
/// later cycle, and old clients reading new accounts must degrade gracefully
/// per `state.rs` doc-comment "MVP mitigation"). We map known bytes via
/// `as_known()`; the `Unknown` arm is the fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionStateByte {
    Active,
    Paused,
    GracePeriod,
    Exhausted,
    Cancelled,
    Unknown(u8),
}

impl From<u8> for SubscriptionStateByte {
    fn from(b: u8) -> Self {
        match b {
            0 => Self::Active,
            1 => Self::Paused,
            2 => Self::GracePeriod,
            3 => Self::Exhausted,
            4 => Self::Cancelled,
            other => Self::Unknown(other),
        }
    }
}

/// Borsh view of `Subscription`. Mirrors `state.rs:122-163`.
///
/// On-wire size: 8 (discriminator) + 267 (Borsh body) = 275 bytes. Verified
/// at compile time on-chain via `assert!(Subscription::INIT_SPACE == 267)`.
/// Off-chain we trust the on-chain const-assert; if Borsh decode succeeds on
/// a 267-byte slice the layout matches.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct SubscriptionView {
    pub next_charge_at: i64,
    pub subscriber: Pubkey,
    pub plan: Pubkey,
    pub price: u64,
    pub period: i64,
    pub token_mint: Pubkey,
    pub merchant: Pubkey,
    pub merchant_ata: Pubkey,
    /// Raw byte — not the enum — for forward-compat with future state variants.
    pub state: u8,
    pub bump: u8,
    pub vault_bump: u8,
    pub created_at: i64,
    pub last_charge_at: i64,
    pub deposited_amount: u64,
    pub withdrawn_amount: u64,
    pub rate_per_second: u64,
    pub stream_start: i64,
    pub reserved: [u8; 32],
}

impl SubscriptionView {
    /// Anchor discriminator for the on-chain `Subscription` account =
    /// first 8 bytes of `sha256("account:Subscription")`. Stable per
    /// Anchor 1.0 codegen contract. Computed once per process via the
    /// `discriminator()` accessor.
    pub fn discriminator() -> [u8; 8] {
        compute_account_discriminator("Subscription")
    }

    /// Decode raw account data (with Anchor discriminator) — legacy
    /// "trust the bytes" path. Retained for unit tests that build buffers
    /// from scratch with a zeroed discriminator. Production callsites
    /// MUST use [`decode_owned`](Self::decode_owned) which validates
    /// `account.owner` and the discriminator value (ADR-015 §F5).
    pub fn try_decode(data: &[u8]) -> Result<Self, AccountDecodeError> {
        let body = strip_discriminator(data)?;
        Ok(Self::try_from_slice(body)?)
    }

    /// Strict decode: validates owner and discriminator before Borsh.
    /// ADR-015 §F5 chokepoint. Use this for every RPC-fetched Subscription.
    pub fn decode_owned(
        account: &Account,
        program_id: &Pubkey,
    ) -> Result<Self, AccountDecodeError> {
        decode_program_owned(account, program_id, Self::discriminator())
    }

    pub fn state_byte(&self) -> SubscriptionStateByte {
        SubscriptionStateByte::from(self.state)
    }
}

/// Borsh view of `GracedSubscription` (ADR-007 §"Storage decision"; I-GRACE-2).
///
/// Layout: `subscription: Pubkey (32) + entered_grace_at: i64 (8) + grace_until: i64 (8)` = 48 bytes Borsh.
/// On-wire: 8 (discriminator) + 48 = 56 bytes.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct GracedSubscriptionView {
    pub subscription: Pubkey,
    pub entered_grace_at: i64,
    pub grace_until: i64,
}

impl GracedSubscriptionView {
    /// Anchor discriminator for `GracedSubscription`.
    pub fn discriminator() -> [u8; 8] {
        compute_account_discriminator("GracedSubscription")
    }

    pub fn try_decode(data: &[u8]) -> Result<Self, AccountDecodeError> {
        let body = strip_discriminator(data)?;
        Ok(Self::try_from_slice(body)?)
    }

    /// Strict decode — ADR-015 §F5.
    pub fn decode_owned(
        account: &Account,
        program_id: &Pubkey,
    ) -> Result<Self, AccountDecodeError> {
        decode_program_owned(account, program_id, Self::discriminator())
    }
}

/// Borsh view of `PausedSubscription` (ADR-006 §"Storage layout").
///
/// Layout: `subscription: Pubkey (32) + paused_at: i64 (8) + bump: u8 (1)`
/// = 41 bytes Borsh. On-wire: 8 (discriminator) + 41 = 49 bytes.
///
/// Existence is the FSM signal: `subscription.state == Paused ⟺ this PDA
/// exists`. `derive_status` consumes `Option<PausedSubscriptionView>`
/// alongside the parent state byte to surface `ComputedStatus::Paused`
/// with `paused_at` for UX.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct PausedSubscriptionView {
    pub subscription: Pubkey,
    pub paused_at: i64,
    pub bump: u8,
}

impl PausedSubscriptionView {
    /// Anchor discriminator for `PausedSubscription`.
    pub fn discriminator() -> [u8; 8] {
        compute_account_discriminator("PausedSubscription")
    }

    pub fn try_decode(data: &[u8]) -> Result<Self, AccountDecodeError> {
        let body = strip_discriminator(data)?;
        Ok(Self::try_from_slice(body)?)
    }

    /// Strict decode — ADR-015 §F5.
    pub fn decode_owned(
        account: &Account,
        program_id: &Pubkey,
    ) -> Result<Self, AccountDecodeError> {
        decode_program_owned(account, program_id, Self::discriminator())
    }

    /// Backwards-compat constructor for callers that needed a placeholder
    /// pre-ADR-006-impl (tests / forward-compat plumbing). Now-real fields
    /// default to zero values.
    pub fn placeholder() -> Self {
        Self {
            subscription: Pubkey::new_from_array([0u8; 32]),
            paused_at: 0,
            bump: 0,
        }
    }
}

/// Borsh view of `PaySession` (ADR-x402-001 §"PaySession PDA Layout").
///
/// Layout (202 bytes Borsh, 210 with 8-byte discriminator):
///   subscription(32) + merchant(32) + merchant_ata(32) + facilitator(32)
///   + session_id(8) + opened_at(8) + last_settle_at(8) + usage_amount(8)
///   + reservation_cap(8) + state(1) + bump(1) + reserved(32)
///
/// `state` is held as the raw byte (matching `PaySessionState`
/// discriminants 0=Open / 1=Settling / 2=Closed). We do NOT decode into the
/// enum because `Settling` should never persist post-tx — observed-on-disk
/// Settling indicates a stuck state needing R3 force_close recovery, and
/// we'd rather surface that to callers than silently coerce.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct PaySessionView {
    pub subscription: Pubkey,
    pub merchant: Pubkey,
    pub merchant_ata: Pubkey,
    pub facilitator: Pubkey,
    pub session_id: u64,
    pub opened_at: i64,
    pub last_settle_at: i64,
    pub usage_amount: u64,
    pub reservation_cap: u64,
    pub state: u8,
    pub bump: u8,
    pub reserved: [u8; 32],
}

impl PaySessionView {
    /// Anchor discriminator for `PaySession`.
    pub fn discriminator() -> [u8; 8] {
        compute_account_discriminator("PaySession")
    }

    pub fn try_decode(data: &[u8]) -> Result<Self, AccountDecodeError> {
        let body = strip_discriminator(data)?;
        Ok(Self::try_from_slice(body)?)
    }

    /// Strict decode — ADR-015 §F5.
    pub fn decode_owned(
        account: &Account,
        program_id: &Pubkey,
    ) -> Result<Self, AccountDecodeError> {
        decode_program_owned(account, program_id, Self::discriminator())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borsh::BorshSerialize;

    /// Helper: prepend an arbitrary 8-byte discriminator (zeroed for tests).
    fn with_disc(body: Vec<u8>) -> Vec<u8> {
        let mut out = vec![0u8; ACCOUNT_DISCRIMINATOR_LEN];
        out.extend(body);
        out
    }

    #[test]
    fn subscription_borsh_round_trip_267_bytes() {
        // Build a SubscriptionView with deterministic field values, serialize,
        // verify Borsh size, decode, compare.
        let pk = Pubkey::new_from_array([7u8; 32]);
        let original = SubscriptionView {
            next_charge_at: 1_000,
            subscriber: pk,
            plan: pk,
            price: 100_000,
            period: 86_400,
            token_mint: pk,
            merchant: pk,
            merchant_ata: pk,
            state: 0,
            bump: 254,
            vault_bump: 253,
            created_at: 100,
            last_charge_at: 200,
            deposited_amount: 5_000_000,
            withdrawn_amount: 1_000_000,
            rate_per_second: 1,
            stream_start: 100,
            reserved: [0u8; 32],
        };

        let mut body = Vec::new();
        // BorshSerialize is derived for SubscriptionView via the same field
        // order — but we only derived BorshDeserialize. Re-derive locally for
        // the test by writing the bytes in field order.
        BorshSerialize::serialize(&original.next_charge_at, &mut body).unwrap();
        BorshSerialize::serialize(&original.subscriber.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&original.plan.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&original.price, &mut body).unwrap();
        BorshSerialize::serialize(&original.period, &mut body).unwrap();
        BorshSerialize::serialize(&original.token_mint.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&original.merchant.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&original.merchant_ata.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&original.state, &mut body).unwrap();
        BorshSerialize::serialize(&original.bump, &mut body).unwrap();
        BorshSerialize::serialize(&original.vault_bump, &mut body).unwrap();
        BorshSerialize::serialize(&original.created_at, &mut body).unwrap();
        BorshSerialize::serialize(&original.last_charge_at, &mut body).unwrap();
        BorshSerialize::serialize(&original.deposited_amount, &mut body).unwrap();
        BorshSerialize::serialize(&original.withdrawn_amount, &mut body).unwrap();
        BorshSerialize::serialize(&original.rate_per_second, &mut body).unwrap();
        BorshSerialize::serialize(&original.stream_start, &mut body).unwrap();
        BorshSerialize::serialize(&original.reserved, &mut body).unwrap();

        // ADR-001 invariant — must be 267.
        assert_eq!(
            body.len(),
            267,
            "Subscription body must be 267 bytes per ADR-001"
        );

        let raw = with_disc(body);
        let decoded = SubscriptionView::try_decode(&raw).expect("decode");
        assert_eq!(decoded.next_charge_at, original.next_charge_at);
        assert_eq!(
            decoded.subscriber.to_bytes(),
            original.subscriber.to_bytes()
        );
        assert_eq!(decoded.state, original.state);
        assert_eq!(decoded.deposited_amount, original.deposited_amount);
        assert_eq!(decoded.reserved, [0u8; 32]);
    }

    #[test]
    fn graced_subscription_borsh_48_bytes() {
        let pk = Pubkey::new_from_array([3u8; 32]);
        let mut body = Vec::new();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&1_000i64, &mut body).unwrap();
        BorshSerialize::serialize(&(1_000i64 + GRACE_DURATION_FOR_TEST), &mut body).unwrap();
        assert_eq!(
            body.len(),
            48,
            "GracedSubscription body must be 48 bytes per ADR-007"
        );

        let raw = with_disc(body);
        let decoded = GracedSubscriptionView::try_decode(&raw).expect("decode");
        assert_eq!(decoded.subscription.to_bytes(), pk.to_bytes());
        assert_eq!(decoded.entered_grace_at, 1_000);
        assert_eq!(decoded.grace_until, 1_000 + GRACE_DURATION_FOR_TEST);
    }

    #[test]
    fn too_short_returns_specific_error() {
        let raw = vec![0u8; 4];
        assert!(matches!(
            SubscriptionView::try_decode(&raw),
            Err(AccountDecodeError::TooShort)
        ));
    }

    #[test]
    fn state_byte_dispatch_known_and_unknown() {
        assert_eq!(
            SubscriptionStateByte::from(0),
            SubscriptionStateByte::Active
        );
        assert_eq!(
            SubscriptionStateByte::from(2),
            SubscriptionStateByte::GracePeriod
        );
        assert_eq!(
            SubscriptionStateByte::from(4),
            SubscriptionStateByte::Cancelled
        );
        assert_eq!(
            SubscriptionStateByte::from(99),
            SubscriptionStateByte::Unknown(99)
        );
    }

    const GRACE_DURATION_FOR_TEST: i64 = 7 * 24 * 60 * 60;

    // ─── ADR-015 §F5 — owner + discriminator guard tests ───────────────────

    /// Build a valid program-owned account body for a `SubscriptionView`,
    /// optionally with a corrupted discriminator. Helper for the §F5 tests.
    fn make_subscription_account(
        owner: Pubkey,
        discriminator_override: Option<[u8; 8]>,
    ) -> Account {
        let pk = Pubkey::new_from_array([7u8; 32]);
        let mut body = Vec::new();
        BorshSerialize::serialize(&0i64, &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&0u64, &mut body).unwrap();
        BorshSerialize::serialize(&0i64, &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&pk.to_bytes(), &mut body).unwrap();
        BorshSerialize::serialize(&0u8, &mut body).unwrap();
        BorshSerialize::serialize(&0u8, &mut body).unwrap();
        BorshSerialize::serialize(&0u8, &mut body).unwrap();
        BorshSerialize::serialize(&0i64, &mut body).unwrap();
        BorshSerialize::serialize(&0i64, &mut body).unwrap();
        BorshSerialize::serialize(&0u64, &mut body).unwrap();
        BorshSerialize::serialize(&0u64, &mut body).unwrap();
        BorshSerialize::serialize(&0u64, &mut body).unwrap();
        BorshSerialize::serialize(&0i64, &mut body).unwrap();
        BorshSerialize::serialize(&[0u8; 32], &mut body).unwrap();

        let disc = discriminator_override.unwrap_or_else(SubscriptionView::discriminator);
        let mut data = disc.to_vec();
        data.extend(body);

        Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[test]
    fn decode_owned_rejects_wrong_owner() {
        // Attacker creates a System-owned account that round-trips Borsh
        // perfectly and even has the correct Anchor discriminator (e.g.
        // copied from a legitimate account). Without the owner guard the
        // decoder would silently accept it.
        let program_id = Pubkey::new_from_array([0xAA; 32]);
        let foreign_owner = Pubkey::new_from_array([0xBB; 32]);
        let account = make_subscription_account(foreign_owner, None);
        let err = SubscriptionView::decode_owned(&account, &program_id).unwrap_err();
        match err {
            AccountDecodeError::WrongOwner { expected, actual } => {
                assert_eq!(expected, program_id);
                assert_eq!(actual, foreign_owner);
            }
            other => panic!("expected WrongOwner, got {other:?}"),
        }
    }

    #[test]
    fn decode_owned_rejects_wrong_discriminator() {
        // Owner correct but discriminator is e.g. PaySession's — caller asked
        // for a Subscription but the on-chain account is something else.
        let program_id = Pubkey::new_from_array([0xAA; 32]);
        let fake_disc = PaySessionView::discriminator();
        let account = make_subscription_account(program_id, Some(fake_disc));
        let err = SubscriptionView::decode_owned(&account, &program_id).unwrap_err();
        assert!(
            matches!(err, AccountDecodeError::WrongDiscriminator { .. }),
            "expected WrongDiscriminator, got {err:?}"
        );
    }

    #[test]
    fn decode_owned_accepts_correct_owner_and_discriminator() {
        let program_id = Pubkey::new_from_array([0xAA; 32]);
        let account = make_subscription_account(program_id, None);
        let view = SubscriptionView::decode_owned(&account, &program_id)
            .expect("valid program-owned account decodes");
        assert_eq!(view.state, 0);
    }

    #[test]
    fn decode_owned_rejects_too_short_data() {
        let program_id = Pubkey::new_from_array([0xAA; 32]);
        let account = Account {
            lamports: 1,
            data: vec![0u8; 4],
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        };
        let err = SubscriptionView::decode_owned(&account, &program_id).unwrap_err();
        assert!(
            matches!(err, AccountDecodeError::TooShort),
            "expected TooShort, got {err:?}"
        );
    }

    #[test]
    fn discriminators_are_pairwise_distinct() {
        let s = SubscriptionView::discriminator();
        let g = GracedSubscriptionView::discriminator();
        let p = PausedSubscriptionView::discriminator();
        let ps = PaySessionView::discriminator();
        for (a, b) in [(s, g), (s, p), (s, ps), (g, p), (g, ps), (p, ps)] {
            assert_ne!(a, b, "Anchor account discriminators must be distinct");
        }
    }
}
