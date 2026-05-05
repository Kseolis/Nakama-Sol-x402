//! Shared LiteSVM fixtures for Nakama integration tests.
//!
//! Black-box methodology: nothing here imports symbols from `nakama::state` or
//! `nakama::instructions`. All program-side details (discriminators, argument
//! layouts, account ordering) are derived from `target/idl/nakama.json` per
//! the test-engineer agent rules.
//!
//! Sources for fixtures:
//! - ADR-001 §USDC mint constant (devnet mint hardcoded in IDL `token_mint`)
//! - ADR-001 §Plan / Subscription seeds
//! - ADR-002 §Account model and authority
//! - ADR-014 §Accounts struct (create_plan)
//! - sign-off BLK-16 (LiteSVM clock helper)
//! - sign-off BLK-19 (state offset 192)

#![allow(dead_code)] // helpers are reused selectively across test files

pub mod clock;
pub mod error;
pub mod ix;

use litesvm::LiteSVM;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_program::program_pack::Pack;
use solana_pubkey::Pubkey;
// Re-export so test files can `use common::Signer;` without depending on the
// granular crate path themselves.
pub use solana_signer::Signer;
use solana_transaction::Transaction;

// -- IDs / constants -------------------------------------------------------

/// Program id (CLAUDE.md hardcoded value, also pinned in IDL).
pub const PROGRAM_ID_STR: &str = "HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm";

/// Devnet USDC mint address used by the program (ADR-001 §USDC mint constant,
/// BLK-11). Same value pinned in `Anchor.toml`-derived IDL.
pub const USDC_MINT_STR: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

/// Anchor classic-SPL token program id.
pub const TOKEN_PROGRAM_ID_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// State byte offset inside `Subscription.data` after the 8-byte Anchor
/// discriminator. Computed from ADR-001 revised layout (sign-off BLK-19):
/// disc(8) + next_charge_at(8) + subscriber(32) + plan(32) + price(8) +
/// period(8) + token_mint(32) + merchant(32) + merchant_ata(32) = 192.
pub const STATE_OFFSET: usize = 192;

/// Seed prefixes (ADR-001 §Constants). Repeated here to avoid coupling to impl
/// `constants.rs`.
pub const PLAN_SEED: &[u8] = b"plan";
pub const SUB_SEED: &[u8] = b"sub";
pub const VAULT_SEED: &[u8] = b"vault";
/// ADR-007 §"Storage decision" — `[GRACE_SEED, subscription.key().as_ref()]`.
pub const GRACE_SEED: &[u8] = b"grace";

/// ADR-007 §I-CONST-1 — `GRACE_DURATION = 7 * 24 * 60 * 60` seconds.
pub const GRACE_DURATION: i64 = 7 * 24 * 60 * 60;

pub fn program_id() -> Pubkey {
    PROGRAM_ID_STR.parse().expect("hardcoded valid base58")
}

pub fn usdc_mint() -> Pubkey {
    USDC_MINT_STR.parse().expect("hardcoded valid base58")
}

pub fn token_program_id() -> Pubkey {
    TOKEN_PROGRAM_ID_STR
        .parse()
        .expect("hardcoded valid base58")
}

// -- LiteSVM bring-up ------------------------------------------------------

/// Resolve the on-disk path to the freshly built program shared object.
fn so_path() -> std::path::PathBuf {
    // tests/ → programs/nakama/ → nakama/programs/nakama/Cargo.toml
    // CARGO_MANIFEST_DIR resolves to programs/nakama/.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Anchor places the `.so` under `<workspace>/target/deploy/<crate>.so`.
    // workspace = nakama/, crate = nakama → ../../target/deploy/nakama.so
    manifest_dir.join("../../target/deploy/nakama.so")
}

/// Spin up a LiteSVM instance with the Nakama program loaded and a fresh
/// USDC test mint at the canonical address (mint authority = the returned
/// keypair).
///
/// Returning the mint authority lets test code mint additional supply later.
pub struct TestEnv {
    pub svm: LiteSVM,
    pub mint_authority: Keypair,
}

pub fn setup() -> TestEnv {
    let mut svm = LiteSVM::new();

    let path = so_path();
    assert!(
        path.exists(),
        "program SO missing at {:?}; run `anchor build` first",
        path
    );
    svm.add_program_from_file(program_id(), &path)
        .expect("load program");

    let mint_authority = Keypair::new();
    svm.airdrop(&mint_authority.pubkey(), 100_000_000_000)
        .expect("airdrop mint authority");

    install_mint(&mut svm, &usdc_mint(), &mint_authority.pubkey(), 6);

    TestEnv {
        svm,
        mint_authority,
    }
}

/// Write a packed SPL Mint directly into `set_account`, since LiteSVM lacks a
/// faucet-style helper. Bypasses `initialize_mint` — equivalent post-state.
pub fn install_mint(svm: &mut LiteSVM, mint: &Pubkey, authority: &Pubkey, decimals: u8) {
    use spl_token::state::Mint;

    // SAFETY: programs/spl_token::state::Mint is `Pack` with LEN = 82.
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    let m = Mint {
        mint_authority: spl_token::solana_program::program_option::COption::Some(*authority),
        supply: 0,
        decimals,
        is_initialized: true,
        freeze_authority: spl_token::solana_program::program_option::COption::None,
    };
    Mint::pack(m, &mut data).expect("pack mint");

    let acct = Account {
        lamports: 1_000_000_000, // rent-exempt-ish
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(*mint, acct).expect("set mint account");
}

// -- ATA helpers -----------------------------------------------------------

/// Deterministic ATA address (does not require running the program).
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address(owner, mint)
}

/// Create an SPL TokenAccount at a specific (non-ATA) address by pre-packing
/// account state. Useful for adversarial tests where we plant an account with
/// a wrong owner / mint.
pub fn install_token_account(
    svm: &mut LiteSVM,
    address: &Pubkey,
    mint: &Pubkey,
    owner: &Pubkey,
    amount: u64,
) {
    use spl_token::state::Account as TokenAccount;
    use spl_token::state::AccountState;

    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let a = TokenAccount {
        mint: *mint,
        owner: *owner,
        amount,
        delegate: spl_token::solana_program::program_option::COption::None,
        state: AccountState::Initialized,
        is_native: spl_token::solana_program::program_option::COption::None,
        delegated_amount: 0,
        close_authority: spl_token::solana_program::program_option::COption::None,
    };
    TokenAccount::pack(a, &mut data).expect("pack token account");

    let acct = Account {
        lamports: 2_039_280, // typical TokenAccount rent-exempt minimum
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(*address, acct).expect("set token account");
}

/// Create the ATA at its canonical address with `amount` tokens minted in.
/// Mirrors the SDK helper `getOrCreateAssociatedTokenAccount` semantics.
pub fn install_funded_ata(svm: &mut LiteSVM, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
    let ata_addr = ata(owner, mint);
    install_token_account(svm, &ata_addr, mint, owner, amount);
    ata_addr
}

// -- PDA derivations -------------------------------------------------------

pub fn plan_pda(merchant: &Pubkey, plan_id: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[PLAN_SEED, merchant.as_ref(), &plan_id.to_le_bytes()],
        &program_id(),
    )
}

pub fn subscription_pda(subscriber: &Pubkey, plan: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[SUB_SEED, subscriber.as_ref(), plan.as_ref()],
        &program_id(),
    )
}

pub fn vault_pda(subscription: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED, subscription.as_ref()], &program_id())
}

/// ADR-007 §"Storage decision" — `GracedSubscription` satellite PDA.
/// Seeds: `[GRACE_SEED, subscription.key().as_ref()]`.
pub fn grace_pda(subscription: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[GRACE_SEED, subscription.as_ref()], &program_id())
}

// -- Transaction helpers ---------------------------------------------------

/// Build, sign and submit a transaction. Returns the wrapped LiteSVM result so
/// the caller can decode error metadata via `common::error`.
///
/// `clippy::result_large_err` allowed: `litesvm::types::TransactionResult` is
/// `Result<TransactionMetadata, FailedTransactionMetadata>`, the latter a
/// large enum from upstream. Boxing here would force every test caller to
/// match-and-deref. ADR-007 cycle-4 sign-off: documented suppression.
#[allow(clippy::result_large_err)]
pub fn send_tx(
    svm: &mut LiteSVM,
    payer: &Keypair,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> litesvm::types::TransactionResult {
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(instructions, Some(&payer.pubkey()), &blockhash);
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx)
}

/// Mint `amount` tokens to `dest` (which must already be a TokenAccount).
pub fn mint_to(
    svm: &mut LiteSVM,
    mint_authority: &Keypair,
    mint: &Pubkey,
    dest: &Pubkey,
    amount: u64,
) {
    let ix = spl_token::instruction::mint_to(
        &token_program_id(),
        mint,
        dest,
        &mint_authority.pubkey(),
        &[],
        amount,
    )
    .expect("mint_to ix");
    send_tx(svm, mint_authority, &[ix], &[mint_authority]).expect("mint_to tx");
}

/// Read the raw account.data for an arbitrary address.
pub fn read_account_data(svm: &LiteSVM, address: &Pubkey) -> Vec<u8> {
    svm.get_account(address)
        .unwrap_or_else(|| panic!("account {} not found", address))
        .data
}

/// Decode an SPL TokenAccount balance.
pub fn token_balance(svm: &LiteSVM, address: &Pubkey) -> u64 {
    use spl_token::state::Account as TokenAccount;
    let data = read_account_data(svm, address);
    let acct = TokenAccount::unpack(&data).expect("unpack token account");
    acct.amount
}

/// Quick sanity-funded subscriber + merchant pair, both airdropped lamports
/// and (for subscriber) given a USDC ATA prefilled with `subscriber_usdc`.
pub struct Actors {
    pub subscriber: Keypair,
    pub merchant: Keypair,
    pub subscriber_ata: Pubkey,
    pub merchant_ata: Pubkey,
}

pub fn fund_actors(env: &mut TestEnv, subscriber_usdc: u64) -> Actors {
    let subscriber = Keypair::new();
    let merchant = Keypair::new();
    env.svm
        .airdrop(&subscriber.pubkey(), 5_000_000_000)
        .expect("airdrop subscriber");
    env.svm
        .airdrop(&merchant.pubkey(), 5_000_000_000)
        .expect("airdrop merchant");

    let subscriber_ata = install_funded_ata(
        &mut env.svm,
        &subscriber.pubkey(),
        &usdc_mint(),
        subscriber_usdc,
    );
    let merchant_ata = install_funded_ata(&mut env.svm, &merchant.pubkey(), &usdc_mint(), 0);

    Actors {
        subscriber,
        merchant,
        subscriber_ata,
        merchant_ata,
    }
}
