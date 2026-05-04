//! `NakamaError` decoding helpers (sign-off handoff item 4).
//!
//! Every error-path test routes through `assert_nakama_err` — never bare
//! `result.is_err()`, per the test-engineer rules.

use litesvm::types::{FailedTransactionMetadata, TransactionResult};
// SDK v3: `TransactionError` lives at `solana_transaction_error::TransactionError`,
// re-exported under `solana_transaction::TransactionError` (top-level) — we
// avoid hard-coding the path because it has shifted between solana-transaction
// patch releases. Instead we match on the Debug-string representation, which
// is stable for the variants we care about.

/// Anchor convention (anchor_lang::error::ERROR_CODE_OFFSET = 6000).
pub const ERROR_CODE_OFFSET: u32 = 6000;

/// Mirror of `programs/nakama/src/error.rs` — by design we duplicate the
/// variant *names and discriminant codes* here from black-box reading of
/// `error.rs`. The mirror is allowed because the agent rules permit reading
/// `error.rs` to discover variant ordering.
///
/// Discriminants are derived as `ERROR_CODE_OFFSET + index`, where `index`
/// matches the source-code declaration order in `nakama::NakamaError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NakamaError {
    ZeroPeriod = 0,
    ZeroPrice = 1,
    ZeroPeriodsToFund = 2,
    ZeroRatePerSecond = 3,
    InsufficientUnlockedFunds = 4,
    ClockBackwards = 5,
    MathOverflow = 6,
    IllegalStateForCancel = 7,
    IllegalStateForCharge = 8,
    UnauthorizedCancel = 9,
    DuplicateAtaAndVault = 10,
    // ADR-004 §8 — `charge` custom errors. Indices anticipate anchor-engineer
    // appending in declaration order to `programs/nakama/src/error.rs`. If
    // anchor-engineer chooses a different order, update these indices to match
    // — the order of variants in the source file is the wire contract.
    AtaMismatch = 11,
    MintMismatch = 12,
    VaultOwnerMismatch = 13,
}

impl NakamaError {
    pub fn code(self) -> u32 {
        ERROR_CODE_OFFSET + (self as u32)
    }
}

/// Anchor framework error codes that the program can surface without
/// triggering a `NakamaError` variant. Values from `anchor_lang::error::ErrorCode`
/// (docs.rs/anchor-lang/1.0.1).
pub mod anchor_codes {
    /// Returned by Anchor when an `Account<'info, T>` deserialization fails
    /// because the account is uninitialised (closed or never inited).
    pub const ACCOUNT_NOT_INITIALIZED: u32 = 3012;
    /// Returned when the constraint `token::mint` fails.
    pub const CONSTRAINT_TOKEN_MINT: u32 = 2014;
    /// Returned when the constraint `token::authority` fails.
    pub const CONSTRAINT_TOKEN_OWNER: u32 = 2015;
    /// Returned when a seed-derived account's address doesn't match.
    pub const CONSTRAINT_SEEDS: u32 = 2006;
    /// Returned when `has_one = ...` fails.
    pub const CONSTRAINT_HAS_ONE: u32 = 2001;
    /// Anchor: `address = ...` constraint failed.
    pub const CONSTRAINT_ADDRESS: u32 = 2012;
    /// Anchor framework: account owner program differs from the expected one
    /// (e.g. Token-2022 owned mint passed where classic Token program owner
    /// is required by `Account<'info, Mint>`).
    pub const ACCOUNT_OWNED_BY_WRONG_PROGRAM: u32 = 3007;
    /// Anchor framework: `Program<'info, T>` saw a pubkey that didn't match
    /// `T`'s embedded program id (e.g. Token-2022 program id passed where
    /// `Program<'info, Token>` expects classic SPL Token).
    pub const INVALID_PROGRAM_ID: u32 = 3008;
}

/// Pretty-print transaction failure metadata so a missed assertion shows full
/// log context, not just the unwrapped message.
fn dump_meta(meta: &FailedTransactionMetadata) -> String {
    let logs: String = meta
        .meta
        .logs
        .iter()
        .map(|l| format!("    {}", l))
        .collect::<Vec<_>>()
        .join("\n");
    format!("error: {:?}\nlogs:\n{}", meta.err, logs)
}

/// Extract the `Custom(code)` value from a failed tx, if any.
///
/// Implementation note: we parse the Debug-printed form of the error rather
/// than match on a typed enum. `InstructionError::Custom(N)` and the
/// `TransactionError::InstructionError(_, Custom(N))` Debug impls are stable
/// across solana-transaction 3.0.x; a substring match for `"Custom("` plus
/// a digit run is sufficient and immune to crate-path drift between patch
/// releases.
pub fn extract_custom_code(meta: &FailedTransactionMetadata) -> Option<u32> {
    let s = format!("{:?}", meta.err);
    let pat = "Custom(";
    let start = s.find(pat)? + pat.len();
    let rest = &s[start..];
    let end = rest.find(')')?;
    rest[..end].trim().parse::<u32>().ok()
}

/// Assert the tx failed with `expected` Nakama error variant.
#[track_caller]
pub fn assert_nakama_err<T>(result: TransactionResult, expected: NakamaError) {
    let meta = match result {
        Ok(_) => panic!("expected NakamaError::{:?} but tx succeeded", expected),
        Err(meta) => meta,
    };

    let actual_code = extract_custom_code(&meta).unwrap_or_else(|| {
        panic!(
            "expected NakamaError::{:?} (code {}), got non-Custom error.\n{}",
            expected,
            expected.code(),
            dump_meta(&meta)
        )
    });

    if actual_code != expected.code() {
        // Helpfully decode if it's another Nakama variant.
        let translated = (ERROR_CODE_OFFSET..ERROR_CODE_OFFSET + 32)
            .find(|c| *c == actual_code)
            .map(|c| c - ERROR_CODE_OFFSET);
        panic!(
            "expected NakamaError::{:?} (code {}), got code {} (nakama variant idx = {:?}).\n{}",
            expected,
            expected.code(),
            actual_code,
            translated,
            dump_meta(&meta)
        );
    }

    // Suppress unused warning when T is non-Drop.
    let _ = std::marker::PhantomData::<T>;
}

/// Assert the tx failed with one of the Anchor-internal codes (e.g. seed /
/// constraint / not-initialised).
#[track_caller]
pub fn assert_anchor_err(result: TransactionResult, expected_code: u32) {
    let meta = match result {
        Ok(_) => panic!("expected anchor code {} but tx succeeded", expected_code),
        Err(meta) => meta,
    };
    let actual_code = extract_custom_code(&meta).unwrap_or_else(|| {
        panic!(
            "expected anchor code {}, got non-Custom error.\n{}",
            expected_code,
            dump_meta(&meta)
        )
    });
    if actual_code != expected_code {
        panic!(
            "expected anchor code {}, got code {}.\n{}",
            expected_code,
            actual_code,
            dump_meta(&meta)
        );
    }
}

/// Assert the tx failed; print full metadata for caller's eyeballs but don't
/// pin the exact code. Use sparingly — only when a single test exercises a
/// path that can fail two equally-correct ways (e.g. close-then-replay where
/// either AccountNotInitialized or seeds mismatch is acceptable).
#[track_caller]
pub fn assert_any_err(result: TransactionResult) -> FailedTransactionMetadata {
    match result {
        Ok(_) => panic!("expected tx failure but it succeeded"),
        Err(meta) => meta,
    }
}
