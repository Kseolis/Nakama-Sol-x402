## Summary

<!-- One sentence: what this PR changes and why. -->

## Type

- [ ] feat (new feature)
- [ ] fix (bug)
- [ ] chore (deps / tooling)
- [ ] docs (no code change)
- [ ] test (test-only)
- [ ] refactor (no behaviour change)

## Target branch

- [ ] `stage` (default for feature / fix / chore work — Tier-1 gates)
- [ ] `main` (only from `stage`, only on release cut — Tier-2 gates)

## ADR reference

<!-- e.g. ADR-008 / ADR-x402-001 / N/A -->

## Test plan

- [ ] `make test` green (149 tests)
- [ ] `make lint` clean
- [ ] `make tsc` clean
- [ ] `make build` green (anchor build)
- [ ] New tests added (if applicable)

## CI gates

CI runs automatically per `.claude/rules/ci-gates.md`. Failing gate
blocks merge.
