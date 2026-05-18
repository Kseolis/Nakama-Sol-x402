# CI gate policy — Nakama Protocol

Versioned summary of the CI gate matrix. Full rationale + drift rules
live in `.claude/rules/ci-gates.md` (local-only per project convention).

## Branch topology

```
feature/*  ──PR──►  stage  ──PR──►  main
                  Tier-1 gates    Tier-2 gates
                  (~10 min)       (~25 min)
```

- `main` — pristine demo branch. Only updated via PR from `stage` after
  Tier-2 passes. No direct push, no feature merges.
- `stage` — main working branch during Colosseum judging period (introduced
  2026-05-18). Accepts feature/*, fix/*, chore/*, dependabot/*.
- `feature/*` — short-lived; deleted after merge to stage.

## Tier 1 — feature/* → stage

Fast feedback gates. All blocking.

| ID | Gate | Workflow file |
|---|---|---|
| T1-1 | `cargo fmt --check` (both workspaces) | `ci-feature.yml` |
| T1-2 | `tsc --noEmit` strict | `ci-feature.yml` |
| T1-3 | `cargo clippy --all-targets -- -D warnings` (both ws) | `ci-feature.yml` |
| T1-4 | `cargo test --workspace` (both ws) | `ci-feature.yml` |
| T1-6 | `actionlint .github/workflows/*.yml` | `ci-feature.yml` |

T1-5 anchor build runs only in Tier-2 — install cost (~8 min cargo install
of anchor-cli) breaks the Tier-1 12-min P95 budget.

## Tier 2 — stage → main

All Tier-1 gates plus full audit. All blocking unless noted.

| ID | Gate | Notes |
|---|---|---|
| T1-5 | Anchor build (.so + IDL) | Anchor 1.0.1 + Solana CLI 2.0.21 binaries cached |
| T2-1 | `cargo audit` (both ws) | Ignores `RUSTSEC-2025-0141` (bincode unmaintained, transitive via solana-*) |
| T2-2 | `semgrep` (pinned configs) | `p/rust`, `p/typescript`, `p/secrets` — NOT `auto` |
| T2-4 | ADR reference integrity | warn-only; silent skip if `docs/` not in checkout |
| T2-5 | Conventional commits (PR title) | warn-only |

T2-4/T2-5 promote from warn-only to blocking after two clean cycles.

## Nightly

`nightly.yml` runs T2-1 + T2-2 against `stage` HEAD on cron — early
detection of transitive RUSTSEC advisory drift between PRs.

## Project-specific notes

- **Russian prose in `.md` / `.claude/` / `docs/`** is intentional per
  `CLAUDE.md` "Output conventions". `.semgrepignore` excludes these
  paths so the homoglyph rule doesn't fire on prose. Code (`.rs`, `.ts`)
  is English-only by convention (verified: 0 Cyrillic in `.rs`, 1
  doc-comment in `.ts`).

- **`docs/` is gitignored** (per `.gitignore` `/docs/`). T2-4 gate
  acknowledges this and silently skips if the directory isn't present
  in the CI checkout.

- **`bincode` advisory**: `RUSTSEC-2025-0141` (unmaintained) is
  transitively pulled via solana-* 3.x. Upgrade path tracked separately;
  not gating release.

## Branch protection (manual setup)

CI workflows do NOT auto-enforce protection. Repo admin runs (one-time):

```bash
gh api -X PUT repos/{owner}/{repo}/branches/stage/protection \
  -H "Accept: application/vnd.github+json" \
  --input - <<'EOF'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "T1-1 Rust fmt (both workspaces)",
      "T1-2 TS strict typecheck",
      "T1-3 Rust clippy -D warnings",
      "T1-4 Rust tests (both workspaces)",
      "T1-6 actionlint workflows"
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false
}
EOF
```

Apply Tier-2 protection on `main` only after 5 clean Tier-1 cycles —
prevents false-positive lockout during initial bringup.

## Iteration policy

1. Ship `ci-feature.yml` first. Prove 5 clean cycles on real PRs.
2. After cycle 5, formalize `ci-release.yml` as required check on main.
3. Promote T2-4 / T2-5 from warn to blocking after two more cycles.
