/**
 * Nakama Protocol TypeScript SDK — public surface.
 *
 * Builders exported below (in addition to PDA helpers, type mirrors, and
 * F5 owner-check fetchers):
 *
 *   subscribe / charge          — on-chain primitives, used inline via
 *                                 `program.methods` (no dedicated builder)
 *   topUp                       — ADR-007 (grace recovery)
 *   cancel / cleanup            — ADR-009 / ADR-013 (decomposed teardown)
 *   pause / resume              — ADR-006
 *   openSession / settleUsage /
 *     closeSession              — ADR-x402-001 (PaySession lifecycle)
 *   resubscribe                 — ADR-008 (composite cleanup + subscribe
 *                                 + close_session × N)
 *   changeRate                  — ADR-005 (composite migration, post-MVP)
 *   computedStatus              — ADR-007 (off-chain status derivation)
 */

export {
  PLAN_SEED,
  SUB_SEED,
  VAULT_SEED,
  GRACE_SEED,
  PAY_SESSION_SEED,
  PAUSED_SUB_SEED,
  GRACE_DURATION_SECONDS,
  derivePlanPda,
  deriveSubscriptionPda,
  deriveVaultPda,
  deriveGracedSubscriptionPda,
  derivePaySessionPda,
  derivePausedSubscriptionPda,
} from "./pdas";

export {
  SubscriptionState,
  decodeSubscriptionState,
  type Nakama,
  type SubscriptionAccount,
  type GracedSubscriptionAccount,
  type PausedSubscriptionAccount,
} from "./types";

export {
  buildTopUpIx,
  type BuildTopUpIxArgs,
} from "./instructions/topUp";

export {
  buildCancelIx,
  type BuildCancelIxArgs,
} from "./instructions/cancel";

export {
  buildOpenSessionIx,
  type BuildOpenSessionIxArgs,
} from "./instructions/openSession";

export {
  buildSettleUsageIx,
  type BuildSettleUsageIxArgs,
} from "./instructions/settleUsage";

export {
  buildCloseSessionIx,
  type BuildCloseSessionIxArgs,
} from "./instructions/closeSession";

export { buildPauseIx, type BuildPauseIxArgs } from "./instructions/pause";

export { buildResumeIx, type BuildResumeIxArgs } from "./instructions/resume";

export {
  buildResubscribeIxs,
  findAlivePaySessions,
  resubscribeOrSubscribe,
  type BuildResubscribeIxsArgs,
  type ResubscribeOrSubscribeArgs,
  type ResubscribeOrSubscribeResult,
  type AlivePaySession,
} from "./instructions/resubscribe";

export {
  buildChangeRateTx,
  type ChangeRateOptions,
  type ChangeRateError,
} from "./instructions/changeRate";

export {
  deriveStatus,
  normalizeSubscriptionAccount,
  type ComputedStatus,
} from "./computedStatus";

// F5-mirror (ADR-015 §F5) — owner-check trust boundary for off-chain
// RPC reads. Mirror of `crates/nakama-client/src/accounts.rs`.
export {
  ANCHOR_DISCRIMINATOR_LEN,
  AccountFetchError,
  decodeProgramOwnedAccount,
  fetchProgramOwnedAccount,
  fetchProgramOwnedAccountNullable,
} from "./accounts";
