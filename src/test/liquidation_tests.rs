//! Both liquidation routes, the close factor, and the insolvency branch.
//!
//! The insolvency branch is the one the deck calls *"the most serious mistake that can be made
//! in the entire contract"*: if it reverts, the position is stuck forever — nobody can close it,
//! the collateral cannot be withdrawn, the debt cannot be cleared. Everything in the second half
//! of this file exists to prove it does not.

use super::harness::*;
use crate::Error;

/// 1 BTC of collateral, $10 000 of debt, grant cap $20 000.
fn loaded() -> F {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f
}

/// The same, with the price crashed to $16 000 — LTV 7103 bps, past the 7000 threshold but
/// nowhere near the 9000 full-close point, and still solvent ($16 000 of collateral against
/// $10 000 of debt).
fn distressed() -> F {
    let f = loaded();
    f.set_price(P16K);
    f
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

#[test]
fn a_healthy_position_cannot_be_liquidated() {
    let f = loaded();
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0),
        Error::PositionHealthy
    );
    assert_err!(
        f.v().try_lender_liquidate(&f.owner, &(1_000 * USD)),
        Error::PositionHealthy
    );
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.liquidatable, false);
    assert_eq!(q.blocked_reason, Some(Error::PositionHealthy as u32));
    assert_eq!(q.ltv_bps, 1_894); // 1e11 × 1e4 / 528e9, rounded up
}

#[test]
fn a_position_with_no_debt_is_nothing_to_do() {
    let f = F::new();
    f.funded();
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0),
        Error::NoDebt
    );
    assert_eq!(
        f.v().quote_liquidation(&f.owner).blocked_reason,
        Some(Error::NoDebt as u32)
    );
}

#[test]
fn the_close_factor_bounds_one_call() {
    let f = distressed();
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.liquidatable, true);
    assert_eq!(q.ltv_bps, 7_103);
    assert_eq!(q.max_repay, 5_000 * USD); // 50 % of $10 000
    assert_eq!(q.seize_for_max_repay, 33_750_000);

    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(5_000 * USD + 1), &0),
        Error::CloseFactorExceeded
    );
}

#[test]
fn slippage_protection() {
    let f = distressed();
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &33_750_001),
        Error::SlippageExceeded
    );
    // Exactly the expected amount is fine.
    f.v()
        .liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &33_750_000);
    f.check_books();
}

#[test]
fn liquidation_rejects_non_positive_amounts() {
    let f = distressed();
    assert_err!(
        f.v().try_liquidate(&f.owner, &f.liquidator, &0, &0),
        Error::InvalidAmount
    );
    assert_err!(
        f.v().try_lender_liquidate(&f.owner, &-1),
        Error::InvalidAmount
    );
}

// ---------------------------------------------------------------------------
// The ordinary route
// ---------------------------------------------------------------------------

/// The liquidator receives exactly `repay × (1 + bonus)` of value, and the reserve's claim is
/// **not** written down — their tokens arrived, so it is intact. That single asymmetry with
/// `lender_liquidate` is where a mistake would hide.
#[test]
fn ordinary_liquidation_pays_the_bonus_and_leaves_the_claim_intact() {
    let f = distressed();
    let before = f.raw_reserve();

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &0);

    // $5 000 × 1.08 = $5 400 at $16 000/BTC = 0.3375 BTC
    assert_eq!(seized, 33_750_000);
    assert_eq!(f.dep_t().balance(&f.liquidator), 33_750_000);
    assert_eq!(
        f.debt_t().balance(&f.liquidator),
        10_000_000 * USD - 5_000 * USD
    );

    let p = f.raw_position();
    assert_eq!(p.principal, 5_000 * USD);
    assert_eq!(p.deposit, BTC - 33_750_000);

    let after = f.raw_reserve();
    assert_eq!(after.total, before.total); // NOT reduced
    assert_eq!(after.outstanding, before.outstanding - 5_000 * USD);
    // No headroom is restored, and no grant is touched.
    //
    // A liquidator pays down the debt but has no view of the customer's authorities and no
    // business choosing between them, so with several grants live there is no grant this
    // repayment belongs to. Restoring nothing is the only answer that needs neither a policy nor
    // a loop over the customer's grants on the money path.
    //
    // The capital still comes free — `outstanding` fell — it just returns to the pool rather than
    // to a spending line. The customer reopens a line by repaying and naming it, which is also the
    // better default: a position that has just been liquidated should not silently regain the
    // ability to spend.
    assert_eq!(after.committed, before.committed);
    assert_eq!(after.bad_debt, 0);

    // Liquidation is what makes the position healthy again.
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, false);
    f.check_books();
}

/// At or above `full_liq_ltv_bps` the close factor is dropped. Below that point a partial
/// liquidation still lowers LTV; above it, it raises it, and the position spirals.
#[test]
fn above_full_liq_the_whole_debt_closes_in_one_call() {
    let f = loaded();
    f.set_price(12_000 * 100_000_000_000_000); // eligible $10 560 against $10 000 of debt

    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.max_repay, 10_000 * USD); // the WHOLE debt, not half

    let seized = f
        .v()
        .liquidate(&f.owner, &f.liquidator, &(10_000 * USD), &0);
    assert_eq!(seized, 90_000_000); // $10 800 at $12 000/BTC = 0.9 BTC

    let p = f.raw_position();
    assert_eq!(p.principal, 0);
    assert_eq!(p.interest, 0);
    assert_eq!(p.deposit, BTC - 90_000_000);
    assert_eq!(f.raw_reserve().bad_debt, 0);
    f.check_books();
}

/// Otherwise a 50 % close factor leaves $3 remainders that no liquidator will bother to clear,
/// and positions hang open indefinitely.
#[test]
fn a_dust_sized_debt_closes_fully_regardless_of_ltv() {
    let f = F::new();
    f.v().fund_reserve(&(100_000 * USD));
    f.v().deposit(&f.owner, &10_000); // 0.0001 BTC = $6 at $60 000
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 2 * USD); // $2 — under the 50 % ceiling of $2.64

    f.set_price(30_000 * 100_000_000_000_000); // eligible $2.64, LTV 7576 bps
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.liquidatable, true);
    assert_eq!(q.ltv_bps, 7_576);
    // Not above full_liq_ltv — the dust escape is what allows the full close.
    assert_eq!(q.max_repay, 2 * USD);

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &(2 * USD), &0);
    assert_eq!(seized, 7_200); // $2.16 at $30 000/BTC
    assert_eq!(f.raw_position().principal, 0);
    assert_eq!(f.raw_position().deposit, 2_800);
    f.check_books();
}

/// A pause is normally triggered by an incident at the card or backend layer. Letting it block
/// liquidation would convert an operational incident into capital risk for the lender.
#[test]
fn liquidation_works_while_paused() {
    let f = distressed();
    f.v().pause();
    f.v().liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &0);
    f.check_books();
}

/// Slide 10 shows a *"still in violation → loop"* edge. The contract needs no mechanism for it:
/// each call is an independent transaction. The only requirement is that consecutive calls are
/// safe, with no state double-counted.
#[test]
fn consecutive_liquidations_are_safe() {
    let f = distressed();
    for (price, repay) in [
        (P16K, 5_000 * USD),
        (12_000 * 100_000_000_000_000, 2_500 * USD),
        (P8K, 1_250 * USD),
    ] {
        f.set_price(price);
        f.v().liquidate(&f.owner, &f.liquidator, &repay, &0);
        f.check_books();
    }
    assert_eq!(f.raw_reserve().bad_debt, 0);
    assert_eq!(f.raw_position().principal, 1_250 * USD);
}

// ---------------------------------------------------------------------------
// The lender's route
// ---------------------------------------------------------------------------

/// They net internally: collateral in, their own claim written down by the same amount. The
/// assertion that catches the bug is that **no debt token moved** — asserting only that the
/// lender received collateral would pass even if they collected twice.
#[test]
fn lender_liquidation_moves_no_debt_tokens() {
    let f = distressed();
    let vault_debt = f.debt_t().balance(&f.id);
    let lender_debt = f.debt_t().balance(&f.lender);
    let before = f.raw_reserve();

    let seized = f.v().lender_liquidate(&f.owner, &(5_000 * USD));

    assert_eq!(f.debt_t().balance(&f.id), vault_debt);
    assert_eq!(f.debt_t().balance(&f.lender), lender_debt);

    // No bonus: market price, exactly. $5 000 at $16 000/BTC = 0.3125 BTC.
    assert_eq!(seized, 31_250_000);
    assert_eq!(f.dep_t().balance(&f.lender), 31_250_000);

    let after = f.raw_reserve();
    assert_eq!(after.total, before.total - 5_000 * USD); // the netting
    assert_eq!(after.outstanding, before.outstanding - 5_000 * USD);
    // `committed` is not credited, and neither is any grant: the same reason as the ordinary
    // route. `total` and `outstanding` both fell by the netted amount, so the reserve's slack is
    // unchanged and I1 holds without touching a line.
    //
    // The earlier single-grant version shrank the one grant's `cap` and `drawn` together here, on
    // the argument that a written-off claim should shrink the line it backed. That was fairness,
    // not soundness — and with several grants it would mean picking whose line pays for the
    // write-off. The grant is left exactly as it was.
    assert_eq!(after.committed, before.committed);
    let g = f.grant_of(&f.owner).unwrap();
    assert_eq!(g.drawn, 10_000 * USD);
    assert_eq!(g.cap, 20_000 * USD);
    f.check_books();
}

/// Dropping the close factor removes the only thing that bounded `repay_amount` on this route.
/// Without the clamp, `total -= principal_part` drives the claim negative and surfaces as code 5
/// — which is supposed to mean *there is a bug*.
#[test]
fn lender_liquidation_is_clamped_to_the_debt() {
    let f = distressed();
    assert_err!(
        f.v().try_lender_liquidate(&f.owner, &(10_000 * USD + 1)),
        Error::InvalidAmount
    );
    f.v().lender_liquidate(&f.owner, &(10_000 * USD));
    assert_eq!(f.raw_position().principal, 0);
    f.check_books();
}

// ---------------------------------------------------------------------------
// Insolvency
// ---------------------------------------------------------------------------

/// The branch that must never revert.
///
/// Trigger on **market** value, not eligible value: the haircut is a buffer for *lending*
/// decisions, not for *recovery*. Using eligible value would declare insolvency while up to 12 %
/// of collateral remains — possibly enough to repay the debt in full — and write off principal
/// that was never lost.
#[test]
fn insolvency_closes_the_position_and_writes_down_the_claim() {
    let f = loaded();
    f.set_price(P8K); // $8 000 of collateral against $10 000 of debt

    let before = f.raw_reserve();
    assert_eq!(before.total, 100_000 * USD);
    let vault_debt = f.debt_t().balance(&f.id);

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    // Everything is taken, regardless of what was offered.
    assert_eq!(seized, BTC);
    assert_eq!(f.dep_t().balance(&f.liquidator), BTC);
    // The liquidator paid what the collateral was worth: $8 000.
    assert_eq!(f.debt_t().balance(&f.id), vault_debt + 8_000 * USD);

    let p = f.raw_position();
    assert_eq!(p.deposit, 0);
    assert_eq!(p.principal, 0);
    assert_eq!(p.interest, 0);

    let after = f.raw_reserve();
    assert_eq!(after.bad_debt, 2_000 * USD);
    // The line that the deck's own sketch omits, and that I1 cannot see.
    assert_eq!(after.total, before.total - 2_000 * USD);
    assert_eq!(after.outstanding, 0);
    // The grant is revoked, and committed released — headroom restored FIRST, so `released`
    // matches the books.
    assert_eq!(f.grant_of(&f.owner), None);
    assert_eq!(after.committed, 0);

    f.check_books();
}

#[test]
fn insolvency_leaves_nothing_to_liquidate() {
    let f = loaded();
    f.set_price(P8K);
    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);
    assert_err!(
        f.v().try_liquidate(&f.owner, &f.liquidator, &USD, &0),
        Error::NoDebt
    );
    f.check_books();
}

/// The netting route has no insolvency branch: it nets, so there are no incoming tokens to
/// measure a recovery against and the write-down would never be booked. Rejecting an insolvent
/// position forces the lender through the ordinary route, where `bad_debt` is recorded properly.
#[test]
fn the_lender_cannot_net_an_insolvent_position() {
    let f = loaded();
    f.set_price(P8K);
    assert_err!(
        f.v().try_lender_liquidate(&f.owner, &(1_000 * USD)),
        Error::PositionHealthy
    );
    assert_eq!(f.raw_reserve().bad_debt, 0);
}

/// A liquidator hitting this branch knows they receive **all** remaining collateral; asking for
/// more is their own error, and reverting is safe — anyone else can call with `min_out = 0`, so
/// the position is not stuck.
#[test]
fn insolvency_still_honours_slippage_protection() {
    let f = loaded();
    f.set_price(P8K);
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &USD, &(BTC + 1)),
        Error::SlippageExceeded
    );
    f.v().liquidate(&f.owner, &f.third, &USD, &0);
    assert_eq!(f.dep_t().balance(&f.third), BTC);
}

/// Insolvency with **no live grant**: `restore_headroom` finds nothing, no `GrantRevoked` is
/// emitted, and the books still balance. The branch has to survive every shape of prior state,
/// not just the tidy one.
#[test]
fn insolvency_with_no_grant() {
    let f = loaded();
    f.revoke(&f.owner);
    f.set_price(P8K);

    let before = f.raw_reserve();
    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);
    let after = f.raw_reserve();

    assert_eq!(after.bad_debt, 2_000 * USD);
    assert_eq!(after.total, before.total - 2_000 * USD);
    assert_eq!(after.committed, 0);
    assert_eq!(after.outstanding, 0);
    f.check_books();
}

/// Insolvency with accrued interest: interest is collected first out of the recovery, so `lost`
/// is the principal shortfall and nothing else. Booking interest as lost principal would
/// overstate `bad_debt` and understate the lender's revenue.
#[test]
fn insolvency_with_accrued_interest() {
    let f = loaded();
    f.warp(365 * 86_400);
    f.set_price(P8K);
    f.v().accrue(&f.owner);

    let interest = f.raw_position().interest;
    assert_eq!(interest, 1_000 * USD); // 10 %/yr on $10 000
    let before = f.raw_reserve();

    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    // $8 000 recovered: $1 000 of interest first, then $7 000 of principal, leaving $3 000 lost.
    let after = f.raw_reserve();
    assert_eq!(after.bad_debt, 3_000 * USD);
    assert_eq!(after.total, before.total + 1_000 * USD - 3_000 * USD);
    assert_eq!(after.outstanding, 0);
    f.check_books();
}
