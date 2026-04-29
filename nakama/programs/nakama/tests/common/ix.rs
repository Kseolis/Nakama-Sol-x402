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
    plan_pda, program_id, subscription_pda, token_program_id, usdc_mint, vault_pda,
};

// IDL-pinned discriminators (8 bytes each).
const DISC_CREATE_PLAN: [u8; 8] = [77, 43, 141, 254, 212, 118, 41, 186];
const DISC_SUBSCRIBE: [u8; 8] = [254, 28, 191, 138, 156, 179, 183, 53];
const DISC_CANCEL: [u8; 8] = [232, 219, 223, 41, 219, 236, 220, 190];

// System program id (literal-encoded so we don't pull in solana-sdk-ids).
fn system_program_id() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

fn rent_sysvar_id() -> Pubkey {
    "SysvarRent111111111111111111111111111111111".parse().unwrap()
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
            AccountMeta::new(*merchant, true),                  // merchant signer
            AccountMeta::new(plan, false),                      // plan PDA (init)
            AccountMeta::new_readonly(usdc_mint(), false),      // token_mint
            AccountMeta::new_readonly(*merchant_ata, false),    // merchant_ata
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
    let subscription = subscription_override.unwrap_or_else(|| subscription_pda(subscriber, plan).0);
    let vault = vault_override.unwrap_or_else(|| vault_pda(&subscription).0);

    let mut data = DISC_SUBSCRIBE.to_vec();
    data.extend(borsh::to_vec(&SubscribeArgs { periods_to_prefund }).expect("borsh"));

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscriber, true),                // subscriber signer
            AccountMeta::new_readonly(*plan, false),            // plan
            AccountMeta::new_readonly(*token_mint, false),      // token_mint
            AccountMeta::new(subscription, false),              // subscription PDA (init)
            AccountMeta::new(vault, false),                     // vault PDA (init)
            AccountMeta::new(*subscriber_ata, false),           // subscriber_ata (mut)
            AccountMeta::new_readonly(token_program_id(), false),
            AccountMeta::new_readonly(system_program_id(), false),
            AccountMeta::new_readonly(rent_sysvar_id(), false),
        ],
        data,
    }
}

// -- cancel ----------------------------------------------------------------

pub fn cancel_ix(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
) -> Instruction {
    cancel_ix_with_overrides(
        subscriber,
        subscription,
        None,
        merchant_ata,
        subscriber_ata,
    )
}

pub fn cancel_ix_with_overrides(
    subscriber: &Pubkey,
    subscription: &Pubkey,
    vault_override: Option<Pubkey>,
    merchant_ata: &Pubkey,
    subscriber_ata: &Pubkey,
) -> Instruction {
    let vault = vault_override.unwrap_or_else(|| vault_pda(subscription).0);

    let data = DISC_CANCEL.to_vec();

    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*subscriber, true),                // subscriber signer
            AccountMeta::new(*subscription, false),             // subscription PDA (closed at end)
            AccountMeta::new(vault, false),                     // vault PDA (closed)
            AccountMeta::new(*merchant_ata, false),             // merchant_ata (settle dest)
            AccountMeta::new(*subscriber_ata, false),           // subscriber_ata (refund dest)
            AccountMeta::new_readonly(token_program_id(), false),
        ],
        data,
    }
}
