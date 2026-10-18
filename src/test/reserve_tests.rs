//! Lender capital.
//!
//! Withdrawal is **not** blocked by pause: invariant I1 already prevents withdrawing committed
//! capital, and blocking further would only lock the lender's legitimate funds without adding
//! safety.

use super::harness::*;
use crate::Error;

#[test]
fn fund_and_withdraw() {
    let f = F::new();
    f.v().fund_reserve(&(100_000 * USD));
    assert_eq!(f.v().get_reserve().total, 100_000 * USD);
    assert_eq!(f.debt_t().balance(&f.id), 100_000 * USD);

    f.v().withdraw_reserve(&(40_000 * USD));
    assert_eq!(f.v().get_reserve().total, 60_000 * USD);
    assert_eq!(f.debt_t().balance(&f.id), 60_000 * USD);
    f.check_books();
}

#[test]
fn fund_and_withdraw_reject_non_positive() {
    let f = F::new();
    assert_err!(f.v().try_fund_reserve(&0), Error::InvalidAmount);
    assert_err!(f.v().try_withdraw_reserve(&-1), Error::InvalidAmount);
}

/// The boundary, to the unit: `total − amount >= committed + outstanding`.
#[test]
fn withdraw_below_committed_plus_outstanding() {
    let f = F::new();
    f.armed(); // committed $20 000
    f.draw(1, 5_000 * USD); // committed $15 000, outstanding $5 000

    assert_err!(
        f.v().try_withdraw_reserve(&(80_001 * USD)),
        Error::ReserveLocked
    );
    f.v().withdraw_reserve(&(80_000 * USD));
    let r = f.v().get_reserve();
    assert_eq!(r.total, 20_000 * USD);
    assert_eq!(r.free, 0);
    f.check_books();
}

/// No price is read, so a dead feed cannot lock the lender's capital; and a pause must not
/// either.
#[test]
fn withdraw_works_with_a_dead_oracle_and_while_paused() {
    let f = F::new();
    f.funded();
    f.kill_oracle();
    f.v().pause();
    f.v().withdraw_reserve(&(100_000 * USD));
    assert_eq!(f.v().get_reserve().total, 0);
}

/// The lender's revenue is withdrawable like any other capital, and this is the one path where
/// `total` can exceed what was ever funded.
#[test]
fn earned_interest_is_withdrawable() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);
    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner);

    let interest = f.raw_position().interest;
    assert_eq!(interest, 500 * USD); // 10 %/yr on $5 000
    f.repay_as(&f.owner, &f.owner, &(5_000 * USD + interest));

    let r = f.v().get_reserve();
    assert_eq!(r.total, 100_500 * USD);
    assert_eq!(r.outstanding, 0);

    f.revoke(&f.owner);
    f.v().withdraw_reserve(&(100_500 * USD));
    assert_eq!(f.debt_t().balance(&f.id), 0);
    assert_eq!(f.v().get_reserve().total, 0);
    f.check_books();
}

/// `withdraw_reserve` is the one function that asserts I2 on chain. It costs a cross-contract
/// call, and this is the single place where a discrepancy turns directly into a loss.
#[test]
fn withdrawal_leaves_the_undisbursed_capital_backed() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);

    // Tokens in the vault: 100 000 funded − 5 000 disbursed = 95 000.
    assert_eq!(f.debt_t().balance(&f.id), 95_000 * USD);
    f.v().withdraw_reserve(&(80_000 * USD));
    // total 20 000, outstanding 5 000 → 15 000 must remain backed, and 15 000 is what is there.
    assert_eq!(f.debt_t().balance(&f.id), 15_000 * USD);
    f.check_books();
}
