/**
 * `resume` instruction builder (ADR-006 §"Resume handler").
 *
 * Merchant-only. Reads `paused_at` from satellite, shifts `stream_start
 * += pause_duration` for time-frozen continuity, closes satellite (rent
 * → merchant). State → Active.
 *
 * @see ADR-006 §"Resume handler", §6 continuity proof
 */

import { Program } from "@anchor-lang/core";
import { PublicKey, TransactionInstruction } from "@solana/web3.js";

import { Nakama } from "../types";
import { derivePausedSubscriptionPda } from "../pdas";

export interface BuildResumeIxArgs {
  program: Program<Nakama>;
  /** Merchant pubkey — must match the snapshot in subscription.merchant. */
  merchant: PublicKey;
  subscription: PublicKey;
}

export async function buildResumeIx(
  args: BuildResumeIxArgs,
): Promise<TransactionInstruction> {
  const [pausedPda] = derivePausedSubscriptionPda(
    args.program.programId,
    args.subscription,
  );

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .resume()
    .accounts({
      subscription: args.subscription,
      pausedSatellite: pausedPda,
      merchant: args.merchant,
    })
    .instruction();
}
