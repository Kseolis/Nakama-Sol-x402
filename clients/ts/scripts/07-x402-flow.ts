#!/usr/bin/env ts-node
/**
 * Loom-pitch x402 demo (ADR-x402-001 Phase 5).
 *
 * Walkthrough — the "same escrow, two billing models" pitch moment:
 *   1. open_session(session_id, facilitator, reservation_cap)
 *   2. simulate N API calls — each settle_usage(amount) by facilitator
 *   3. close_session — rent → subscriber, escrow stays in parent vault
 *
 * Pre-requisites (run beforehand):
 *   - 03-create-plan.ts (or whichever creates the Plan)
 *   - 04-subscribe.ts   (creates Subscription with fully-funded vault)
 *   - This script picks up an existing Subscription PDA via --subscription
 *
 * Usage:
 *   ts-node clients/ts/scripts/07-x402-flow.ts \
 *     --network=devnet \
 *     --subscriber-keypair=~/.config/solana/id.json \
 *     --facilitator-keypair=./facilitator-keypair.json \
 *     --subscription=<sub-pda-base58> \
 *     --session-id=42 \
 *     --reservation-cap=300000000 \
 *     --settle-amount=50000000 \
 *     --settles=3
 *
 * Defaults:
 *   --network              = devnet
 *   --subscriber-keypair   = ~/.config/solana/id.json
 *   --facilitator-keypair  = $PWD/facilitator-keypair.json
 *   --session-id           = random u64
 *   --reservation-cap      = 300_000_000 (300 USDC, base units)
 *   --settle-amount        = 50_000_000  (50 USDC)
 *   --settles              = 3
 *
 * Exit codes:
 *   0 — full demo succeeded; PaySession closed; rent returned
 *   1 — argv / fs error
 *   2 — RPC / Anchor error during open / settle / close
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as crypto from "crypto";

import { AnchorProvider, BN, Program, Wallet } from "@anchor-lang/core";
import {
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  clusterApiUrl,
} from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";

import {
  buildOpenSessionIx,
  buildSettleUsageIx,
  buildCloseSessionIx,
  derivePaySessionPda,
  normalizeSubscriptionAccount,
  type Nakama,
} from "../src";

// --------------------------------------------------------------------------
// argv
// --------------------------------------------------------------------------

interface ScriptArgs {
  network: "devnet" | "localnet";
  subscriberKeypair: string;
  facilitatorKeypair: string;
  subscription: PublicKey;
  sessionId: BN;
  reservationCap: BN;
  settleAmount: BN;
  settles: number;
}

function parseArgs(argv: string[]): ScriptArgs {
  const args: Record<string, string> = {};
  for (const raw of argv.slice(2)) {
    const [k, v] = raw.replace(/^--/, "").split("=");
    if (k && v !== undefined) args[k] = v;
  }
  const network =
    args.network === "localnet" ? "localnet" : ("devnet" as const);
  const subscriberKeypair = (
    args["subscriber-keypair"] ??
    path.join(os.homedir(), ".config", "solana", "id.json")
  ).replace(/^~(?=$|\/|\\)/, os.homedir());
  const facilitatorKeypair = (
    args["facilitator-keypair"] ??
    path.join(process.cwd(), "facilitator-keypair.json")
  ).replace(/^~(?=$|\/|\\)/, os.homedir());

  if (!args.subscription) {
    throw new Error("missing --subscription=<base58 pubkey>");
  }

  // Random u64 if not supplied. Collision probability 2^-64 — Q2 trade-off.
  const sessionId = args["session-id"]
    ? new BN(args["session-id"])
    : new BN(crypto.randomBytes(7).toString("hex"), 16); // 56-bit random fits BN

  return {
    network,
    subscriberKeypair,
    facilitatorKeypair,
    subscription: new PublicKey(args.subscription),
    sessionId,
    reservationCap: new BN(args["reservation-cap"] ?? "300000000"),
    settleAmount: new BN(args["settle-amount"] ?? "50000000"),
    settles: args.settles ? parseInt(args.settles, 10) : 3,
  };
}

function loadKeypair(filePath: string): Keypair {
  const raw = fs.readFileSync(filePath, "utf8");
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(raw) as number[]));
}

function rpcUrlFor(network: "devnet" | "localnet"): string {
  return network === "localnet"
    ? "http://127.0.0.1:8899"
    : clusterApiUrl("devnet");
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function loadIdl(): { idl: any; programId: PublicKey } {
  const idlPath = path.resolve(
    __dirname,
    "../../../nakama/target/idl/nakama.json",
  );
  const idl = JSON.parse(fs.readFileSync(idlPath, "utf8"));
  return { idl, programId: new PublicKey(idl.address) };
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function decodeAnchorError(err: unknown, idl: any): string {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const e = err as any;
  if (e?.error?.errorCode?.code) {
    return `${e.error.errorCode.code} (${e.error.errorCode.number})`;
  }
  const logs: string[] = e?.logs ?? e?.transactionLogs ?? [];
  for (const line of logs) {
    const m = line.match(/Error Code:\s*(\w+)\.\s*Error Number:\s*(\d+)/);
    if (m) {
      const knownNames =
        idl.errors?.map((er: { name: string }) => er.name) ?? [];
      const matched = knownNames.includes(m[1]) ? m[1] : `${m[1]}?`;
      return `${matched} (${m[2]})`;
    }
  }
  return e?.message ?? String(err);
}

// --------------------------------------------------------------------------
// main
// --------------------------------------------------------------------------

async function main() {
  const args = parseArgs(process.argv);
  const subscriber = loadKeypair(args.subscriberKeypair);
  const facilitator = loadKeypair(args.facilitatorKeypair);

  const connection = new Connection(rpcUrlFor(args.network), "confirmed");
  const subscriberWallet = new Wallet(subscriber);
  const facilitatorWallet = new Wallet(facilitator);

  const { idl, programId } = loadIdl();
  const subscriberProvider = new AnchorProvider(
    connection,
    subscriberWallet,
    {
      commitment: "confirmed",
    },
  );
  const facilitatorProvider = new AnchorProvider(
    connection,
    facilitatorWallet,
    {
      commitment: "confirmed",
    },
  );

  const subscriberProgram = new Program(
    idl,
    subscriberProvider,
  ) as unknown as Program<Nakama>;
  const facilitatorProgram = new Program(
    idl,
    facilitatorProvider,
  ) as unknown as Program<Nakama>;

  console.log(`Network:        ${args.network}`);
  console.log(`Program ID:     ${programId.toBase58()}`);
  console.log(`Subscriber:     ${subscriber.publicKey.toBase58()}`);
  console.log(`Facilitator:    ${facilitator.publicKey.toBase58()}`);
  console.log(`Subscription:   ${args.subscription.toBase58()}`);
  console.log(`Session ID:     ${args.sessionId.toString()}`);
  console.log(`Reservation:    ${args.reservationCap.toString()} base units`);
  console.log("");

  // Pre-flight: read parent Subscription so we know merchant_ata.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const subRaw = await (subscriberProgram.account as any).subscription.fetch(
    args.subscription,
  );
  const sub = normalizeSubscriptionAccount(subRaw);
  console.log(`Parent state:   ${sub.state}`);
  console.log(
    `Parent escrow:  deposited=${sub.depositedAmount.toString()}, withdrawn=${sub.withdrawnAmount.toString()}`,
  );
  console.log("");

  // ── 1. open_session ──
  console.log(
    `[1/3] open_session(id=${args.sessionId}, cap=${args.reservationCap})`,
  );
  try {
    const openIx = await buildOpenSessionIx({
      program: subscriberProgram,
      subscriber: subscriber.publicKey,
      subscription: args.subscription,
      sessionId: args.sessionId,
      facilitator: facilitator.publicKey,
      reservationCap: args.reservationCap,
    });
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(openIx),
      [subscriber],
      { commitment: "confirmed" },
    );
    console.log(`      tx: ${sig}`);
  } catch (err) {
    console.error(`      open_session FAILED: ${decodeAnchorError(err, idl)}`);
    process.exit(2);
  }

  const [paySessionPda] = derivePaySessionPda(
    programId,
    args.subscription,
    args.sessionId,
  );

  // Use the snapshot's merchant_ata for settle (guaranteed by ADR snapshot
  // at open_session time).
  const merchantAta = getAssociatedTokenAddressSync(
    sub.tokenMint,
    sub.merchant,
  );
  void merchantAta; // ATA derived but we read the snapshot from the just-opened PaySession below

  // ── 2. simulate API calls ── settle N times ──
  console.log(
    `[2/3] simulate ${args.settles} API calls — settle_usage(amount=${args.settleAmount})`,
  );
  for (let i = 0; i < args.settles; i++) {
    try {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const sessRaw = await (
        facilitatorProgram.account as any
      ).paySession.fetch(paySessionPda);
      const settleIx = await buildSettleUsageIx({
        program: facilitatorProgram,
        facilitator: facilitator.publicKey,
        subscription: args.subscription,
        sessionId: args.sessionId,
        merchantAta: sessRaw.merchantAta as PublicKey,
        amount: args.settleAmount,
      });
      const sig = await facilitatorProvider.sendAndConfirm(
        new Transaction().add(settleIx),
        [facilitator],
        { commitment: "confirmed" },
      );
      console.log(`      [${i + 1}/${args.settles}] tx: ${sig}`);
    } catch (err) {
      console.error(
        `      settle_usage[${i}] FAILED: ${decodeAnchorError(err, idl)}`,
      );
      process.exit(2);
    }
  }

  // ── 3. close_session ──
  console.log(`[3/3] close_session — return rent to subscriber`);
  try {
    const closeIx = await buildCloseSessionIx({
      program: subscriberProgram,
      subscriber: subscriber.publicKey,
      subscription: args.subscription,
      sessionId: args.sessionId,
    });
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(closeIx),
      [subscriber],
      { commitment: "confirmed" },
    );
    console.log(`      tx: ${sig}`);
  } catch (err) {
    console.error(`      close_session FAILED: ${decodeAnchorError(err, idl)}`);
    process.exit(2);
  }

  // Post-state: parent escrow shows updated withdrawn_amount.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const finalRaw = await (subscriberProgram.account as any).subscription.fetch(
    args.subscription,
  );
  const final = normalizeSubscriptionAccount(finalRaw);
  console.log("");
  console.log("Final parent state:");
  console.log(`  withdrawn = ${final.withdrawnAmount.toString()}`);
  console.log(
    `  delta     = ${final.withdrawnAmount.sub(sub.withdrawnAmount).toString()} (== Σ settles)`,
  );
  console.log("");
  console.log("Demo complete — same escrow, two billing models.");
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
