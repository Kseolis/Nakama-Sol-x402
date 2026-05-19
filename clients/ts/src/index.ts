/**
 * Nakama Protocol TypeScript SDK — public surface.
 *
 * Stage-2 of ADR-007 ships the top_up + computed-status flow. Other
 * instructions (subscribe, charge, cancel, cleanup) live elsewhere or
 * will be added in subsequent ADR cycles.
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
