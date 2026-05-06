/**
 * `pause` instruction builder (ADR-006 §"Pause handler").
 *
 * Merchant-only. Creates the `PausedSubscription` satellite at
 * `paused_at = now`, flips state Active → Paused. Streaming math freezes
 * for the pause window — charge refuses Paused; resume shifts
 * `stream_start` by pause_duration so subscriber loses no funds.
 *
 * @see ADR-006 §"Pause handler"
 */

import { Program } from "@anchor-lang/core";
import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from "@solana/web3.js";

import { Nakama } from "../types";
import { derivePausedSubscriptionPda } from "../pdas";

export interface BuildPauseIxArgs {
  program: Program<Nakama>;
  /** Merchant pubkey — must match `subscription.merchant` AND be tx signer. */
  merchant: PublicKey;
  /** Subscription PDA. */
  subscription: PublicKey;
}

export async function buildPauseIx(
  args: BuildPauseIxArgs,
): Promise<TransactionInstruction> {
  const [pausedPda] = derivePausedSubscriptionPda(
    args.program.programId,
    args.subscription,
  );

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .pause()
    .accounts({
      subscription: args.subscription,
      pausedSatellite: pausedPda,
      merchant: args.merchant,
      systemProgram: SystemProgram.programId,
    })
    .instruction();
}
