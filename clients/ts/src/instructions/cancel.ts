/**
 * `cancel` instruction builder (ADR-002 + ADR-013 + ADR-009).
 *
 * Polymorphic signer — caller may be either subscription.subscriber OR
 * subscription.merchant. The polymorphism is enforced on-chain by a
 * runtime require! against the snapshotted Subscription fields; the SDK
 * leaves the choice to the caller.
 *
 * Settle math, vault close, and Subscription tombstone semantics inherit
 * unchanged from ADR-013. Rent flow is fixed: vault rent → snapshotted
 * subscriber regardless of who signs (ADR-009 §"Rent-flow invariant").
 *
 * @see ADR-009 §"Decision"
 * @see ADR-013 §"Cancel handler"
 * @see ADR-002 §cancel
 */

import { Program } from "@anchor-lang/core";
import { PublicKey, TransactionInstruction } from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";

import { Nakama, SubscriptionState } from "../types";
import { deriveGracedSubscriptionPda } from "../pdas";

export interface BuildCancelIxArgs {
  /** Anchor `Program<Nakama>` from `target/types/nakama.ts`. */
  program: Program<Nakama>;
  /**
   * Polymorphic signer pubkey — must equal `subscription.subscriber` OR
   * `subscription.merchant`. The on-chain handler validates against both.
   * Passing any other pubkey returns `NoCancelAuthority`.
   */
  signer: PublicKey;
  /**
   * Snapshotted subscriber wallet — rent recipient for vault close and (if
   * Grace) GracedSubscription close. MUST equal `subscription.subscriber`;
   * the on-chain `address = ...` constraint enforces this.
   */
  subscriber: PublicKey;
  /** Subscription PDA. */
  subscription: PublicKey;
  /** Per-subscription vault PDA. */
  vault: PublicKey;
  /** Merchant USDC ATA — settle destination. */
  merchantAta: PublicKey;
  /** Subscriber USDC ATA — refund destination. */
  subscriberAta: PublicKey;
  /**
   * Current Subscription FSM state. Drives the optional GracedSubscription
   * slot:
   *  - Active → null
   *  - GracePeriod → satellite PDA, so Anchor runs `close = subscriber`.
   * Other states are illegal for cancel and surface `IllegalStateForCancel`.
   */
  state: SubscriptionState;
}

/**
 * Build a `cancel()` `TransactionInstruction`.
 *
 * @example Subscriber cancels their own subscription:
 * ```ts
 * const ix = await buildCancelIx({
 *   program,
 *   signer: subscriber.publicKey,
 *   subscriber: subscriber.publicKey,
 *   subscription: subPda,
 *   vault: vaultPda,
 *   merchantAta,
 *   subscriberAta,
 *   state: SubscriptionState.Active,
 * });
 * ```
 *
 * @example Merchant cancels (offboarding / compliance — ADR-009):
 * ```ts
 * const ix = await buildCancelIx({
 *   program,
 *   signer: merchant.publicKey,            // signer = merchant
 *   subscriber: subscriber.publicKey,      // rent recipient unchanged
 *   subscription: subPda,
 *   vault: vaultPda,
 *   merchantAta,
 *   subscriberAta,
 *   state: SubscriptionState.Active,
 * });
 * ```
 */
export async function buildCancelIx(
  args: BuildCancelIxArgs,
): Promise<TransactionInstruction> {
  const [gracedPda] = deriveGracedSubscriptionPda(
    args.program.programId,
    args.subscription,
  );

  const gracedSlot: PublicKey | null =
    args.state === SubscriptionState.GracePeriod ? gracedPda : null;

  // The `as any` cast is permanent under the current package layout, NOT
  // an IDL-staleness workaround: `Nakama` is aliased to the structural
  // `Idl` type in `../types.ts` because importing the generated
  // `nakama/target/types/nakama.ts` would expand tsconfig `rootDir`
  // outside `clients/ts/` and break the published `dist/` layout. The
  // IDL itself already carries the ADR-009 `cancel` signature; see
  // `types.ts` for the rootDir trade-off and the planned
  // `@nakama/idl-types` split.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .cancel()
    .accounts({
      signer: args.signer,
      subscription: args.subscription,
      subscriber: args.subscriber,
      vault: args.vault,
      merchantAta: args.merchantAta,
      subscriberAta: args.subscriberAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      gracedSubscription: gracedSlot,
    })
    .instruction();
}
