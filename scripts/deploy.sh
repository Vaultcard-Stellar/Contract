#!/usr/bin/env bash
#
# Deploy a market to testnet.
#
# # One transaction, always
#
# `__constructor` runs inside the deploy, so everything after `--` below is constructor
# arguments. There is no separate `initialize` step and there must not be one: a two-step deploy
# leaves a window in which somebody else can initialise the contract and name themselves the
# lender, and that window is open for as long as it takes the second transaction to land.
#
# # What this writes
#
# `.deploy/env`, which `demo.sh` sources. Addresses only — the identities live in the Stellar
# CLI's own keystore and no secret is written here.
#
# Usage:
#   ./scripts/deploy.sh                         # mock tokens and mock oracle, for the demo.
#                                               # KEY defaults to vc_deployer, created and funded
#                                               # on testnet if it does not exist yet
#   KEY=deployer ORACLE=C… COLLATERAL=C… ./scripts/deploy.sh   # against real ones

set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib.sh

TARGET=wasm32v1-none
VAULT_WASM="target/$TARGET/release/collateral_vault.wasm"
TOKEN_WASM="target/$TARGET/release/mock_token.wasm"
ORACLE_WASM="target/$TARGET/release/mock_oracle.wasm"

# Risk parameters. The deck's bridged-BTC profile.
#
# These three — haircut, max LTV, liquidation threshold — are ESTIMATES until they are
# recalibrated against real volatility data, which is gate G2. They are deliberately written out
# here rather than defaulted somewhere, because a market is deployed with them frozen and the
# only way to change one is to deploy a new market.
HAIRCUT_BPS="${HAIRCUT_BPS:-1200}"
MAX_LTV_BPS="${MAX_LTV_BPS:-5000}"
LIQ_THRESHOLD_BPS="${LIQ_THRESHOLD_BPS:-7000}"
FULL_LIQ_LTV_BPS="${FULL_LIQ_LTV_BPS:-9000}"
LIQ_BONUS_BPS="${LIQ_BONUS_BPS:-800}"
CLOSE_FACTOR_BPS="${CLOSE_FACTOR_BPS:-5000}"
PRICE_MAX_AGE="${PRICE_MAX_AGE:-300}"
RATE_BPS_YEAR="${RATE_BPS_YEAR:-1000}"
VAULT_CAP="${VAULT_CAP:-10000000000000}"      # $1,000,000 at 7 decimals
DUST_THRESHOLD="${DUST_THRESHOLD:-100000000}" # $10 at 7 decimals
DEPOSIT_FEED="${DEPOSIT_FEED:-BTC}"

preflight

step "Building"
make build >/dev/null
# Checked one by one, and named. A missing wasm otherwise surfaces as the CLI failing to read a
# file three steps later, which is a confusing way to learn that the build did not build
# everything — mock-oracle is only a dev-dependency, so it needs `--workspace`.
for w in "$VAULT_WASM" "$TOKEN_WASM" "$ORACLE_WASM"; do
    [ -f "$w" ] || die "$w was not produced. Run 'make build' — it needs --workspace, because the
mock oracle is a dev-dependency of the vault and a plain cargo build skips it."
done
ok "vault $(du -h "$VAULT_WASM" | cut -f1) · token · oracle"

DEPLOYER=$(address_of "$KEY")
say "  deployer $DEPLOYER"

# ---------------------------------------------------------------------------
# Dependencies
# ---------------------------------------------------------------------------

step "Dependencies"

if [ -z "${COLLATERAL:-}" ]; then
    # 8 decimals, deliberately unlike the debt token's 7. A market where both are 7 exercises
    # none of the rescaling, and the rescale is where a power-of-ten error hides.
    COLLATERAL=$(stellar contract deploy --wasm "$TOKEN_WASM" \
        --source "$KEY" --network "$NETWORK" -- --decimals 8 2>/dev/null | tail -1)
    ok "collateral token (8 dp) $COLLATERAL"
else
    ok "collateral token (given) $COLLATERAL"
fi

if [ -z "${DEBT:-}" ]; then
    DEBT=$(stellar contract deploy --wasm "$TOKEN_WASM" \
        --source "$KEY" --network "$NETWORK" -- --decimals 7 2>/dev/null | tail -1)
    ok "debt token (7 dp) $DEBT"
else
    ok "debt token (given) $DEBT"
fi

if [ -z "${ORACLE:-}" ]; then
    # A controllable feed. The acceptance scenario has to make a price stale, restore it, and
    # crash it to insolvency on demand — none of which Reflector will do on request.
    ORACLE=$(stellar contract deploy --wasm "$ORACLE_WASM" \
        --source "$KEY" --network "$NETWORK" -- --admin "$DEPLOYER" --decimals 14 2>/dev/null | tail -1)
    ok "mock oracle (14 dp) $ORACLE"
    note "testnet only — a real market points at Reflector"
else
    ok "oracle (given) $ORACLE"
fi

# ---------------------------------------------------------------------------
# The market
# ---------------------------------------------------------------------------

step "Deploying the market"

LENDER_ADDR="${LENDER_ADDR:-$DEPLOYER}"

# `deposit_decimals` and `debt_decimals` are passed as 99 on purpose. The constructor OVERWRITES
# both by calling `decimals()` on the token contracts, and never trusts what its caller says. A
# deploy that passed the true values would not demonstrate that.
CONFIG=$(jq -nc \
    --arg lender "$LENDER_ADDR" \
    --arg deposit_token "$COLLATERAL" \
    --arg debt_token "$DEBT" \
    --arg oracle "$ORACLE" \
    --arg feed "$DEPOSIT_FEED" \
    --argjson haircut "$HAIRCUT_BPS" \
    --argjson max_ltv "$MAX_LTV_BPS" \
    --argjson liq_thr "$LIQ_THRESHOLD_BPS" \
    --argjson full_liq "$FULL_LIQ_LTV_BPS" \
    --argjson bonus "$LIQ_BONUS_BPS" \
    --argjson close_factor "$CLOSE_FACTOR_BPS" \
    --argjson price_max_age "$PRICE_MAX_AGE" \
    --argjson rate "$RATE_BPS_YEAR" \
    --arg vault_cap "$VAULT_CAP" \
    --arg dust "$DUST_THRESHOLD" \
    '{
        lender: $lender,
        deposit_token: $deposit_token,
        deposit_decimals: 99,
        deposit_feed: { Other: $feed },
        debt_token: $debt_token,
        debt_decimals: 99,
        oracle: $oracle,
        haircut_bps: $haircut,
        max_ltv_bps: $max_ltv,
        liq_threshold_bps: $liq_thr,
        full_liq_ltv_bps: $full_liq,
        liq_bonus_bps: $bonus,
        close_factor_bps: $close_factor,
        price_max_age: $price_max_age,
        rate_bps_year: $rate,
        vault_cap: $vault_cap,
        dust_threshold: $dust
    }')

# The ledger the network is on RIGHT NOW, before anything is deployed.
#
# This is where an indexer must start replaying from. It is taken before rather than after because
# the constructor's `Initialized` event lands at this ledger or later, never earlier — so starting
# here cannot miss it, whereas a reading taken afterwards can be one ledger too late and skip the
# frozen config the whole projection is built on.
OPENED_LEDGER=$(curl -s -X POST "$RPC_URL" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getLatestLedger"}' | jq -r '.result.sequence // 0')
[ "$OPENED_LEDGER" != "0" ] || die "could not read the current ledger from $RPC_URL"
ok "current ledger $OPENED_LEDGER — the indexer will replay from here"

VAULT=$(stellar contract deploy --wasm "$VAULT_WASM" \
    --source "$KEY" --network "$NETWORK" \
    -- --config "$CONFIG" 2>/dev/null | tail -1)

[ -n "$VAULT" ] || die "the deploy produced no contract id"
ok "vault $VAULT"

# ---------------------------------------------------------------------------
# Post-deployment check
# ---------------------------------------------------------------------------

step "Checking what was actually stored"

STORED=$(view "$VAULT" get_config)
stored_dep=$(printf '%s' "$STORED" | jq -r '.deposit_decimals')
stored_debt=$(printf '%s' "$STORED" | jq -r '.debt_decimals')

# The check that matters: the constructor read the decimals from the tokens rather than
# believing the 99 it was handed. Everything downstream is scaled by these two numbers, and a
# market that trusted its caller here would be wrong by ten to the power of the difference.
[ "$stored_dep" = "8" ] || die "deposit_decimals is $stored_dep, expected 8 read from the token"
[ "$stored_debt" = "7" ] || die "debt_decimals is $stored_debt, expected 7 read from the token"
ok "decimals came from the tokens (8 / 7), not from the caller"

stored_max_ltv=$(printf '%s' "$STORED" | jq -r '.max_ltv_bps')
[ "$stored_max_ltv" = "$MAX_LTV_BPS" ] || die "max_ltv_bps is $stored_max_ltv, expected $MAX_LTV_BPS"
ok "risk parameters frozen as configured"

# ---------------------------------------------------------------------------

mkdir -p "$DEPLOY_DIR"
cat > "$DEPLOY_DIR/env" <<EOF
# Written by scripts/deploy.sh on $(date -u '+%Y-%m-%d %H:%M:%SZ').
# Addresses only. Identities live in the Stellar CLI keystore; no secret is written here.
NETWORK=$NETWORK
VAULT=$VAULT
COLLATERAL=$COLLATERAL
DEBT=$DEBT
ORACLE=$ORACLE
DEPLOYER=$DEPLOYER
LENDER_ADDR=$LENDER_ADDR
DEPOSIT_FEED=$DEPOSIT_FEED
PRICE_MAX_AGE=$PRICE_MAX_AGE
# Where the indexer replays from. Read before the deploy, so it cannot be later than the
# constructor's own event.
OPENED_LEDGER=$OPENED_LEDGER
EOF

step "Done"
say "  $DEPLOY_DIR/env written"
say "  next: make demo"
