//! Hand-rolled Anchor-instruction builders.
//!
//! We do not depend on `anchor-client` so the test harness stays close to the
//! wire format described in `target/idl/nakama.json`. Discriminators are
//! copied verbatim from the IDL; argument layouts use Borsh.
//!
//! Discriminator sources:
//! - `create_plan`: IDL `instructions[1].discriminator`
//! - `subscribe`:   IDL `instructions[2].discriminator`
//! - `cancel`:      IDL `instructions[0].discriminator`

use borsh::BorshSerialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{
    grace_pda, plan_pda, program_id, subscription_pda, token_program_id, usdc_mint, vault_pda,
};

// IDL-pinned discriminators (8 bytes each).
const DISC_CREATE_PLAN: [u8; 8] = [77, 43, 141, 254, 212, 118, 41, 186];
const DISC_SUBSCRIBE: [u8; 8] = [254, 28, 191, 138, 156, 179, 183, 53];
const DISC_CANCEL: [u8; 8] = [232, 219, 223, 41, 219, 236, 220, 190];
// `charge` discriminator: cross-checked against `target/idl/nakama.json`
// (instruction "charge".discriminator) on 2026-05-04 after the handler landed.
const DISC_CHARGE: [u8; 8] = [26, 55, 197, 209, 93, 77, 242, 15];
// `cleanup` discriminator (ADR-013 cycle-3): cross-checked against
// `target/idl/nakama.json` (instruction "cleanup".discriminator) on
// 2026-05-04 after the handler landed.
const DISC_CLEANUP: [u8; 8] = [36, 158, 31, 187, 253, 37, 68, 210];
// `top_up` discriminator (ADR-007 cycle-4): cross-checked against
// `target/idl/nakama.json` (instruction "top_up".discriminator) on
// 2026-05-05 after the handler landed.
const DISC_TOP_UP: [u8; 8] = [236, 225, 96, 9, 60, 106, 77, 208];

// System program id (literal-encoded so we don't pull in solana-sdk-ids).
fn system_program_id() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

fn rent_sysvar_id() -> Pubkey {
    "SysvarRent111111111111111111111111111111111"
        .parse()
        .unwrap()
}

// -- create_plan -----------------------------------------------------------

#[derive(BorshSerialize)]
struct CreatePlanArgs {
    plan_id: u64,
    price: u64,
    period: i64,
}

/// Build a `create_plan` ix following the ADR-014 Accounts struct order.
pub fn create_plan_ix(
    merchant: &Pubkey,
    merchant_ata: &Pubkey,
    plan_id: u64,
    price: u64,
    period: i64,
) -> Instruction {
    let (plan, _) = plan_pda(merchant, plan_id);

    let mut data = DISC_CREATE_PLAN.to_vec();
    data.extend(
        borsh::to_vec(&CreatePlanArgs {
            plan_id,
            price,
            period,
        })
        .expect("borsh"),
    );

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*merchant, true),               // merchant signer
            AccountMeta::new(plan, false),                   // plan PDA (init)
            AccountMeta::new_readonly(usdc_mint(), false),   // token_mint
            AccountMeta::new_readonly(*merchant_ata, false), // merchant_ata
            AccountMeta::new_readonly(token_program_id(), false),
            AccountMeta::new_readonly(system_program_id(), false),
        ],
        data,
    }
}

/// Variant: same as `create_plan_ix` but with caller-supplied token_mint
/// (lets us probe the address-pinning constraint by passing a non-USDC mint).
pub fn create_plan_ix_with_mint(
    merchant: &Pubkey,
    merchant_ata: &Pubkey,
    token_mint: &Pubkey,
    plan_id: u64,
    price: u64,
    period: i64,
) -> Instruction {
    let (plan, _) = plan_pda(merchant, plan_id);

    let mut data = DISC_CREATE_PLAN.to_vec();
    data.extend(
        borsh::to_vec(&CreatePlanArgs {
            plan_id,
            price,
            period,
        })
        .expect("borsh"),
    );

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*merchant, true),
            AccountMeta::new(plan, false),
            AccountMeta::new_readonly(*token_mint, false),
            AccountMeta::new_readonly(*merchant_ata, false),
            AccountMeta::new_readonly(token_program_id(), false),
            AccountMeta::new_readonly(system_program_id(), false),
        ],
        data,
    }
}

// -- subscribe -------------------------------------------------------------

#[derive(BorshSerialize)]
struct SubscribeArgs {
    periods_to_prefund: u8,
}

pub fn subscribe_ix(
    subscriber: &Pubkey,
    plan: &Pubkey,
    subscriber_ata: &Pubkey,
    periods_to_prefund: u8,
) -> Instruction {
    subscribe_ix_with_overrides(
        subscriber,
        plan,
        subscriber_ata,
        &usdc_mint(),
        periods_to_prefund,
        None,
        None,
    )
}

/// Power version: lets adversarial tests substitute the `plan` account, the
/// vault address or the subscription PDA. Defaults preserve canonical
/// derivations.
pub fn subscribe_ix_with_overrides(
    subscriber: &Pubkey,
    plan: &Pubkey,
    subscriber_ata: &Pubkey,
    token_mint: &Pubkey,
    periods_to_prefund: u8,
    subscription_override: Option<Pubkey>,
    vault_override: Option<Pubkey>,
) -> Instruction {
    let subscription =
        subscription_override.unwrap_or_else(|| subscription_pda(subscriber, plan).0);
    let vault = vault_override.unwrap_or_else(|| vault_pda(&subscription).0);

    let mut data = DISC_SUBSCRIBE.to_vec();
    data.extend(borsh::to_vec(&SubscribeArgs { periods_to_prefund }).expect("borsh"));

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscriber, true),     // subscriber signer
            AccountMeta::new_readonly(*plan, false), // plan
            AccountMeta::new_readonly(*token_mint, false), // token_mint
            AccountMeta::new(subscription, false),   // subscription PDA (init)
            AccountMeta::new(vault, false),          // vault PDA (init)
            AccountMeta::new(*subscriber_ata, false), // subscriber_ata (mut)
            AccountMeta::new_readonly(token_program_id(), false),
            AccountMeta::new_readonly(system_program_id(), false),
            AccountMeta::new_readonly(rent_sysvar_id(), false),
        ],
        data,
    }
}

// -- charge ----------------------------------------------------------------

/// Build a `charge` ix following ADR-004 §9 Accounts struct order.
///
/// Order: subscription, plan, vault, merchant_ata, token_program, payer.
/// `payer` is permissionless per ADR-004 §1 (no `Signer<'info>` constraint
/// on subscriber/merchant). Caller passes whoever signs the tx.
pub fn charge_ix(
    subscription: &Pubkey,
    plan: &Pubkey,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    payer: &Pubkey,
) -> Instruction {
    charge_ix_with_overrides(
        subscription,
        plan,
        vault,
        merchant_ata,
        payer,
        &token_program_id(),
    )
}

/// Power version: lets adversarial tests substitute the token program id
/// (Token-2022 reject, ADR-004 §6).
///
/// `graced_subscription` defaults to the `program_id` placeholder, which
/// Anchor 1.0.1 (with the `allow-missing-optionals` feature, see
/// `programs/nakama/Cargo.toml`) interprets as `Option::None`. This is the
/// correct shape for charges that do NOT exhaust the stream — the post-CPI
/// math leaves `withdrawn_amount < deposited_amount` and the §I-CHARGE-1
/// tail does not fire. ADR-007 §"Source-of-truth verification" Q9.
pub fn charge_ix_with_overrides(
    subscription: &Pubkey,
    plan: &Pubkey,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    payer: &Pubkey,
    token_prog: &Pubkey,
) -> Instruction {
    charge_ix_full(
        subscription,
        plan,
        vault,
        merchant_ata,
        payer,
        token_prog,
        /* graced_subscription = */ None,
    )
}

/// Full version: explicit `graced_subscription` for ADR-007 charge-tail tests.
///
/// `graced_subscription = Some(pda)` makes Anchor treat the optional slot as
/// `Option::Some` and run the `init` constraint when the post-CPI math flips
/// the stream into GracePeriod (§I-CHARGE-1). `None` plants the `program_id`
/// placeholder, signalling absence.
///
/// Order MUST stay aligned with IDL `instructions[].accounts` (verified
/// 2026-05-05 against `target/idl/nakama.json`):
///   subscription, plan, vault, merchant_ata, token_program, payer,
///   graced_subscription, system_program.
pub fn charge_ix_full(
    subscription: &Pubkey,
    plan: &Pubkey,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    payer: &Pubkey,
    token_prog: &Pubkey,
    graced_subscription: Option<Pubkey>,
) -> Instruction {
    let data = DISC_CHARGE.to_vec();

    // Optional-account placeholder convention: `program_id` of the executing
    // program signals `Option::None` to Anchor codegen. Marking it
    // `new_readonly` (and matching `writable=false`) is benign because Anchor
    // skips constraint evaluation entirely for the None case.
    let graced_meta = match graced_subscription {
        Some(pda) => AccountMeta::new(pda, false),
        None => AccountMeta::new_readonly(program_id(), false),
    };

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscription, false), // subscription PDA (mut)
            AccountMeta::new_readonly(*plan, false), // plan (read-only, has_one target)
            AccountMeta::new(*vault, false),        // vault (mut, source of CPI)
            AccountMeta::new(*merchant_ata, false), // merchant_ata (mut, dest)
            AccountMeta::new_readonly(*token_prog, false), // token_program
            AccountMeta::new(*payer, true),         // payer signer (any pubkey)
            graced_meta,                            // graced_subscription (Option)
            AccountMeta::new_readonly(system_program_id(), false), // system_program
        ],
        data,
    }
}

// -- cancel ----------------------------------------------------------------

/// Subscriber-signer cancel — pre-ADR-009 default. Signer == subscriber.
pub fn cancel_ix(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
) -> Instruction {
    cancel_ix_with_signer(
        /* signer = */ subscriber,
        /* subscriber = */ subscriber,
        subscription,
        None,
        merchant_ata,
        subscriber_ata,
        /* graced_subscription = */ None,
    )
}

/// Merchant-signer cancel — ADR-009. Signer == merchant; subscriber slot still
/// receives rent.
pub fn cancel_ix_by_merchant(
    merchant: &Pubkey,
    subscriber: &Pubkey,
    subscription: &Pubkey,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
) -> Instruction {
    cancel_ix_with_signer(
        merchant,
        subscriber,
        subscription,
        None,
        merchant_ata,
        subscriber_ata,
        None,
    )
}

/// Adversarial / parametric variant — explicit signer + subscriber slot,
/// optional vault override and graced satellite. ADR-009 wire format.
#[allow(clippy::too_many_arguments)]
pub fn cancel_ix_with_signer(
    signer: &Pubkey,
    subscriber: &Pubkey,
    subscription: &Pubkey,
    vault_override: Option<Pubkey>,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
    graced_subscription: Option<Pubkey>,
) -> Instruction {
    cancel_ix_full(
        signer,
        subscriber,
        subscription,
        vault_override,
        merchant_ata,
        subscriber_ata,
        graced_subscription,
    )
}

// -- cleanup ---------------------------------------------------------------

/// Build a `cleanup` ix following ADR-013 §"Cleanup handler" Accounts struct
/// order: subscription (mut, closed) + subscriber (signer, mut). No args.
pub fn cleanup_ix(subscriber: &Pubkey, subscription: &Pubkey) -> Instruction {
    cleanup_ix_with_signer(subscriber, subscription, subscriber)
}

/// Adversarial variant: lets us pass a different signer than the snapshotted
/// `subscription.subscriber` so we can prove the `has_one = subscriber` /
/// `UnauthorizedCleanup` guard fires (ADR-013 §Q1).
///
/// `signer_pk` goes into the AccountMeta with `is_signer = true`; the program
/// will compare it against `subscription.subscriber` and raise
/// `NakamaError::UnauthorizedCleanup` (or Anchor `ConstraintHasOne` if the
/// declarative path fires first; the test accepts either).
// -- top_up (ADR-007) ------------------------------------------------------

#[derive(BorshSerialize)]
struct TopUpArgs {
    amount: u64,
}

/// Build a `top_up` ix following ADR-007 §"top_up handler" Accounts struct.
///
/// IDL order (verified 2026-05-05 against `target/idl/nakama.json`):
///   subscriber (signer), subscription, graced_subscription (optional, mid),
///   vault, subscriber_ata, token_program.
///
/// Default behavior: signals `Option::None` for `graced_subscription` by
/// planting `program_id` as the placeholder pubkey (Anchor 1.0.1 +
/// `allow-missing-optionals` convention — the satellite is NOT trailing in
/// the `top_up` Accounts struct, so the account meta cannot be omitted; the
/// program_id placeholder is the documented signal for absent optional).
/// Suitable for top_up from Active/Paused where the satellite does not
/// exist on chain.
///
/// For top_up from GracePeriod, the caller MUST use `top_up_ix_with_grace`
/// to pass the inited satellite PDA explicitly.
pub fn top_up_ix(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    subscriber_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    let (vault, _) = vault_pda(subscription);
    top_up_ix_full(
        subscriber,
        subscription,
        /* graced_subscription = */ None,
        &vault,
        subscriber_ata,
        &token_program_id(),
        amount,
    )
}

/// Variant for top_up from GracePeriod — passes the satellite PDA so Anchor
/// runs the `close = subscriber` constraint (rent → subscriber, ADR-007
/// §I-GRACE-3). Use when the on-chain `Subscription.state == GracePeriod`.
pub fn top_up_ix_with_grace(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    subscriber_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    let (vault, _) = vault_pda(subscription);
    let (graced, _) = grace_pda(subscription);
    top_up_ix_full(
        subscriber,
        subscription,
        Some(graced),
        &vault,
        subscriber_ata,
        &token_program_id(),
        amount,
    )
}

/// Full version: explicit `graced_subscription` + `vault` + `token_program`
/// for adversarial tests (wrong PDA, wrong token program, missing satellite).
///
/// `graced_subscription = Some(pda)` writes the explicit address;
/// `None` plants `program_id` placeholder so Anchor reads it as
/// `Option::None`. ADR-007 §"top_up handler".
pub fn top_up_ix_full(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    graced_subscription: Option<Pubkey>,
    vault: &Pubkey,
    subscriber_ata: &Pubkey,
    token_prog: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = DISC_TOP_UP.to_vec();
    data.extend(borsh::to_vec(&TopUpArgs { amount }).expect("borsh top_up args"));

    let graced_meta = match graced_subscription {
        Some(pda) => AccountMeta::new(pda, false),
        None => AccountMeta::new_readonly(program_id(), false),
    };

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscriber, true),    // subscriber signer (mut)
            AccountMeta::new(*subscription, false), // subscription PDA (mut)
            graced_meta,                            // graced_subscription (Option)
            AccountMeta::new(*vault, false),        // vault (mut, CPI dest)
            AccountMeta::new(*subscriber_ata, false), // subscriber_ata (mut, CPI src)
            AccountMeta::new_readonly(*token_prog, false), // token_program
        ],
        data,
    }
}

pub fn cleanup_ix_with_signer(
    _subscriber_snapshot: &Pubkey,
    subscription: &Pubkey,
    signer_pk: &Pubkey,
) -> Instruction {
    let data = DISC_CLEANUP.to_vec();
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscription, false), // subscription PDA (mut, closed by Anchor)
            AccountMeta::new(*signer_pk, true),     // subscriber signer (mut for rent return)
        ],
        data,
    }
}

/// Adversarial entry-point preserved for ADR-007 callsites that pre-date
/// ADR-009; signer always == subscriber.
pub fn cancel_ix_with_overrides(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    vault_override: Option<Pubkey>,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
) -> Instruction {
    cancel_ix_full(
        subscriber,
        subscriber,
        subscription,
        vault_override,
        merchant_ata,
        subscriber_ata,
        /* graced_subscription = */ None,
    )
}

/// Full version: explicit `signer` + rent-recipient `subscriber` slot +
/// `graced_subscription` for ADR-007 cancel-from-Grace and ADR-009
/// merchant-cancel tests.
///
/// IDL order (ADR-009 canonical, post-merchant-cancel split):
///   signer, subscription, subscriber, vault, merchant_ata, subscriber_ata,
///   token_program, graced_subscription (optional, trailing).
///
/// Note: subscription precedes subscriber so the latter's
/// `address = subscription.subscriber` constraint resolves against an
/// already-loaded account; forward-references surface as Anchor 3007.
///
/// `signer` writes is_signer=true; `subscriber` is is_signer=false (rent
/// recipient validated by handler against `subscription.subscriber`).
/// `graced_subscription`: `Some(pda)` makes Anchor run `close = subscriber`;
/// `None` plants `program_id` placeholder for `allow-missing-optionals`.
#[allow(clippy::too_many_arguments)]
pub fn cancel_ix_full(
    signer: &Pubkey,
    subscriber: &Pubkey,
    subscription: &Pubkey,
    vault_override: Option<Pubkey>,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
    graced_subscription: Option<Pubkey>,
) -> Instruction {
    let vault = vault_override.unwrap_or_else(|| vault_pda(subscription).0);

    let data = DISC_CANCEL.to_vec();

    let graced_meta = match graced_subscription {
        Some(pda) => AccountMeta::new(pda, false),
        None => AccountMeta::new_readonly(program_id(), false),
    };

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*signer, true),          // signer (Signer)
            AccountMeta::new(*subscription, false),   // subscription PDA (preserved)
            AccountMeta::new(*subscriber, false), // subscriber (UncheckedAccount, rent recipient)
            AccountMeta::new(vault, false),       // vault PDA (closed via SPL CPI)
            AccountMeta::new(*merchant_ata, false), // merchant_ata (settle dest)
            AccountMeta::new(*subscriber_ata, false), // subscriber_ata (refund dest)
            AccountMeta::new_readonly(token_program_id(), false),
            graced_meta, // graced_subscription (Option, trailing)
        ],
        data,
    }
}

// -- ADR-x402-001 lifecycle ix --------------------------------------------

const DISC_OPEN_SESSION: [u8; 8] = [130, 54, 124, 7, 236, 20, 104, 104];
const DISC_CLOSE_SESSION: [u8; 8] = [68, 114, 178, 140, 222, 38, 248, 211];

#[derive(BorshSerialize)]
struct OpenSessionArgs {
    session_id: u64,
    facilitator: Pubkey,
    reservation_cap: u64,
}

/// Build an `open_session(session_id, facilitator, reservation_cap)` ix.
///
/// Wire order (canonical, matches ADR-x402-001 §"open_session" Accounts struct):
///   parent (Subscription PDA), pay_session (init), subscriber (Signer mut),
///   system_program.
pub fn open_session_ix(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    session_id: u64,
    facilitator: &Pubkey,
    reservation_cap: u64,
) -> Instruction {
    let mut data = DISC_OPEN_SESSION.to_vec();
    data.extend(
        borsh::to_vec(&OpenSessionArgs {
            session_id,
            facilitator: *facilitator,
            reservation_cap,
        })
        .expect("borsh open_session args"),
    );

    let (pay_session, _) = super::pay_session_pda(subscription, session_id);

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscription, false), // parent (mut for has_one read; not modified)
            AccountMeta::new(pay_session, false),   // pay_session (init mut)
            AccountMeta::new(*subscriber, true),    // subscriber (Signer, mut payer)
            AccountMeta::new_readonly(system_program_id(), false),
        ],
        data,
    }
}

/// Build a `close_session()` ix.
///
/// Wire order (canonical, matches ADR-x402-001 §"close_session" Accounts struct):
///   parent (Subscription PDA), pay_session (mut, closed), subscriber
///   (Signer, mut rent recipient).
///
/// Note: NO `parent.state == Active` guard on close_session per ADR-x402-001
/// R1 closure — close must work from any parent state including Cancelled.
pub fn close_session_ix(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    session_id: u64,
) -> Instruction {
    let data = DISC_CLOSE_SESSION.to_vec();
    let (pay_session, _) = super::pay_session_pda(subscription, session_id);

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*subscription, false),
            AccountMeta::new(pay_session, false), // mut, will be Anchor-closed
            AccountMeta::new(*subscriber, true),  // Signer + rent recipient
        ],
        data,
    }
}

const DISC_SETTLE_USAGE: [u8; 8] = [61, 174, 167, 9, 21, 219, 242, 117];

#[derive(BorshSerialize)]
struct SettleUsageArgs {
    amount: u64,
}

/// Build a `settle_usage(amount)` ix.
///
/// Wire order (canonical, matches ADR-x402-001 §"settle_usage" Accounts struct):
///   parent (Subscription mut), pay_session (mut), vault (mut TokenAccount),
///   merchant_ata (mut TokenAccount), facilitator (Signer), token_program.
pub fn settle_usage_ix(
    facilitator: &Pubkey,
    subscription: &Pubkey,
    session_id: u64,
    vault: &Pubkey,
    merchant_ata: &Pubkey,
    token_prog: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = DISC_SETTLE_USAGE.to_vec();
    data.extend(borsh::to_vec(&SettleUsageArgs { amount }).expect("borsh settle_usage args"));

    let (pay_session, _) = super::pay_session_pda(subscription, session_id);

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscription, false), // parent (mut for withdrawn_amount)
            AccountMeta::new(pay_session, false),   // pay_session (mut — usage_amount, state)
            AccountMeta::new(*vault, false),        // vault (mut, source of CPI)
            AccountMeta::new(*merchant_ata, false), // merchant_ata (mut, dest of CPI)
            AccountMeta::new(*facilitator, true),   // facilitator (Signer)
            AccountMeta::new_readonly(*token_prog, false),
        ],
        data,
    }
}
