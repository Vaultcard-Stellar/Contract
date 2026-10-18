//! Markets whose debt token is not 7 decimals.
//!
//! Every other file in this suite pins `debt_decimals` at 7, where `health::debt_value_usd` is the
//! identity and the entire rescaling path is a no-op. That was found by a coverage run, and it is
//! the most dangerous kind of gap: if the rescale were wrong, **every gate would be off by a power
//! of ten and the books would still balance**, because the same wrong figure would flow into the
//! LTV check, the liquidation threshold, the close factor and the seize computation alike. No
//! invariant would notice. No balance would disagree.
//!
//! `math_tests::debt_value_rescales_when_decimals_differ` pins the conversion as a unit. These
//! tests run the whole lifecycle — grant, draw, accrue, repay, liquidate — through a market that
//! actually lends such a token.
//!
//! Two profiles, chosen because both are real: **6 decimals** is USDC and USDT as issued on most
//! chains, **18 decimals** is anything bridged from an EVM.

use super::harness::*;
use crate::Error;

/// $26 400 is exactly 50 % of the eligible value of 1 BTC at $60 000 with a 12 % haircut, so the
/// draw ceiling lands on a round number in every decimals profile and the boundary is testable to
/// the unit.
const CEILING_USD: i128 = 26_400;

// ---------------------------------------------------------------------------
// 6 decimals — USDC as issued almost everywhere
// ---------------------------------------------------------------------------

/// The full lifecycle against a 6-decimal debt token.
///
/// `debt_value_usd` here **multiplies by ten** — `scale(raw, 6 − 7, up)`. A market that skipped
/// the rescale would read $26 400 of debt as $2 640 and let a customer borrow ten times their
/// collateral.
#[test]
fn six_decimal_debt_token_end_to_end() {
    let f = F::with_all(DEP_DEC, 6, ORACLE_DEC, |c| c);
    let unit = f.debt_unit();
    assert_eq!(unit, 1_000_000);
    assert_eq!(f.v().get_config().debt_decimals, 6);

    f.v().fund_reserve(&(100_000 * unit));
    f.v().deposit(&f.owner, &BTC);
    f.grant(50_000 * unit, 50_000 * unit, 5000);

    // The ceiling is reported in debt-token units, and the position view rescales to USD.
    let q = f.v().quote_draw(&f.owner, &f.operator, &0);
    assert_eq!(q.ltv_headroom, CEILING_USD * unit);
    assert_eq!(f.v().get_position(&f.owner).eligible_value, 528_000_000_000);

    // One unit past the ceiling is refused; the ceiling itself is not.
    assert_err!(
        f.v().try_draw(
            &f.owner,
            &f.operator,
            &id32(&f.e, 1),
            &(CEILING_USD * unit + 1)
        ),
        Error::LtvCeilingExceeded
    );
    f.draw(1, CEILING_USD * unit);

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.principal, 26_400_000_000); // 6-decimal units
    assert_eq!(p.debt_value, 264_000_000_000); // ×10, on the USD scale
    assert_eq!(p.ltv_bps, 5_000);
    f.check_books();

    // Interest is charged in debt-token units, so it scales with the token, not with USD.
    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_position().interest, 2_640 * unit); // 10 %/yr of $26 400
    f.check_books();

    // And repayment splits interest-first in the same units.
    f.repay_as(&f.owner, &f.third, &(2_640 * unit + 400 * unit));
    assert_eq!(f.raw_position().interest, 0);
    assert_eq!(f.raw_position().principal, 26_000 * unit);
    f.check_books();
}

/// Liquidation against a 6-decimal token: the close factor, the bonus and the seizure all cross
/// the decimals boundary, in both directions.
#[test]
fn six_decimal_debt_token_liquidation() {
    let f = F::with_all(DEP_DEC, 6, ORACLE_DEC, |c| c);
    let unit = f.debt_unit();

    f.v().fund_reserve(&(100_000 * unit));
    f.v().deposit(&f.owner, &BTC);
    f.grant(50_000 * unit, 50_000 * unit, 5000);
    f.draw(1, 10_000 * unit);

    f.set_price(P16K); // eligible $14 080 against $10 000 → 7103 bps
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.ltv_bps, 7_103);
    assert_eq!(q.max_repay, 5_000 * unit); // 50 % close factor, in debt units
    assert_eq!(q.seize_for_max_repay, 33_750_000); // $5 400 at $16 000, in BTC units

    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(5_000 * unit + 1), &0),
        Error::CloseFactorExceeded
    );
    let seized = f
        .v()
        .liquidate(&f.owner, &f.liquidator, &(5_000 * unit), &0);
    assert_eq!(seized, 33_750_000);
    assert_eq!(f.raw_position().principal, 5_000 * unit);
    f.check_books();
}

// ---------------------------------------------------------------------------
// 18 decimals — anything bridged from an EVM
// ---------------------------------------------------------------------------

/// The same lifecycle where `debt_value_usd` **divides by 10¹¹ and rounds up**.
///
/// Rounding up is what stops a market lending against a debt it has rounded away: at 18 decimals
/// a single smallest unit is 10⁻¹¹ of a cent, and flooring it would let a position sit one unit
/// over its ceiling and read as exactly at it.
#[test]
fn eighteen_decimal_debt_token_end_to_end() {
    let f = F::with_all(DEP_DEC, 18, ORACLE_DEC, |c| c);
    let unit = f.debt_unit();
    assert_eq!(unit, 1_000_000_000_000_000_000);

    f.v().fund_reserve(&(100_000 * unit));
    f.v().deposit(&f.owner, &BTC);
    f.grant(50_000 * unit, 50_000 * unit, 5000);

    assert_eq!(
        f.v().quote_draw(&f.owner, &f.operator, &0).ltv_headroom,
        CEILING_USD * unit
    );

    // The rounding direction, at the finest granularity the token has: one smallest unit past the
    // ceiling must be refused, even though it is 10⁻¹¹ of a cent.
    assert_err!(
        f.v().try_draw(
            &f.owner,
            &f.operator,
            &id32(&f.e, 1),
            &(CEILING_USD * unit + 1)
        ),
        Error::LtvCeilingExceeded
    );
    f.draw(1, CEILING_USD * unit);

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.principal, 26_400 * unit);
    assert_eq!(p.debt_value, 264_000_000_000); // back on the USD scale, exactly
    assert_eq!(p.ltv_bps, 5_000);
    f.check_books();

    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_position().interest, 2_640 * unit);
    f.check_books();
}

/// Insolvency at 18 decimals: `usd_to_debt` converts the recovery **back** across the boundary,
/// and `bad_debt` is booked in debt-token units.
#[test]
fn eighteen_decimal_debt_token_insolvency() {
    let f = F::with_all(DEP_DEC, 18, ORACLE_DEC, |c| c);
    let unit = f.debt_unit();

    f.v().fund_reserve(&(100_000 * unit));
    f.v().deposit(&f.owner, &BTC);
    f.grant(50_000 * unit, 50_000 * unit, 5000);
    f.draw(1, 10_000 * unit);

    f.set_price(P8K); // $8 000 of collateral against $10 000 of debt
    let before = f.raw_reserve().total;
    let seized = f.v().liquidate(&f.owner, &f.liquidator, &unit, &0);

    assert_eq!(seized, BTC);
    assert_eq!(f.raw_position().principal, 0);
    assert_eq!(f.raw_reserve().bad_debt, 2_000 * unit);
    assert_eq!(f.raw_reserve().total, before - 2_000 * unit);
    f.check_books();
}

// ---------------------------------------------------------------------------
// The degenerate rate
// ---------------------------------------------------------------------------

/// `rate_bps_year = 0` is a valid configuration — an interest-free market — and it must not stall
/// anything.
///
/// The index never moves, so `advance` reports no change on every call and `last_accrual` never
/// advances. That is the same "do not move the clock when the index did not" rule that stops
/// anyone griefing the lender's revenue, running in its degenerate case.
#[test]
fn a_zero_rate_market_never_accrues_and_never_stalls() {
    let f = F::with_all(DEP_DEC, DEBT_DEC, ORACLE_DEC, |mut c| {
        c.rate_bps_year = 0;
        c
    });
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);

    let idx_before = f.raw_index();
    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner);

    assert_eq!(f.raw_index(), idx_before, "a zero rate must move nothing");
    assert_eq!(f.raw_position().interest, 0);
    assert_eq!(f.v().get_position(&f.owner).debt, 10_000 * USD);

    // Everything downstream still works: the debt is repayable and the collateral comes back.
    f.repay_as(&f.owner, &f.owner, &(10_000 * USD));
    f.revoke(&f.owner);
    f.v().withdraw(&f.owner, &BTC);
    assert_eq!(f.raw_position().deposit, 0);
    f.check_books();
}
