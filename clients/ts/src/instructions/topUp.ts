/**
 * `top_up` instruction builder (ADR-007 §"top_up handler").
 *
 * Single-ix design: covers Active, Paused, GracePeriod state branches
 * via Anchor 1.0.1 `Option<Account<GracedSubscription>>`. Caller passes
 * the satellite PDA only when state == GracePeriod; otherwise `null`.
 *
 * Authority: subscriber-only. `has_one = subscriber` + Signer<'info>
 * enforced on-chain (ADR-007 I-TOPUP-1, A-1 adversarial case).
 *
 * @see ADR-007 §"top_up handler"
 * @see ADR-007 §"Per-state eligibility table"
 */

import { Program, BN } from "@anchor-lang/core";
import {
  PublicKey,
  TransactionInstruction,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";

import { Nakama, SubscriptionState } from "../types";
import { deriveGracedSubscriptionPda } from "../pdas";

export interface BuildTopUpIxArgs {
  /** Anchor `Program<Nakama>` from `target/types/nakama.ts`. */
  program: Program<Nakama>;
  /** Subscriber wallet pubkey — must be the tx signer. */
  subscriber: PublicKey;
  /** Subscription PDA (derive via `deriveSubscriptionPda`). */
  subscription: PublicKey;
  /** Subscriber USDC ATA (source of CPI transfer). */
  subscriberAta: PublicKey;
  /** Per-subscription vault PDA (CPI transfer destination). */
  vault: PublicKey;
  /** Top-up amount in USDC base units (u64). MUST be `> 0` (I-TOPUP-2). */
  amount: bigint;
  /**
   * Current Subscription FSM state.
   *
   * Drives the optional GracedSubscription satellite slot:
   *  - Active / Paused → `null` (no satellite to close)
   *  - GracePeriod → satellite PDA passed for Anchor `close = subscriber`
   *
   * Caller is responsible for fetching `subscription.state` first; this
   * builder does NOT round-trip to RPC.
   */
  state: SubscriptionState;
}

/**
 * Build a `top_up(amount)` `TransactionInstruction`.
 *
 * Does not sign or send — caller composes into a `Transaction` /
 * `VersionedTransaction` and dispatches via their preferred path
 * (wallet adapter, raw `RpcClient`, etc.).
 *
 * @example
 * ```ts
 * const ix = await buildTopUpIx({
 *   program,
 *   subscriber: subscriber.publicKey,
 *   subscription: subPda,
 *   subscriberAta,
 *   vault: vaultPda,
 *   amount: 86_400_000_000n, // 86_400 USDC
 *   state: SubscriptionState.GracePeriod,
 * });
 * const sig = await program.provider.sendAndConfirm(
 *   new Transaction().add(ix),
 *   [subscriber],
 * );
 * ```
 */
export async function buildTopUpIx(
  args: BuildTopUpIxArgs,
): Promise<TransactionInstruction> {
  const [gracedPda] = deriveGracedSubscriptionPda(
    args.program.programId,
    args.subscription,
  );

  // ADR-007 single-ix design: graced_subscription is `Option<Account<...>>`
  // on-chain. Anchor TS clients pass `null` when state ∈ {Active, Paused}.
  //
  // Coordination note (kickoff §7.5 Q9): if the on-chain crate enables the
  // `allow-missing-optionals` cargo feature, the account is fully omitted
  // from the AccountMeta list. Without that feature, Anchor passes the
  // program ID as a placeholder pubkey when the JS value is `null`.
  // Either path is wire-compatible — we hand Anchor `null` and let it
  // serialise correctly.
  const gracedSlot: PublicKey | null =
    args.state === SubscriptionState.GracePeriod ? gracedPda : null;

  // The `as any` cast is permanent under the current package layout, NOT
  // an IDL-staleness workaround: `Nakama` is aliased to the structural
  // `Idl` type in `../types.ts` because importing the generated
  // `nakama/target/types/nakama.ts` would expand tsconfig `rootDir`
  // outside `clients/ts/` and break the published `dist/` layout. The
  // IDL itself already contains `top_up` (ADR-007 shipped 2026-05-05);
  // see `types.ts` for the rootDir trade-off and the planned
  // `@nakama/idl-types` split.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .topUp(new BN(args.amount.toString()))
    .accounts({
      subscriber: args.subscriber,
      subscription: args.subscription,
      gracedSubscription: gracedSlot,
      subscriberAta: args.subscriberAta,
      vault: args.vault,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .instruction();
}
