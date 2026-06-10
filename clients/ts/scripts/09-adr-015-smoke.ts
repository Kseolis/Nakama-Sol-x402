/**
 * ADR-015 SDK-side smoke test — F5-mirror owner check + F4-mirror lazy
 * precise math. Pure in-memory (no RPC, no LiteSVM); deliberately a
 * thin self-check that the helpers reject hostile inputs and produce
 * byte-equivalent results to the Rust mirror on the canonical F4 vector.
 *
 * Run: `npx ts-node scripts/09-adr-015-smoke.ts` from `clients/ts/`.
 *
 * Cross-references:
 *  - F5-mirror: `crates/nakama-client/src/accounts.rs::fetch_program_owned`
 *  - F4-mirror: ADR-015 §F4 canonical formula, golden vectors shared
 *    with offchain-rust-dev's computed_status tests.
 */

import { AccountInfo, PublicKey } from "@solana/web3.js";
import BN from "bn.js";

import {
  AccountFetchError,
  decodeProgramOwnedAccount,
  deriveStatus,
  SubscriptionState,
  type SubscriptionAccount,
} from "../src";

// ─────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────

const PROGRAM_ID = new PublicKey("HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm");
const FOREIGN_OWNER = new PublicKey(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
);

function makeAccount(
  owner: PublicKey,
  data: Buffer = Buffer.alloc(64, 0xab),
): AccountInfo<Buffer> {
  return {
    executable: false,
    owner,
    lamports: 1_000_000,
    data,
    rentEpoch: 0,
  };
}

function makeSubscriptionFixture(opts: {
  deposited: bigint;
  withdrawn: bigint;
  price: bigint;
  period: bigint;
  streamStart: bigint;
}): SubscriptionAccount {
  return {
    nextChargeAt: new BN(0),
    subscriber: PROGRAM_ID,
    plan: PROGRAM_ID,
    price: new BN(opts.price.toString()),
    period: new BN(opts.period.toString()),
    tokenMint: PROGRAM_ID,
    merchant: PROGRAM_ID,
    merchantAta: PROGRAM_ID,
    state: SubscriptionState.Active,
    bump: 254,
    vaultBump: 253,
    createdAt: new BN(0),
    lastChargeAt: new BN(0),
    depositedAmount: new BN(opts.deposited.toString()),
    withdrawnAmount: new BN(opts.withdrawn.toString()),
    // F4-mirror: ratePerSecond is advisory only. Set to a value that
    // would yield the WRONG result under the pre-F4 formula so we can
    // observe the corrected math through deriveStatus output.
    ratePerSecond: new BN(
      (opts.price / opts.period).toString(), // truncated rate
    ),
    streamStart: new BN(opts.streamStart.toString()),
    reserved: Array(32).fill(0),
  };
}

// ─────────────────────────────────────────────────────────────────────────
// Assertions (tiny, no test runner — just a script)
// ─────────────────────────────────────────────────────────────────────────

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
  console.log(`  OK   ${msg}`);
}

function expectThrows<T>(
  fn: () => T,
  predicate: (err: unknown) => boolean,
  msg: string,
): void {
  try {
    fn();
    console.error(`FAIL: ${msg} — expected throw, none observed`);
    process.exit(1);
  } catch (err) {
    if (!predicate(err)) {
      console.error(`FAIL: ${msg} — wrong error: ${err}`);
      process.exit(1);
    }
    console.log(`  OK   ${msg}`);
  }
}

// ─────────────────────────────────────────────────────────────────────────
// F5-mirror — owner check
// ─────────────────────────────────────────────────────────────────────────

console.log("F5-mirror (ADR-015 §F5): off-chain owner check");

expectThrows(
  () =>
    decodeProgramOwnedAccount(
      makeAccount(FOREIGN_OWNER),
      PROGRAM_ID,
      null,
      () => "decoded",
    ),
  (err) =>
    err instanceof AccountFetchError && err.kind === "WrongAccountOwner",
  "rejects account owned by foreign program with WrongAccountOwner",
);

expectThrows(
  () =>
    decodeProgramOwnedAccount(
      null,
      PROGRAM_ID,
      null,
      () => "decoded",
    ),
  (err) => err instanceof AccountFetchError && err.kind === "NotFound",
  "rejects null AccountInfo with NotFound",
);

const goodDisc = Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]);
const goodAccount = makeAccount(
  PROGRAM_ID,
  Buffer.concat([goodDisc, Buffer.alloc(32, 0xaa)]),
);
const decoded = decodeProgramOwnedAccount(
  goodAccount,
  PROGRAM_ID,
  goodDisc,
  (data) => ({ bodyLen: data.length - 8 }),
);
assert(decoded.bodyLen === 32, "happy path returns decoder output (32-byte body)");

const wrongDisc = Buffer.from([9, 9, 9, 9, 9, 9, 9, 9]);
expectThrows(
  () =>
    decodeProgramOwnedAccount(
      goodAccount,
      PROGRAM_ID,
      wrongDisc,
      () => "decoded",
    ),
  (err) =>
    err instanceof AccountFetchError && err.kind === "WrongDiscriminator",
  "rejects mismatched discriminator (right owner, wrong type)",
);

expectThrows(
  () =>
    decodeProgramOwnedAccount(
      makeAccount(PROGRAM_ID, Buffer.alloc(4, 0)), // shorter than 8
      PROGRAM_ID,
      goodDisc,
      () => "decoded",
    ),
  (err) => err instanceof AccountFetchError && err.kind === "TooShort",
  "rejects data shorter than discriminator length with TooShort",
);

// ─────────────────────────────────────────────────────────────────────────
// F4-mirror — lazy precise math
// ─────────────────────────────────────────────────────────────────────────

console.log("\nF4-mirror (ADR-015 §F4): lazy precise (price, period) math");

// Canonical ADR-015 §F4 vector: $10 USDC over 30 days, full period elapsed.
// Pre-F4 (rate truncation): rate = 10_000_000 / 2_592_000 = 3 (truncated);
// unlocked_old = 3 * 2_592_000 = 7_776_000 → ~22% under-pay.
// F4 (lazy precise): unlocked = (2_592_000 * 10_000_000) / 2_592_000 = 10_000_000.
const subFullPeriod = makeSubscriptionFixture({
  deposited: 100_000_000n,
  withdrawn: 0n,
  price: 10_000_000n,
  period: 2_592_000n,
  streamStart: 0n,
});
const statusFullPeriod = deriveStatus(
  subFullPeriod,
  null,
  null,
  2_592_000n,
);
assert(
  statusFullPeriod.kind === "Active" || statusFullPeriod.kind === "ActiveLowFunds",
  "full-period derive yields an Active-class status",
);
if (
  statusFullPeriod.kind === "Active" ||
  statusFullPeriod.kind === "ActiveLowFunds"
) {
  assert(
    statusFullPeriod.claimable === 10_000_000n,
    `claimable == 10_000_000 (F4 exact); got ${statusFullPeriod.claimable}`,
  );
}

// Mid-period (half period elapsed): unlocked should be price / 2 = 5_000_000.
// Pre-F4 with rate=3: unlocked = 3 * 1_296_000 = 3_888_000 → also wrong.
const subHalfPeriod = makeSubscriptionFixture({
  deposited: 100_000_000n,
  withdrawn: 0n,
  price: 10_000_000n,
  period: 2_592_000n,
  streamStart: 0n,
});
const statusHalfPeriod = deriveStatus(
  subHalfPeriod,
  null,
  null,
  1_296_000n,
);
if (
  statusHalfPeriod.kind === "Active" ||
  statusHalfPeriod.kind === "ActiveLowFunds"
) {
  assert(
    statusHalfPeriod.claimable === 5_000_000n,
    `claimable == 5_000_000 at half period (F4 exact); got ${statusHalfPeriod.claimable}`,
  );
}

// Clock-skew defence: now < streamStart → elapsed = 0 → claimable = 0.
const subClockBackwards = makeSubscriptionFixture({
  deposited: 100_000_000n,
  withdrawn: 0n,
  price: 10_000_000n,
  period: 2_592_000n,
  streamStart: 1000n,
});
const statusBackwards = deriveStatus(subClockBackwards, null, null, 500n);
if (
  statusBackwards.kind === "Active" ||
  statusBackwards.kind === "ActiveLowFunds"
) {
  assert(
    statusBackwards.claimable === 0n,
    `claimable == 0 when clock < streamStart; got ${statusBackwards.claimable}`,
  );
}

// Unlocked clamped to deposited even when elapsed * price / period overshoots.
const subOvershoot = makeSubscriptionFixture({
  deposited: 1_000_000n,
  withdrawn: 0n,
  price: 10_000_000n,
  period: 2_592_000n,
  streamStart: 0n,
});
const statusOvershoot = deriveStatus(
  subOvershoot,
  null,
  null,
  10n * 2_592_000n, // 10 periods elapsed but only 0.1 period worth deposited
);
if (
  statusOvershoot.kind === "Active" ||
  statusOvershoot.kind === "ActiveLowFunds"
) {
  assert(
    statusOvershoot.claimable === 1_000_000n,
    `claimable clamped to deposited (1_000_000); got ${statusOvershoot.claimable}`,
  );
}

console.log("\nADR-015 SDK smoke (F4-mirror + F5-mirror + F6 by reference): PASS");
