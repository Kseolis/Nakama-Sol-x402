/**
 * PDA derivation helpers for the Nakama on-chain program.
 *
 * Seed bytes mirror `programs/nakama/src/constants.rs` byte-for-byte:
 *   - PLAN_SEED   = b"plan"
 *   - SUB_SEED    = b"sub"
 *   - VAULT_SEED  = b"vault"
 *   - GRACE_SEED  = b"grace"   (ADR-007 §"Storage decision")
 *
 * Drift between these constants and the on-chain side will desync every
 * client tx. Verified via const-asserts in the Rust side; mirror manually
 * here (codegen via Codama is post-MVP).
 *
 * Sources:
 *  - ADR-001 §Plan account / §Subscription account (PLAN/SUB seeds)
 *  - ADR-002 §Account model and authority (VAULT seed)
 *  - ADR-007 §"Storage decision" (GRACE seed + GRACE_DURATION)
 */

import { PublicKey } from "@solana/web3.js";
import BN from "bn.js";

export const PLAN_SEED = Buffer.from("plan");
export const SUB_SEED = Buffer.from("sub");
export const VAULT_SEED = Buffer.from("vault");
export const GRACE_SEED = Buffer.from("grace");
/** ADR-x402-001 §"PaySession PDA Layout" — `b"pay_session"`. */
export const PAY_SESSION_SEED = Buffer.from("pay_session");

/** ADR-007: 7 days, hardcoded — no per-Plan override (rejected alt (f)). */
export const GRACE_DURATION_SECONDS = 7 * 24 * 60 * 60;

/**
 * Derive the canonical Plan PDA.
 *
 * Seeds: `[PLAN_SEED, merchant.key().as_ref(), &plan_id.to_le_bytes()]`.
 *
 * @example
 * ```ts
 * const [planPda] = derivePlanPda(programId, merchant.publicKey, new BN(1));
 * ```
 */
export function derivePlanPda(
  programId: PublicKey,
  merchant: PublicKey,
  planId: BN,
): [PublicKey, number] {
  // u64 little-endian, 8 bytes — must match Rust `plan_id.to_le_bytes()`.
  const planIdLe = planId.toArrayLike(Buffer, "le", 8);
  return PublicKey.findProgramAddressSync(
    [PLAN_SEED, merchant.toBuffer(), planIdLe],
    programId,
  );
}

/**
 * Derive the canonical Subscription PDA.
 *
 * Seeds: `[SUB_SEED, subscriber.key().as_ref(), plan.key().as_ref()]`.
 *
 * @example
 * ```ts
 * const [subPda] = deriveSubscriptionPda(programId, subscriber, planPda);
 * ```
 */
export function deriveSubscriptionPda(
  programId: PublicKey,
  subscriber: PublicKey,
  plan: PublicKey,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [SUB_SEED, subscriber.toBuffer(), plan.toBuffer()],
    programId,
  );
}

/**
 * Derive the per-subscription vault PDA (token account, owner = Subscription PDA).
 *
 * Seeds: `[VAULT_SEED, subscription.key().as_ref()]`.
 *
 * @example
 * ```ts
 * const [vaultPda] = deriveVaultPda(programId, subPda);
 * ```
 */
export function deriveVaultPda(
  programId: PublicKey,
  subscription: PublicKey,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [VAULT_SEED, subscription.toBuffer()],
    programId,
  );
}

/**
 * Derive the GracedSubscription satellite PDA (ADR-007).
 *
 * Seeds: `[GRACE_SEED, subscription.key().as_ref()]`.
 *
 * Lifecycle:
 *  - init at `charge` tail when `withdrawn == deposited` (payer = keeper).
 *  - close at `top_up` from GracePeriod or `cancel` from GracePeriod
 *    (rent → subscriber).
 *
 * @example
 * ```ts
 * const [gracedPda] = deriveGracedSubscriptionPda(programId, subPda);
 * ```
 */
export function deriveGracedSubscriptionPda(
  programId: PublicKey,
  subscription: PublicKey,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [GRACE_SEED, subscription.toBuffer()],
    programId,
  );
}

/**
 * Derive the PaySession satellite PDA (ADR-x402-001).
 *
 * Seeds: `[PAY_SESSION_SEED, subscription.key().as_ref(), &session_id.to_le_bytes()]`.
 *
 * Q1 (ADR-x402-001): N concurrent sessions per Subscription via u64 nonce.
 * Subscriber is responsible for choosing a non-colliding session_id;
 * Anchor `init` on duplicate seeds returns `AccountAlreadyInUse`.
 *
 * @example
 * ```ts
 * const [paySessionPda] = derivePaySessionPda(programId, subPda, new BN(42));
 * ```
 */
export function derivePaySessionPda(
  programId: PublicKey,
  subscription: PublicKey,
  sessionId: BN,
): [PublicKey, number] {
  // u64 little-endian, 8 bytes — must match Rust `session_id.to_le_bytes()`.
  const sessionIdLe = sessionId.toArrayLike(Buffer, "le", 8);
  return PublicKey.findProgramAddressSync(
    [PAY_SESSION_SEED, subscription.toBuffer(), sessionIdLe],
    programId,
  );
}
