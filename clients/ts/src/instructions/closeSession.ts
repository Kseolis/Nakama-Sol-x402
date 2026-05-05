/**
 * `close_session` instruction builder (ADR-x402-001 §"close_session").
 *
 * Subscriber-signed PaySession close. Anchor `close = subscriber`
 * returns rent. NO `parent.state == Active` guard — close works even
 * when parent is in Cancelled tombstone (R1 closure).
 *
 * @see ADR-x402-001 §"close_session", §"Boundary contracts" R1 closure
 */

import { Program, BN } from "@anchor-lang/core";
import { PublicKey, TransactionInstruction } from "@solana/web3.js";

import { Nakama } from "../types";
import { derivePaySessionPda } from "../pdas";

export interface BuildCloseSessionIxArgs {
  program: Program<Nakama>;
  /** Subscriber wallet — must match `parent.subscriber`. */
  subscriber: PublicKey;
  subscription: PublicKey;
  sessionId: BN;
}

export async function buildCloseSessionIx(
  args: BuildCloseSessionIxArgs,
): Promise<TransactionInstruction> {
  const [paySessionPda] = derivePaySessionPda(
    args.program.programId,
    args.subscription,
    args.sessionId,
  );

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .closeSession()
    .accounts({
      parent: args.subscription,
      paySession: paySessionPda,
      subscriber: args.subscriber,
    })
    .instruction();
}
