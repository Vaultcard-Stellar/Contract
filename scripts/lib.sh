#!/usr/bin/env bash
# Shared plumbing for deploy.sh and demo.sh.
#
# The one function that matters is `call`. Everything else is bookkeeping.

set -euo pipefail

NETWORK="${NETWORK:-testnet}"

# The RPC the scripts read from directly, for the two things the CLI does not expose: the current
# ledger, and a transaction's ledger. Defaulted per network rather than required, so an ordinary
# testnet run needs no configuration.
if [ -z "${RPC_URL:-}" ]; then
    case "$NETWORK" in
        testnet) RPC_URL="https://soroban-testnet.stellar.org" ;;
        futurenet) RPC_URL="https://rpc-futurenet.stellar.org" ;;
        *) RPC_URL="" ;;
    esac
fi
DEPLOY_DIR="${DEPLOY_DIR:-.deploy}"
EVIDENCE="$DEPLOY_DIR/evidence.md"

# The identity that pays for the deploy when the caller names none. Defaulted rather than
# required: the first thing a reviewer does is run `make demo`, and stopping them to generate a
# throwaway testnet key teaches nothing about the contract. Named with the same `vc_` prefix as
# the cast so a reviewer can see at a glance which keys this repository created.
DEFAULT_KEY="${DEFAULT_KEY:-vc_deployer}"

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

if [ -t 1 ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m'); OFF=$(printf '\033[0m')
else
    BOLD=""; DIM=""; RED=""; GREEN=""; OFF=""
fi

say()  { printf '%s\n' "$*"; }
step() { printf '\n%s%s%s\n' "$BOLD" "$*" "$OFF"; }
ok()   { printf '  %s✓%s %s\n' "$GREEN" "$OFF" "$*"; }
die()  { printf '\n%s✗ %s%s\n' "$RED" "$*" "$OFF" >&2; exit 1; }
note() { printf '  %s%s%s\n' "$DIM" "$*" "$OFF"; }

need() {
    command -v "$1" >/dev/null 2>&1 \
        || die "$1 is not installed. $2"
}

# Create and fund an identity if it is not already in the CLI keystore, and top it up if it is.
#
# Funding needs a friendbot, so on a network without one a missing identity is a hard error
# rather than a key that exists and can pay for nothing — which surfaces later as a
# `txInsufficientBalance` that says nothing about the real problem.
#
# The top-up on the already-exists path is not redundant: an identity left over from a previous
# run can have been drained, and friendbot is idempotent and cheap.
ensure_identity() {
    local id="$1"

    if stellar keys address "$id" >/dev/null 2>&1; then
        case "$NETWORK" in
            testnet|futurenet) stellar keys fund "$id" --network "$NETWORK" >/dev/null 2>&1 || true ;;
        esac
        return 0
    fi

    case "$NETWORK" in
        testnet|futurenet) ;;
        *) die "'$id' is not a known identity, and '$NETWORK' has no friendbot to fund a new one.
Create and fund it yourself, then re-run:  stellar keys generate $id --network $NETWORK" ;;
    esac

    stellar keys generate --fund "$id" --network "$NETWORK" >/dev/null 2>&1 \
        || die "could not create and fund '$id' on $NETWORK. Friendbot may be rate-limited or
down; wait a minute and run it again."
}

preflight() {
    need stellar "Install the Stellar CLI: https://developers.stellar.org/docs/tools/cli"
    need jq "Install jq — the script parses contract output with it."

    # `make demo` passes KEY through unconditionally, so an unset KEY arrives here as the empty
    # string rather than as unset. Both mean the same thing and both get the default.
    if [ -z "${KEY:-}" ]; then
        KEY="$DEFAULT_KEY"
        case "$NETWORK" in
            testnet|futurenet) note "KEY not set — using '$KEY', created and funded on $NETWORK if it does not exist yet" ;;
            *) note "KEY not set — using '$KEY'" ;;
        esac
    fi
    # deploy.sh is a child process when demo.sh runs it, and it needs the same identity.
    export KEY

    ensure_identity "$KEY"
    mkdir -p "$DEPLOY_DIR"
}

# ---------------------------------------------------------------------------
# The step recorder
# ---------------------------------------------------------------------------

STEP_NO=0
declare -a EVIDENCE_ROWS=()

# Invoke, and assert what the chain says.
#
#   call ""   <label> -- stellar contract invoke ...     expect success
#   call 33   <label> -- stellar contract invoke ...     expect exactly Error(Contract, #33)
#
# THE GREP IS THE WHOLE POINT. Without it a step that fails for the WRONG reason still passes,
# and the evidence package becomes a set of transactions nobody verified — which is worse than
# no evidence, because it looks like evidence.
#
# A rejection that never reaches the ledger is recorded as `simulation`, never given a fake
# hash. The CLI simulates before submitting, so a call the contract will refuse fails locally
# and produces no transaction at all. Inventing a hash for it would be fabricating the one
# artefact a reviewer is expected to open.
call() {
    local expect="$1"; shift
    local label="$1"; shift
    [ "$1" = "--" ] && shift

    STEP_NO=$((STEP_NO + 1))
    printf '  %2d. %-58s' "$STEP_NO" "$label"

    local out status
    set +e
    out=$("$@" 2>&1)
    status=$?
    set -e

    if [ $status -eq 0 ]; then
        if [ -n "$expect" ]; then
            printf '%sFAIL%s\n' "$RED" "$OFF"
            die "step $STEP_NO expected error $expect and the call SUCCEEDED.
A gate that should have refused did not. Output:
$out"
        fi
        # Parsed from the CLI's own label, not by scanning for 64 hex characters: a `draw_id`
        # argument is also 64 hex characters, and a loose match picked THAT out of the echoed
        # command line and recorded it as the transaction hash. An evidence file full of draw ids
        # would look entirely plausible and open to nothing on the explorer.
        local hash
        hash=$(printf '%s' "$out" | sed -n 's/.*Signing transaction: \([0-9a-f]\{64\}\).*/\1/p' | tail -1)
        [ -n "$hash" ] || hash=$(printf '%s' "$out" | sed -n 's#.*explorer/testnet/tx/\([0-9a-f]\{64\}\).*#\1#p' | tail -1)
        if [ -n "$hash" ]; then
            printf '%s✓%s %s%s%s\n' "$GREEN" "$OFF" "$DIM" "${hash:0:16}…" "$OFF"
            EVIDENCE_ROWS+=("$STEP_NO|$label|—|\`$hash\`")
        else
            printf '%s✓%s\n' "$GREEN" "$OFF"
            EVIDENCE_ROWS+=("$STEP_NO|$label|—|read-only")
        fi
        LAST_OUTPUT="$out"
        return 0
    fi

    if [ -z "$expect" ]; then
        printf '%sFAIL%s\n' "$RED" "$OFF"
        die "step $STEP_NO expected success and failed:
$out"
    fi

    # The assertion. `Error(Contract, #N)` is how the host reports a contract error, and matching
    # the exact number is what separates "it refused" from "it refused for the reason we claim".
    if printf '%s' "$out" | grep -q "Error(Contract, #$expect)"; then
        printf '%s✓%s %sblocked, code %s%s\n' "$GREEN" "$OFF" "$DIM" "$expect" "$OFF"
        EVIDENCE_ROWS+=("$STEP_NO|$label|**$expect**|simulation")
        return 0
    fi

    printf '%sFAIL%s\n' "$RED" "$OFF"
    die "step $STEP_NO expected error $expect but got something else.
A step that fails for the wrong reason is not evidence of anything. Output:
$out"
}

# ---------------------------------------------------------------------------
# Convenience wrappers
# ---------------------------------------------------------------------------

# Fire and forget, quietly. The CLI narrates every submission on stderr, which is useful when a
# human runs it and pure noise inside a scripted scenario — it buries the step lines the reader
# is actually following.
invoke() {
    local id="$1"; shift
    local as="${AS:-$KEY}"
    stellar contract invoke --id "$id" --source "$as" --network "$NETWORK" -- "$@" 2>/dev/null
}

# A read-only call whose output is JSON. Not routed through `call`, because a view emits no
# transaction and recording it as a step would pad the evidence with things nobody can open.
view() {
    local id="$1"; shift
    stellar contract invoke --id "$id" --source "$KEY" --network "$NETWORK" -- "$@" 2>/dev/null
}

address_of() { stellar keys address "$1"; }

# ---------------------------------------------------------------------------
# Evidence
# ---------------------------------------------------------------------------

write_evidence() {
    local title="$1"
    {
        echo "# $title"
        echo
        echo "Generated by \`make demo\` on $(date -u '+%Y-%m-%d %H:%M:%SZ')."
        echo
        echo "**How to check this.** Open each transaction hash on Stellar Expert and confirm it"
        echo "succeeded. Rows marked \`simulation\` never reached the ledger: the CLI simulates"
        echo "before submitting, so a call the contract refuses fails locally and produces no"
        echo "transaction. They are labelled rather than given a hash, because a fabricated hash is"
        echo "the one thing a reviewer would have no way to catch."
        echo
        echo "| | |"
        echo "|---|---|"
        echo "| Network | $NETWORK |"
        for var in VAULT ORACLE COLLATERAL DEBT; do
            [ -n "${!var:-}" ] && echo "| $var | \`${!var}\` |"
        done
        for var in LENDER OWNER OPERATOR SETTLEMENT LIQUIDATOR; do
            [ -n "${!var:-}" ] && echo "| $var | \`${!var}\` |"
        done
        echo
        echo "| # | Step | Error code | Transaction |"
        echo "|---|---|---|---|"
        printf '%s\n' "${EVIDENCE_ROWS[@]}" | awk -F'|' '{printf "| %s | %s | %s | %s |\n", $1, $2, $3, $4}'
        echo
    } > "$EVIDENCE"
}
