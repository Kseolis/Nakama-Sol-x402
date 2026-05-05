#!/usr/bin/env ts-node
/**
 * Polymorphic `cancel` smoke script (ADR-009 stage-2 SDK demo).
 *
 * Demonstrates:
 *   - subscriber-initiated cancel (legacy path, single-actor flow)
 *   - merchant-initiated cancel (ADR-009: offboarding / compliance)
 *
 * The on-chain handler runtime-validates `signer ∈ {subscription.subscriber,
 * subscription.merchant}` so this script doesn't need to inspect the
 * Subscription before composing the tx — it just submits and reads the
 * post-state to confirm `state = Cancelled`.
 *
 * Usage:
 *   ts-node clients/ts/scripts/06-cancel-by-merchant.ts \
 *     --network=devnet \
 *     --keypair=~/.config/solana/id.json \
 *     --subscription=<sub-pda-base58> \
 *     --actor=merchant            # or `subscriber` (default)
 *
 * Defaults:
 *   --network = devnet
 *   --keypair = ~/.config/solana/id.json
 *   --actor   = subscriber
 *
 * Exit codes:
 *   0 — cancel succeeded; tombstone observable on-chain
 *   1 — argv / fs error
 *   2 — RPC / Anchor error (decoded code printed)
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";

import { AnchorProvider, Program, Wallet } from "@anchor-lang/core";
import {
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  clusterApiUrl,
} from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";

import {
  buildCancelIx,
  deriveVaultPda,
  normalizeSubscriptionAccount,
  SubscriptionState,
  type Nakama,
} from "../src";

interface ScriptArgs {
  network: "devnet" | "localnet";
  keypairPath: string;
  subscription: PublicKey;
  actor: "subscriber" | "merchant";
}

function parseArgs(argv: string[]): ScriptArgs {
  const args: Record<string, string> = {};
  for (const raw of argv.slice(2)) {
    const [k, v] = raw.replace(/^--/, "").split("=");
    if (k && v !== undefined) args[k] = v;
  }
  const network =
    args.network === "localnet" ? "localnet" : ("devnet" as const);
  const keypairPath = args.keypair
    ? args.keypair.replace(/^~(?=$|\/|\\)/, os.homedir())
    : path.join(os.homedir(), ".config", "solana", "id.json");
  const actor =
    args.actor === "merchant" ? "merchant" : ("subscriber" as const);

  if (!args.subscription) {
    throw new Error("missing --subscription=<base58 pubkey>");
  }
  return {
    network,
    keypairPath,
    subscription: new PublicKey(args.subscription),
    actor,
  };
}

function loadKeypair(filePath: string): Keypair {
  const raw = fs.readFileSync(filePath, "utf8");
  const bytes = Uint8Array.from(JSON.parse(raw) as number[]);
  return Keypair.fromSecretKey(bytes);
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
  const programId = new PublicKey(idl.address);
  return { idl, programId };
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
      const name = m[1];
      const matched = knownNames.includes(name) ? name : `${name}?`;
      return `${matched} (${m[2]})`;
    }
  }
  return e?.message ?? String(err);
}

async function main() {
  const args = parseArgs(process.argv);
  const signer = loadKeypair(args.keypairPath);
  const connection = new Connection(rpcUrlFor(args.network), "confirmed");
  const wallet = new Wallet(signer);
  const provider = new AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });

  const { idl, programId } = loadIdl();
  const program = new Program(idl, provider) as unknown as Program<Nakama>;

  console.log(`Network:      ${args.network}`);
  console.log(`Program ID:   ${programId.toBase58()}`);
  console.log(`Actor:        ${args.actor}`);
  console.log(`Signer:       ${signer.publicKey.toBase58()}`);
  console.log(`Subscription: ${args.subscription.toBase58()}`);
  console.log("");

  // Read pre-state to derive accounts + verify caller intent.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const subAccountRaw = await (program.account as any).subscription.fetch(
    args.subscription,
  );
  const sub = normalizeSubscriptionAccount(subAccountRaw);

  console.log("Pre-state:");
  console.log(`  state       = ${SubscriptionState[sub.state]}`);
  console.log(`  subscriber  = ${sub.subscriber.toBase58()}`);
  console.log(`  merchant    = ${sub.merchant.toBase58()}`);
  console.log("");

  // ADR-009 polymorphic check (client-side guard for clearer errors than
  // surfacing NoCancelAuthority from the runtime).
  const expected =
    args.actor === "merchant" ? sub.merchant : sub.subscriber;
  if (!signer.publicKey.equals(expected)) {
    console.error(
      `signer pubkey ${signer.publicKey.toBase58()} does not match expected ${args.actor} = ${expected.toBase58()}`,
    );
    process.exit(1);
  }

  const subscriberAta = getAssociatedTokenAddressSync(
    sub.tokenMint,
    sub.subscriber,
  );
  const [vault] = deriveVaultPda(programId, args.subscription);

  const ix = await buildCancelIx({
    program,
    signer: signer.publicKey,
    subscriber: sub.subscriber,
    subscription: args.subscription,
    vault,
    merchantAta: sub.merchantAta,
    subscriberAta,
    state: sub.state,
  });
  const tx = new Transaction().add(ix);

  let signature: string;
  try {
    signature = await provider.sendAndConfirm(tx, [signer], {
      commitment: "confirmed",
    });
  } catch (err) {
    const decoded = decodeAnchorError(err, idl);
    console.error(`cancel FAILED: ${decoded}`);
    process.exit(2);
  }
  console.log(`cancel tx:    ${signature}`);
  console.log("");

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const postRaw = await (program.account as any).subscription.fetch(
    args.subscription,
  );
  const post = normalizeSubscriptionAccount(postRaw);
  console.log("Post-state:");
  console.log(`  state = ${SubscriptionState[post.state]}`);
  console.log(
    "  (Subscription preserved as tombstone per ADR-013 — call cleanup() to reclaim rent.)",
  );
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
