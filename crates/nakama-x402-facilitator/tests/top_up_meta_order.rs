//! Regression test: HTTP `top_up` account-meta order matches the on-chain
//! `TopUp` Accounts struct.
//!
//! Rationale (BLK-007-CRIT-1, review 2026-05-05): a previous off-by-meta bug
//! shipped a top_up handler with `subscriber_ata` and `vault` swapped into
//! slots 2-3 and a spurious `system_program` appended. Every devnet `top_up`
//! failed at submit with Anchor `ConstraintSeeds (2006)`. The TS SDK path
//! kept working only because it goes through Anchor's IDL-driven account
//! resolver, masking the off-chain Rust drift.
//!
//! This test pins the canonical order against:
//!   1. A hardcoded expected name list (always asserted, even in clean
//!      checkouts where the IDL is absent — `nakama/target/idl/` is
//!      gitignored).
//!   2. The IDL JSON at `nakama/target/idl/nakama.json`, when present
//!      (cross-checks expected names against actually-built IDL so a future
//!      on-chain reorder of the `TopUp` Accounts struct fails this test).
//!
//! The test does NOT spin up an HTTP server, RPC client, or facilitator
//! binary — it calls `build_top_up_metas` directly (BLK-007-S2 resolution
//! mandates a unit-style regression test).

use std::path::PathBuf;

use nakama_client::SubscriptionStateByte;
use nakama_x402_facilitator::handlers::top_up::build_top_up_metas;
use solana_pubkey::Pubkey;

/// Canonical IDL order for `TopUp.accounts`. Source of truth:
/// `nakama/programs/nakama/src/instructions/top_up.rs` (`#[derive(Accounts)] struct TopUp`)
/// rendered into `nakama/target/idl/nakama.json::instructions[top_up].accounts`.
const EXPECTED_ACCOUNT_NAMES: &[&str] = &[
    "subscriber",
    "subscription",
    "graced_subscription",
    "vault",
    "subscriber_ata",
    "token_program",
];

/// Distinct deterministic pubkeys so a swap of any two slots is observable
/// by direct `==` comparison rather than relying on writable/signer flag
/// drift alone.
fn fixture() -> Fixture {
    Fixture {
        subscriber: Pubkey::new_from_array([0xA1; 32]),
        subscription: Pubkey::new_from_array([0xA2; 32]),
        program_id: Pubkey::new_from_array([0xA3; 32]),
        grace_pda: Pubkey::new_from_array([0xA4; 32]),
        vault_pda: Pubkey::new_from_array([0xA5; 32]),
        subscriber_ata: Pubkey::new_from_array([0xA6; 32]),
        token_program: Pubkey::new_from_array([0xA7; 32]),
    }
}

struct Fixture {
    subscriber: Pubkey,
    subscription: Pubkey,
    program_id: Pubkey,
    grace_pda: Pubkey,
    vault_pda: Pubkey,
    subscriber_ata: Pubkey,
    token_program: Pubkey,
}

#[test]
fn meta_order_matches_canonical_for_grace_period_branch() {
    let f = fixture();
    let metas = build_top_up_metas(
        &f.subscriber,
        &f.subscription,
        &f.program_id,
        &f.grace_pda,
        &f.vault_pda,
        &f.subscriber_ata,
        &f.token_program,
        SubscriptionStateByte::GracePeriod,
    );

    assert_eq!(
        metas.len(),
        EXPECTED_ACCOUNT_NAMES.len(),
        "meta count drift"
    );

    // Slot 0: subscriber (mut, signer).
    assert_eq!(metas[0].pubkey, f.subscriber);
    assert!(metas[0].is_signer, "subscriber must be signer");
    assert!(metas[0].is_writable, "subscriber must be mut");

    // Slot 1: subscription (mut, non-signer).
    assert_eq!(metas[1].pubkey, f.subscription);
    assert!(!metas[1].is_signer);
    assert!(metas[1].is_writable);

    // Slot 2 (GracePeriod branch): real grace PDA, mut, non-signer.
    assert_eq!(
        metas[2].pubkey, f.grace_pda,
        "slot 2 must be the grace PDA when state == GracePeriod"
    );
    assert!(!metas[2].is_signer);
    assert!(
        metas[2].is_writable,
        "grace satellite is `close = subscriber` so the slot must be writable"
    );

    // Slot 3: vault (mut). NOT subscriber_ata — the swap was the original
    // CRIT-1 bug. Re-asserted explicitly.
    assert_eq!(
        metas[3].pubkey, f.vault_pda,
        "slot 3 must be vault — if this fails check BLK-007-CRIT-1"
    );
    assert!(metas[3].is_writable);

    // Slot 4: subscriber_ata (mut).
    assert_eq!(
        metas[4].pubkey, f.subscriber_ata,
        "slot 4 must be subscriber_ata — if this fails check BLK-007-CRIT-1"
    );
    assert!(metas[4].is_writable);

    // Slot 5: token_program (readonly).
    assert_eq!(metas[5].pubkey, f.token_program);
    assert!(!metas[5].is_writable);
    assert!(!metas[5].is_signer);
}

#[test]
fn meta_order_matches_canonical_for_active_branch() {
    let f = fixture();
    let metas = build_top_up_metas(
        &f.subscriber,
        &f.subscription,
        &f.program_id,
        &f.grace_pda,
        &f.vault_pda,
        &f.subscriber_ata,
        &f.token_program,
        SubscriptionStateByte::Active,
    );

    assert_eq!(metas.len(), EXPECTED_ACCOUNT_NAMES.len());

    // Slot 2 (non-Grace branch): program_id placeholder, readonly, non-signer.
    // This is the `allow-missing-optionals` sentinel pattern — Anchor decodes
    // the optional account as `None` when the supplied pubkey equals the
    // program ID.
    assert_eq!(
        metas[2].pubkey, f.program_id,
        "slot 2 must be program_id placeholder when state != GracePeriod"
    );
    assert!(
        !metas[2].is_writable,
        "placeholder optional must be readonly"
    );

    // Other slots must be identical to the GracePeriod branch.
    assert_eq!(metas[0].pubkey, f.subscriber);
    assert_eq!(metas[1].pubkey, f.subscription);
    assert_eq!(metas[3].pubkey, f.vault_pda);
    assert_eq!(metas[4].pubkey, f.subscriber_ata);
    assert_eq!(metas[5].pubkey, f.token_program);
}

#[test]
fn no_system_program_meta_present() {
    // Explicit sentinel for BLK-007-CRIT-1: the previous handler appended the
    // System program (`11111111111111111111111111111111`) as a 7th meta. The
    // on-chain `TopUp` Accounts struct has no `init` constraint and therefore
    // no `pub system_program: Program<'info, System>` field. If a future
    // refactor regresses this, the assertion below catches it before tx
    // submit.
    let f = fixture();
    let metas = build_top_up_metas(
        &f.subscriber,
        &f.subscription,
        &f.program_id,
        &f.grace_pda,
        &f.vault_pda,
        &f.subscriber_ata,
        &f.token_program,
        SubscriptionStateByte::Active,
    );

    let system_program = Pubkey::new_from_array([0u8; 32]); // "11111…111"
    for (i, m) in metas.iter().enumerate() {
        assert_ne!(
            m.pubkey, system_program,
            "slot {i} unexpectedly contains the System program — BLK-007-CRIT-1 regression"
        );
    }
    assert_eq!(
        metas.len(),
        6,
        "TopUp must have exactly 6 metas (no system_program)"
    );
}

/// When the IDL has been built (`anchor build` populates `nakama/target/idl/`),
/// cross-check the canonical name list against the IDL. Soft-skip when absent
/// so this test does not gate clean checkouts / CI without anchor toolchain.
#[test]
fn idl_account_names_match_expected_when_available() {
    let idl_path = workspace_root().join("nakama/target/idl/nakama.json");
    if !idl_path.exists() {
        eprintln!(
            "SKIP: IDL not built at {} — run `anchor build` to enable this cross-check",
            idl_path.display()
        );
        return;
    }

    let raw = std::fs::read_to_string(&idl_path).expect("read IDL");
    let idl: serde_json::Value = serde_json::from_str(&raw).expect("parse IDL JSON");

    let instructions = idl
        .get("instructions")
        .and_then(|v| v.as_array())
        .expect("idl.instructions is an array");

    let top_up = instructions
        .iter()
        .find(|ix| ix.get("name").and_then(|n| n.as_str()) == Some("top_up"))
        .expect("idl has a `top_up` instruction");

    let accounts = top_up
        .get("accounts")
        .and_then(|v| v.as_array())
        .expect("top_up.accounts is an array");

    let actual_names: Vec<&str> = accounts
        .iter()
        .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
        .collect();

    assert_eq!(
        actual_names.as_slice(),
        EXPECTED_ACCOUNT_NAMES,
        "on-chain TopUp Accounts struct order changed — update \
         build_top_up_metas + EXPECTED_ACCOUNT_NAMES in lockstep \
         (BLK-007-CRIT-1 is the precedent for why this matters)"
    );
}

/// Walk up from `CARGO_MANIFEST_DIR` until we find the workspace root
/// (the directory containing `nakama/`). The facilitator crate lives at
/// `crates/nakama-x402-facilitator/`, so the workspace root is two levels up.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
        .to_path_buf()
}
