# Nakama Protocol — convenience targets.
# Single entry point for demo / build / verify. See docs/demo-cheatsheet.md
# for the recording flow.

PROGRAM_ID    := HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm
USDC_DEVNET   := 4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU
EXPLORER_BASE := https://explorer.solana.com

.DEFAULT_GOAL := help
.PHONY: help demo demo-deps build test test-onchain test-offchain tsc lint \
        explorer-program clean status

help:
	@echo "Nakama Protocol — make targets"
	@echo ""
	@echo "  Demo (devnet):"
	@echo "    make demo              run full e2e demo (~2 min, 11 phases)"
	@echo "    make explorer-program  print Explorer URL of deployed program"
	@echo ""
	@echo "  Build & verify:"
	@echo "    make build             anchor build (program .so + IDL)"
	@echo "    make test              cargo test, both workspaces (149 tests)"
	@echo "    make test-onchain      only LiteSVM program tests (117)"
	@echo "    make test-offchain     only off-chain crate tests (32)"
	@echo "    make tsc               TypeScript typecheck (no emit)"
	@echo "    make lint              cargo clippy --all-targets -D warnings"
	@echo ""
	@echo "  Misc:"
	@echo "    make status            show repo state, program deployment, last commit"
	@echo "    make demo-deps         install TS deps once before first demo run"
	@echo "    make clean             remove derivable demo stdout log"

demo-deps:
	cd clients/ts && npm install --silent

demo:
	cd clients/ts && npx ts-node scripts/00-full-demo.ts

build:
	cd nakama && anchor build

test: test-onchain test-offchain

test-onchain:
	cd nakama && cargo test --workspace --tests

test-offchain:
	cargo test --workspace

tsc:
	cd clients/ts && npx tsc --noEmit

lint:
	cd nakama && cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets -- -D warnings

explorer-program:
	@echo "$(EXPLORER_BASE)/address/$(PROGRAM_ID)?cluster=devnet"

status:
	@echo "Program ID: $(PROGRAM_ID)"
	@echo "USDC mint:  $(USDC_DEVNET)"
	@echo "Cluster:    devnet"
	@echo ""
	@echo "Last commit:"
	@git log --oneline -1
	@echo ""
	@echo "Working tree:"
	@git status --short || true

clean:
	@rm -f clients/ts/scripts/.demo-stdout.log
	@echo "Cleaned derivable artifacts."
