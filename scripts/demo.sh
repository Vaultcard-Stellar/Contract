#!/usr/bin/env bash
#
# The acceptance scenario. One command, on a clean machine, end to end.
#
# # What this is for
#
# The SOW puts it plainly: *"Clone the repository on a clean machine and run the command. If it
# does not complete, Deliverable 3 is not complete regardless of the other evidence."*
#
# So this deploys a market, drives twenty-six steps through it, and writes `.deploy/evidence.md`
# with a transaction hash for every step that reached the ledger.
#
# # The property that makes it evidence rather than theatre
#
# **Every rejection step greps for its exact error code and fails the script on a mismatch.**
# Without that, a step that fails for the *wrong* reason still passes — the script goes green,
# the evidence file fills up, and nobody has verified anything. That is worse than no evidence,
# because it looks like evidence. See `call` in scripts/lib.sh.
#
# Ten rejections across ten distinct codes: 46, 38, 20, 36, 34, 33, 35, 40, 53, 30.
#
# # Rejections do not get transaction hashes
#
# The CLI simulates before submitting, so a call the contract will refuse fails locally and never
# reaches the ledger. Those rows are labelled `simulation`. Giving them a fabricated hash would
# be forging the one artefact a reviewer is expected to open, and it is the only thing in the
# package they could not catch.
#
# Usage:
#   ./scripts/demo.sh                 # creates and funds vc_deployer on testnet if needed
#   KEY=deployer ./scripts/demo.sh    # pay with an identity you already hold

set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib.sh

preflight

# ---------------------------------------------------------------------------
# Cast
# ---------------------------------------------------------------------------

step "Accounts"

# Six roles, six identities. Sharing one would let a gate pass because the caller happened to be
# authorised for something else, which is exactly the confusion the roles exist to prevent.
# `ensure_identity` creates what is missing and tops up what is not — friendbot is not always
# instant, and an unfunded source produces an error that looks nothing like the real problem.
for role in lender owner operator operator2 settlement liquidator; do
    ensure_identity "vc_$role"
done

LENDER=$(address_of vc_lender)
OWNER=$(address_of vc_owner)
OPERATOR=$(address_of vc_operator)
# A second provider, for the one gate that cannot be shown with a single operator: a line the
# reserve cannot back. Proposing it against the SAME operator would be refused with 38 before
# the reserve is ever consulted.
OPERATOR2=$(address_of vc_operator2)
SETTLEMENT=$(address_of vc_settlement)
LIQUIDATOR=$(address_of vc_liquidator)
ok "lender · owner · operator · operator2 · settlement · liquidator"

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

step "Deploying a fresh market"
LENDER_ADDR="$LENDER" ./scripts/deploy.sh >/dev/null || die "deploy.sh failed; run it directly to see why"
# shellcheck disable=SC1090
. "$DEPLOY_DIR/env"
ok "vault $VAULT"

# ---------------------------------------------------------------------------
# Scale
# ---------------------------------------------------------------------------

BTC=100000000 # 1 unit of the 8-decimal collateral
USD=10000000  # 1 unit of the 7-decimal debt token

# Prices at the oracle's 14 decimals.
#
# Not chosen by eye. With 0.5 BTC of collateral, a 12% haircut and $12,000 of debt:
#
#   $60,000  eligible $26,400  LTV 45.5%   healthy, and inside the 50% ceiling
#   $35,000  eligible $15,400  LTV 71.4%   past the 70% threshold: liquidatable, still solvent
#   $10,000  market    $5,000              market value below the debt: INSOLVENT
#
# The middle figure is computed against the debt as it stands AFTER the partial repayment two
# steps earlier — $11,000, not $12,000. An earlier draft used $38,000, which is right for $12,000
# of debt and leaves the position at 65.8% once $1,000 has been repaid: below the threshold, and
# the liquidation step failed. `assert_liquidatable` is what turned that into one clear sentence
# instead of a confusing error three calls later.
#
# The middle one has to be liquidatable *and* solvent, or the ordinary route never runs and the
# insolvency branch is never reached separately. The script re-derives both from the chain before
# it acts, because these numbers stop being right the moment a parameter changes.
P_HEALTHY=6000000000000000000   # $60,000
P_LIQ=3500000000000000000       # $35,000
P_INSOLVENT=1000000000000000000 # $10,000

# Deposited gross, then part of it taken straight back out, leaving 0.5 BTC. The withdrawal is
# not decoration: it is the property the whole design exists for — collateral is never lent on,
# so it stays withdrawable — and the SOW asks for a transaction hash proving it on chain.
# Every price point above is derived from what is LEFT, so these three must keep agreeing.
DEPOSIT_GROSS=$((BTC * 6 / 10)) # 0.6 BTC in
WITHDRAWAL=$((BTC / 10))        # 0.1 BTC straight back out
DEPOSIT=$((BTC / 2))            # 0.5 BTC left behind
CAP=$((20000 * USD))
PER_TX=$((13000 * USD))

set_price() {
    invoke "$ORACLE" set_price --asset "{\"Other\":\"$DEPOSIT_FEED\"}" --price "$1" --timestamp 0 >/dev/null 2>&1
}

# Confirm the position really is in the state the next step assumes, by asking the CONTRACT
# rather than trusting the arithmetic in the comment above.
#
# The prices here were derived from a haircut, a threshold and a deposit — all three of which can
# be changed by an environment variable in deploy.sh. When one of them moves, a hand-computed
# price silently stops triggering what it was chosen to trigger, and the step that follows fails
# with a confusing error instead of a clear one. `quote_liquidation` exists precisely so nobody
# has to submit a failing transaction to learn this.
assert_liquidatable() {
    local q liquidatable reason
    q=$(view "$VAULT" quote_liquidation --owner "$OWNER")
    liquidatable=$(printf '%s' "$q" | jq -r '.liquidatable')
    reason=$(printf '%s' "$q" | jq -r '.blocked_reason // "none"')

    # BOTH halves. `liquidatable` answers only the health question, and `liquidate` checks the
    # grace window BEFORE it checks health — so a position can be liquidatable and still refused
    # with 53. An earlier version of this assertion read only the first field, passed, and let
    # the real call fail one line later with the error it was supposed to pre-empt.
    [ "$liquidatable" = "true" ] || die "the position is not liquidatable at this price \
(blocked_reason $reason). The price points here are derived from the haircut, the liquidation \
threshold and the deposit; if any of those changed, they need recomputing.
quote: $q"
    [ "$reason" = "none" ] || die "the position is liquidatable but the call would still be \
refused with code $reason. 53 means the grace window has not elapsed — the wait is real, and \
SKIP_GRACE_WAIT does not shorten what the contract enforces.
quote: $q"
    ok "the contract agrees: liquidatable, and nothing else blocks the call"
}

assert_insolvent() {
    local pos market debt
    pos=$(view "$VAULT" get_position --owner "$OWNER")
    market=$(printf '%s' "$pos" | jq -r '.market_value' | tr -d '"')
    debt=$(printf '%s' "$pos" | jq -r '.debt_value' | tr -d '"')
    # Insolvency triggers on MARKET value, not eligible value. Haircut is a buffer for lending
    # decisions, not for recovery: triggering on eligible would write off principal while up to
    # haircut_bps of collateral remained, possibly enough to repay in full.
    awk -v m="$market" -v d="$debt" 'BEGIN { exit !(m < d) }' \
        || die "market value $market is not below the debt $debt, so this is not insolvent yet"
    ok "market value $market is below the debt $debt — genuinely insolvent"
}

step "Funding the cast"
invoke "$DEBT" mint --to "$LENDER" --amount $((1000000 * USD)) >/dev/null
invoke "$DEBT" mint --to "$LIQUIDATOR" --amount $((1000000 * USD)) >/dev/null
invoke "$DEBT" mint --to "$OWNER" --amount $((100000 * USD)) >/dev/null
invoke "$COLLATERAL" mint --to "$OWNER" --amount $((10 * BTC)) >/dev/null
set_price "$P_HEALTHY"
ok "balances minted, BTC at \$60,000"

# ---------------------------------------------------------------------------
# The scenario
# ---------------------------------------------------------------------------

step "Acceptance scenario"

#  1  deploy — already done above, recorded so the numbering matches docs/10.
STEP_NO=0
# The deploy is recorded by CONTRACT ID rather than transaction hash, and labelled as such. It
# is the one row a reviewer would otherwise paste into the transaction lookup and get nothing
# from — a column that means "transaction" everywhere else must not silently mean something else
# here.
EVIDENCE_ROWS+=("1|Deploy tokens, oracle and market — constructor inline, one transaction|—|contract \`$VAULT\`")
STEP_NO=1
printf '  %2d. %-58s%s✓%s\n' 1 "Deploy market (constructor in the deploy transaction)" "$GREEN" "$OFF"

#  2  fund_reserve
AS=vc_lender call "" "Lender funds the reserve with 500,000" -- \
    stellar contract invoke --id "$VAULT" --source vc_lender --network "$NETWORK" -- \
    fund_reserve --amount $((500000 * USD))

#  3  deposit
AS=vc_owner call "" "Customer deposits 0.6 BTC of collateral" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    deposit --owner "$OWNER" --amount "$DEPOSIT_GROSS"

#  4  withdraw — with no debt, this reads no price at all
#
# The point being demonstrated is not that a number went down. It is that withdrawal depends on
# nothing but this customer's own debt: no pool utilisation, no other borrower, and — while the
# debt is zero — not even the oracle. A pool design cannot make that promise.
AS=vc_owner call "" "Customer withdraws 0.1 BTC — no debt, so no price is read" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    withdraw --owner "$OWNER" --amount "$WITHDRAWAL"

HELD=$(view "$VAULT" get_position --owner "$OWNER" | jq -r '.deposit' | tr -d '"')
[ "${HELD:-}" = "$DEPOSIT" ] \
    || die "the position holds $HELD after the withdrawal, expected $DEPOSIT — every price point \
below is derived from 0.5 BTC and would no longer trigger what it was chosen to trigger"
ok "0.5 BTC left in the position, which is what the price points below assume"

EXPIRY=$(( $(date +%s) + 30 * 86400 ))

#  5  grant above the protocol ceiling → 46
call 46 "Grant with an LTV ceiling above the market's" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    grant_authority --owner "$OWNER" --operator "$OPERATOR" --settlement "$SETTLEMENT" \
    --cap "$CAP" --per_tx_limit "$PER_TX" --expiry "$EXPIRY" --max_ltv_bps 6000

#  6  grant, valid
call "" "Grant spending authority: 20,000 cap, 13,000 per transaction" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    grant_authority --owner "$OWNER" --operator "$OPERATOR" --settlement "$SETTLEMENT" \
    --cap "$CAP" --per_tx_limit "$PER_TX" --expiry "$EXPIRY" --max_ltv_bps 5000

#  7  grant to the SAME operator again → 38
#
# Per operator, not per customer. A grant to a different provider is allowed — that is the whole
# point of keying by `(owner, operator)` — but overwriting this one would reset `drawn` to zero
# and hand the same operator a fresh cap.
call 38 "Grant a second authority to the same operator" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    grant_authority --owner "$OWNER" --operator "$OPERATOR" --settlement "$SETTLEMENT" \
    --cap $((10000 * USD)) --per_tx_limit $((1000 * USD)) --expiry "$EXPIRY" --max_ltv_bps 5000

#  8  the operator takes the line up — THIS is where capital is committed
#
# Signed by the operator, not the customer, and the split is the whole reason the reserve gate
# below can fire at all. A proposal commits nothing, so it cannot be refused for a thin reserve;
# acceptance moves `reserve.free` into `reserve.committed`, and that is the moment the capital
# behind the line has to actually exist.
AS=vc_operator call "" "Operator accepts the line — 20,000 moves free → committed" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    accept_grant --owner "$OWNER" --operator "$OPERATOR"

COMMITTED=$(view "$VAULT" get_reserve | jq -r '.committed' | tr -d '"')
[ "$COMMITTED" = "$CAP" ] || die "committed is $COMMITTED after acceptance, expected $CAP"
ok "reserve committed $CAP — the line is backed before a single unit is drawn"

#  9  a line larger than the free reserve can back — proposed, then refused on acceptance → 20
#
# This is the guarantee in the direction people forget. A card authorisation must never fail
# because somebody else borrowed first, and the only way to promise that is to refuse the LINE
# at the moment it becomes usable unless the uncommitted reserve already covers all of it.
#
# 600,000 against a 500,000 reserve of which 20,000 is already committed. A second operator is
# required: the same one would be refused with 38 long before the reserve is consulted.
call "" "Customer proposes a 600,000 line to a second provider" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    grant_authority --owner "$OWNER" --operator "$OPERATOR2" --settlement "$SETTLEMENT" \
    --cap $((600000 * USD)) --per_tx_limit $((1000 * USD)) --expiry "$EXPIRY" --max_ltv_bps 5000

call 20 "Second provider accepts — the free reserve cannot back it" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator2 --network "$NETWORK" -- \
    accept_grant --owner "$OWNER" --operator "$OPERATOR2"

# The proposal is left standing on purpose. A pending grant commits nothing — `grant::release`
# returns 0 for one — so it disturbs neither the reserve arithmetic below nor the insolvency
# step, and leaving it on chain shows a reviewer what a refused line looks like afterwards.

DRAW1=0000000000000000000000000000000000000000000000000000000000000001

# 11  draw, valid — funds reach the PINNED settlement address, not the operator
call "" "Operator draws 5,000 to the pinned settlement address" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" --draw_id "$DRAW1" --amount $((5000 * USD))

# The negative assertion. Checking only that the settlement address received something passes
# even when the operator was paid too — and the operator being payable is the entire threat the
# pinned address exists to remove.
OP_BAL=$(view "$COLLATERAL" balance --id "$OPERATOR" | tr -d '"')
[ "${OP_BAL:-0}" = "0" ] || die "the operator holds collateral ($OP_BAL); the settlement pin leaked"
DEBT_AT_SETTLEMENT=$(view "$DEBT" balance --id "$SETTLEMENT" | tr -d '"')
[ "$DEBT_AT_SETTLEMENT" = "$((5000 * USD))" ] \
    || die "settlement holds $DEBT_AT_SETTLEMENT, expected $((5000 * USD))"
OP_DEBT=$(view "$DEBT" balance --id "$OPERATOR" | tr -d '"')
[ "${OP_DEBT:-0}" = "0" ] || die "the operator was paid $OP_DEBT; draw must only pay the pin"
ok "funds reached the pinned address and the operator holds nothing"

# 12  same draw_id → 36
call 36 "Resubmit the same draw_id" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" --draw_id "$DRAW1" --amount $((5000 * USD))

# 13  above per-tx → 34
#
# 14,000 against a 13,000 per-transaction limit, with 5,000 already drawn against a 20,000 cap.
# The cap gate runs FIRST and 5,000 + 14,000 = 19,000 passes it, so 34 is genuinely what fires.
# Ask for more and 33 would pre-empt it, and the step would pass while testing the wrong gate.
call 34 "Draw above the per-transaction limit" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000002 \
    --amount $((14000 * USD))

# 14  beyond the cap → 33
call 33 "Draw beyond the cumulative cap" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000003 \
    --amount $((16000 * USD))

# 15  past the grant's LTV ceiling → 35
#
# 13,000 on top of the 5,000 already drawn. Both static gates pass — 18,000 is inside the 20,000
# cap and 13,000 is exactly the per-transaction limit — so the only thing left to refuse it is
# the price, which is the entire point of the step. 18,000 against 26,400 of eligible value is
# 68.2%, past the 50% ceiling the customer set at grant time.
#
# 35 rather than 11: the grant's ceiling is checked before the market's, so the more specific
# code wins and the backend can tell "this line is maxed out" from "this vault is".
call 35 "Draw past the grant's loan-to-value ceiling" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000007 \
    --amount $((13000 * USD))

# Levering the position up so the liquidation steps have something to act on. Not a numbered
# acceptance step: it demonstrates nothing on its own, it is arithmetic the scenario needs.
call "" "Draw a further 7,000 — total debt 12,000 against 26,400 of eligible value" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000006 \
    --amount $((7000 * USD))

# 17  stale price → 40
step "Making the feed stale"
STALE_TS=$(( $(date +%s) - PRICE_MAX_AGE - 120 ))
invoke "$ORACLE" set_price --asset "{\"Other\":\"$DEPOSIT_FEED\"}" --price "$P_HEALTHY" --timestamp "$STALE_TS" >/dev/null 2>&1
ok "price timestamped $((PRICE_MAX_AGE + 120))s ago, past price_max_age of ${PRICE_MAX_AGE}s"

call 40 "Draw against a stale price" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000004 \
    --amount $((1000 * USD))

# 18–20  record the outage, record the recovery, then liquidate inside the window → 53
call "" "Record the oracle outage (permissionless)" -- \
    stellar contract invoke --id "$VAULT" --source "$KEY" --network "$NETWORK" -- report_oracle_state

set_price "$P_LIQ"
call "" "Record the recovery, opening the grace window" -- \
    stellar contract invoke --id "$VAULT" --source "$KEY" --network "$NETWORK" -- report_oracle_state

call 53 "Liquidate inside the grace window" -- \
    stellar contract invoke --id "$VAULT" --source vc_liquidator --network "$NETWORK" -- \
    liquidate --owner "$OWNER" --liquidator "$LIQUIDATOR" --repay_amount $((1000 * USD)) --min_deposit_out 0

# 21  partial repay, naming the line that reopens
#
# The fourth argument is `Option<Address>`: the grant whose headroom comes back. A customer may
# hold several — one per provider — so the contract does not choose, the payer says. Passing
# `null` here would repay the debt and reopen nothing, which is the third-party route.
call "" "Customer repays 1,000 — interest first, then principal" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    repay --owner "$OWNER" --from "$OWNER" --amount $((1000 * USD)) \
    --operator "\"$OPERATOR\""

# 22  third-party liquidation
step "Waiting out the grace window (30 minutes on chain)"
note "the contract blocks liquidation for LIQUIDATION_GRACE after a feed recovers, so the owner"
note "has a chance to react to a price gap they could not have seen coming"
# LIQUIDATION_GRACE is 1800 seconds and it is a code constant, not configuration — nobody can
# set it to zero after deployment. On testnet the ledger clock is wall clock, so the wait is real.
#
# SKIP_GRACE_WAIT exists for iterating on the script. Using it produces a run that never proves
# the grace window elapses, so the evidence file from such a run is not acceptance evidence.
if [ -n "${SKIP_GRACE_WAIT:-}" ]; then
    note "SKIP_GRACE_WAIT set — this run does NOT demonstrate the grace window and its evidence"
    note "file is not acceptance evidence"
fi
GRACE_UNTIL=$(( $(date +%s) + ${SKIP_GRACE_WAIT:+0}${SKIP_GRACE_WAIT:-1800} ))
while [ "$(date +%s)" -lt "$GRACE_UNTIL" ]; do
    left=$(( GRACE_UNTIL - $(date +%s) ))
    printf '\r  %s%d:%02d remaining%s ' "$DIM" $((left / 60)) $((left % 60)) "$OFF"
    sleep 15
done
printf '\r%*s\r' 40 ''
ok "grace window elapsed"

set_price "$P_LIQ"
assert_liquidatable
call "" "Third party liquidates and earns the bonus" -- \
    stellar contract invoke --id "$VAULT" --source vc_liquidator --network "$NETWORK" -- \
    liquidate --owner "$OWNER" --liquidator "$LIQUIDATOR" --repay_amount $((500 * USD)) --min_deposit_out 0

# 23  lender liquidation — nets, moves no debt token
DEBT_BEFORE=$(view "$DEBT" balance --id "$VAULT" | tr -d '"')
call "" "Lender liquidates by netting — no debt token moves" -- \
    stellar contract invoke --id "$VAULT" --source vc_lender --network "$NETWORK" -- \
    lender_liquidate --owner "$OWNER" --repay_amount $((500 * USD))
DEBT_AFTER=$(view "$DEBT" balance --id "$VAULT" | tr -d '"')
[ "$DEBT_BEFORE" = "$DEBT_AFTER" ] \
    || die "the lender route moved debt token ($DEBT_BEFORE → $DEBT_AFTER); it must net"
ok "the vault's debt-token balance is unchanged — the lender took collateral"

# 24  revoke — BEFORE the insolvency step, and that ordering is not cosmetic
#
# docs/10 puts the insolvency liquidation first and revocation after it. That sequence cannot
# run: the insolvency branch calls `grant::release` and DELETES the grant, so `revoke_authority`
# afterwards returns 30 GrantNotFound and the step fails. Found by running it.
#
# Revoking first also leaves the more interesting shape on chain — a position that goes bad after
# its authority is gone, which is what a customer who reacted to a margin call and still got
# liquidated actually looks like.
call "" "Customer revokes the spending authority" -- \
    stellar contract invoke --id "$VAULT" --source vc_owner --network "$NETWORK" -- \
    revoke_authority --owner "$OWNER" --operator "$OPERATOR"

# 25  draw after revoke → 30, NOT the retired 32
call 30 "Draw after revocation (code 30, not the retired 32)" -- \
    stellar contract invoke --id "$VAULT" --source vc_operator --network "$NETWORK" -- \
    draw --owner "$OWNER" --operator "$OPERATOR" \
    --draw_id 0000000000000000000000000000000000000000000000000000000000000005 \
    --amount $((100 * USD))

# 26  insolvency — must not revert
#
# The grant is already gone by this point, so the branch skips its `GrantRevoked` emission and
# goes straight to the write-off. Both paths through `wipe_out` are exercised by the contract's
# own suite; what matters here is the one property no unit test can show — that it returns rather
# than reverting, on a real ledger, leaving the position closeable.
step "Crashing the price below the debt"
set_price "$P_INSOLVENT"
ok "BTC at \$10,000 — market value now under the debt, so the position is insolvent"

assert_insolvent
call "" "Liquidate an insolvent position — the branch that must not revert" -- \
    stellar contract invoke --id "$VAULT" --source vc_liquidator --network "$NETWORK" -- \
    liquidate --owner "$OWNER" --liquidator "$LIQUIDATOR" --repay_amount $((20000 * USD)) --min_deposit_out 0

RESERVE=$(view "$VAULT" get_reserve)
BAD_DEBT=$(printf '%s' "$RESERVE" | jq -r '.bad_debt' | tr -d '"')
[ "$BAD_DEBT" != "0" ] || die "bad_debt is zero; the insolvency branch did not run"
ok "bad_debt realised: $BAD_DEBT — and the call returned rather than reverting"

# final state
step "Final state"
view "$VAULT" get_position --owner "$OWNER" > "$DEPLOY_DIR/position.json"
printf '%s' "$RESERVE" > "$DEPLOY_DIR/reserve.json"
view "$VAULT" get_config > "$DEPLOY_DIR/config.json"
ok "position.json · reserve.json · config.json"

# ---------------------------------------------------------------------------

write_evidence "Acceptance evidence"

CODES=$(printf '%s\n' "${EVIDENCE_ROWS[@]}" | awk -F'|' '$3 ~ /[0-9]/ {gsub(/\*/,"",$3); print $3}' | sort -un)
COUNT=$(printf '%s\n' "$CODES" | grep -c . || true)

step "Done"
say "  $(printf '%s\n' "${EVIDENCE_ROWS[@]}" | wc -l | tr -d ' ') steps"
say "  $COUNT distinct error codes: $(printf '%s' "$CODES" | tr '\n' ' ')"
say "  evidence written to $EVIDENCE"

# M1 asks for four rejections with four distinct codes. The scenario produces ten, and this
# refuses to call the run a success if it ever produces fewer — a demo that quietly stops
# exercising a gate is how a regression ships.
[ "$COUNT" -ge 10 ] || die "expected at least 10 distinct error codes, got $COUNT"
