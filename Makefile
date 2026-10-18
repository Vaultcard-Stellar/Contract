.PHONY: all build test fmt lint clean coverage verify-oracle deploy demo

NETWORK    ?= testnet
TARGET     := wasm32v1-none
WASM       := target/$(TARGET)/release/collateral_vault.wasm
ORACLE     ?= CCYOZJCOPG34LLQQ7N24YXBM7LL62R7ONMZ3G6WZAAYPB5OYKOMJRN63
USDC       ?= CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA

all: fmt lint test build

## --workspace, not just the root package. mock-oracle is only a DEV-dependency of the vault, so
## a plain `cargo build` never produces its wasm — and scripts/demo.sh needs it to deploy a
## controllable price feed. Discovered by deploy.sh failing at the oracle step.
build:
	cargo build --workspace --target $(TARGET) --release
	@ls -lh $(WASM) | awk '{print "wasm: " $$5}'

test:
	cargo test --workspace

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean
	rm -rf .deploy

## Line coverage over the contract's own source, for the evidence package.
##
## The test modules are excluded from the measurement. Counting them inflates the figure with
## code that is by definition executed, and the question a reviewer is asking is how much of the
## CONTRACT the suite reaches — mock-oracle and mock-token are testnet scaffolding and go out for
## the same reason.
##
## Writes a summary a non-specialist can read and an lcov file a tool can. Needs cargo-llvm-cov:
##   cargo install cargo-llvm-cov --locked
coverage:
	@command -v cargo-llvm-cov >/dev/null 2>&1 \
		|| { echo "cargo-llvm-cov is not installed. Run: cargo install cargo-llvm-cov --locked"; exit 1; }
	@mkdir -p .deploy
	cargo llvm-cov --workspace \
		--ignore-filename-regex '(src/test/|mock-oracle/|mock-token/)' \
		--lcov --output-path .deploy/lcov.info
	@cargo llvm-cov report \
		--ignore-filename-regex '(src/test/|mock-oracle/|mock-token/)' \
		| tee .deploy/coverage.txt

## Three numbers that must be verified on chain before freezing the code — never taken from docs.
verify-oracle:
	@echo "oracle = $(ORACLE)"
	@stellar contract invoke --id $(ORACLE) --source $(KEY) --network $(NETWORK) -- decimals
	@stellar contract invoke --id $(ORACLE) --source $(KEY) --network $(NETWORK) -- resolution
	@stellar contract invoke --id $(ORACLE) --source $(KEY) --network $(NETWORK) -- \
		lastprice --asset '{"Other":"BTC"}'

## Deployment and construction happen in ONE transaction; splitting them leaves a window in
## which somebody else can initialise the contract and name themselves the lender.
## `stellar contract deploy -- <constructor args>` is the form that guarantees it.
## KEY is passed only when the caller set it. Passing `KEY=` unconditionally exports an EMPTY
## KEY, which is not the same as leaving it unset for a script that wants to default it — and it
## put `KEY is not set` in front of every reviewer whose first command was `make demo`.
deploy:
	$(if $(strip $(KEY)),KEY=$(KEY)) NETWORK=$(NETWORK) ./scripts/deploy.sh

## Clone on a clean machine, run one command, get every transaction hash.
##
## Every rejection step greps for its exact error code and fails the script on a mismatch —
## without that, a step failing for the WRONG reason still passes and the evidence package
## becomes a set of transactions nobody verified. See README.md §16.
##
## Takes about 35 minutes: the contract blocks liquidation for 30 minutes after the price feed
## recovers, and on testnet that wait is wall clock.
demo:
	$(if $(strip $(KEY)),KEY=$(KEY)) NETWORK=$(NETWORK) ./scripts/demo.sh
