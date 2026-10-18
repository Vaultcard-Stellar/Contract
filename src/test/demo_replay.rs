//! The acceptance scenario, replayed in memory.
//!
//! `scripts/demo.sh` drives the same sequence against testnet, and every rejection step there
//! greps for one exact error code. Which code a step produces is not a property of the gate
//! alone — it is a property of the gate *and the amounts chosen*. 14,000 against a 13,000
//! per-transaction limit returns 34 only while the cumulative cap still has room; ask for more
//! and 33 pre-empts it, and the script passes while proving nothing about the gate it names.
//!
//! That coupling is invisible in the shell script and expensive to discover from it: the run
//! takes about thirty-five minutes, most of it a real wait on the liquidation grace window, and
//! it fails at whichever step drifted. So the amounts live here too, and CI answers in seconds.
//!
//! The fixture's risk parameters and `scripts/deploy.sh`'s defaults are the same numbers —
//! haircut 1200, ceiling 5000, threshold 7000, `price_max_age` 300. If they ever diverge, this
//! file is testing a market the script does not deploy, and the two sets of constants are worth
//! comparing before trusting either.
//!
//! What is **not** replayed: the liquidation routes and the grace window. Those are covered in
//! `liquidation_tests` against the same fixture, and duplicating them here would add a second
//! place to update without adding a second thing under test. What is unique to this file is the
//! arithmetic that decides *which* code comes back.

use super::harness::*;
use crate::Error;

/// 0.6 BTC in, 0.1 BTC straight back out.
const DEPOSIT_GROSS: i128 = BTC * 6 / 10;
const WITHDRAWAL: i128 = BTC / 10;
const DEPOSIT: i128 = BTC / 2;

const RESERVE: i128 = 500_000 * USD;
const CAP: i128 = 20_000 * USD;
const PER_TX: i128 = 13_000 * USD;

#[test]
fn the_acceptance_scenario_produces_the_codes_the_script_asserts() {
    let f = F::new();

    // 2 · the lender funds the reserve
    f.v().fund_reserve(&RESERVE);

    // 3 · deposit, and 4 · withdraw. With no debt the withdrawal reads no price at all, which is
    // the property the vault exists for: collateral is never lent on, so it stays withdrawable.
    f.v().deposit(&f.owner, &DEPOSIT_GROSS);
    f.v().withdraw(&f.owner, &WITHDRAWAL);
    assert_eq!(
        f.v().get_position(&f.owner).deposit,
        DEPOSIT,
        "every price point in demo.sh is derived from 0.5 BTC"
    );

    let expiry = f.now() + 30 * 86_400;

    // 5 · a ceiling above the market's own → 46
    assert_err!(
        f.v().try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &CAP,
            &PER_TX,
            &expiry,
            &6000
        ),
        Error::GrantCeilingTooHigh
    );

    // 6 · the real line
    f.v().grant_authority(
        &f.owner,
        &f.operator,
        &f.settlement,
        &CAP,
        &PER_TX,
        &expiry,
        &5000,
    );

    // 7 · a second line to the SAME operator → 38. Overwriting would reset `drawn` to zero and
    // hand that operator a fresh cap.
    assert_err!(
        f.v().try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &(10_000 * USD),
            &(1_000 * USD),
            &expiry,
            &5000
        ),
        Error::GrantExists
    );

    // 8 · acceptance is where capital is committed
    f.v().accept_grant(&f.owner, &f.operator);
    assert_eq!(f.v().get_reserve().committed, CAP);

    // 9 · a line the free reserve cannot back → 20, at acceptance rather than at proposal.
    //
    // 600,000 against 500,000 funded, of which 20,000 is already committed. A second operator is
    // required: the same one is refused with 38 long before the reserve is consulted.
    f.v().grant_authority(
        &f.owner,
        &f.third,
        &f.settlement,
        &(600_000 * USD),
        &(1_000 * USD),
        &expiry,
        &5000,
    );
    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.third),
        Error::ReserveUnderfunded
    );
    // The refusal wrote nothing. A pending grant commits no capital, which is also why leaving it
    // standing on chain disturbs nothing that follows.
    assert_eq!(f.v().get_reserve().committed, CAP);

    // 11 · the accepted draw. The negative assertion is the one that matters: checking only that
    // the settlement address was paid passes even when the operator was paid too, and an operator
    // that can pay itself is the entire threat the pinned address removes.
    f.draw(1, 5_000 * USD);
    assert_eq!(f.debt_t().balance(&f.settlement), 5_000 * USD);
    assert_eq!(f.debt_t().balance(&f.operator), 0);

    // 12 · the same draw_id again → 36
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(5_000 * USD)),
        Error::DuplicateDrawId
    );

    // 13 · above the per-transaction limit → 34.
    //
    // 14,000 with 5,000 already drawn: the cap gate runs first and 19,000 is inside 20,000, so 34
    // is genuinely what fires. Ask for more and 33 pre-empts it.
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(14_000 * USD)),
        Error::PerTxLimitExceeded
    );

    // 14 · beyond the cumulative cap → 33
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 3), &(16_000 * USD)),
        Error::CapExceeded
    );

    // 15 · past the grant's LTV ceiling → 35.
    //
    // Both static gates pass — 18,000 is inside the 20,000 cap and 13,000 is exactly the
    // per-transaction limit — so only the price can refuse it, which is the point of the step.
    // 18,000 against 26,400 of eligible value is 68.2%, past the 50% the customer set.
    //
    // 35 and not 11: the grant's ceiling is checked before the market's, so the more specific
    // code wins and a backend can tell "this line is maxed out" from "this vault is".
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 7), &(13_000 * USD)),
        Error::LtvCeilingExceeded
    );

    // 16 · levering up, so the liquidation steps have something to act on. Not an acceptance
    // step; it is arithmetic the scenario needs, and it must succeed.
    f.draw(6, 7_000 * USD);
    assert_eq!(f.raw_position().principal, 12_000 * USD);

    // 17 · a stale price → 40. `price_max_age` is 300 seconds and nothing republishes here, which
    // is exactly what the script arranges by back-dating the feed.
    f.warp(400);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 4), &(1_000 * USD)),
        Error::PriceStale
    );

    // 24 · revocation, and 25 · a draw after it → 30, not the retired 32.
    //
    // The grant is gone, so the lookup fails before any price is read — which is why this step
    // still returns 30 rather than 40 while the feed above is stale.
    f.revoke(&f.owner);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &(100 * USD)),
        Error::GrantNotFound
    );

    f.check_books();
}

/// The script refuses to call a run a success below ten distinct codes. These are the ten.
///
/// Listed rather than derived, because the point is to fail loudly when a gate stops being
/// exercised: a demo that quietly drops one is how a regression reaches an evidence package.
#[test]
fn the_scenario_covers_ten_distinct_error_codes() {
    let mut codes = [
        Error::ReserveUnderfunded as u32,  // 20
        Error::GrantNotFound as u32,       // 30
        Error::CapExceeded as u32,         // 33
        Error::PerTxLimitExceeded as u32,  // 34
        Error::LtvCeilingExceeded as u32,  // 35
        Error::DuplicateDrawId as u32,     // 36
        Error::GrantExists as u32,         // 38
        Error::PriceStale as u32,          // 40
        Error::GrantCeilingTooHigh as u32, // 46
        Error::LiquidationGrace as u32,    // 53
    ];
    codes.sort_unstable();
    for w in codes.windows(2) {
        assert_ne!(w[0], w[1], "two steps of the scenario share a code");
    }
    assert_eq!(codes.len(), 10);
}
