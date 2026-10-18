//! The interest clock.
//!
//! The index compounds; what borrowers pay does not. `advance` multiplies the index, so its
//! value depends on call cadence — but settlement divides by `last_index`, which cancels
//! everything the index accumulated before this position last settled. The total is
//! `principal × rate × elapsed`, **independent of how the interval is split**.
//!
//! Every figure below was computed out of band. `10 %/yr on $10 000 for 365 days = $1 000.00`
//! exactly, which is the sanity anchor for the rest.

use super::harness::*;
use crate::Error;

const P: i128 = 10_000 * USD; // $10 000 of principal

fn borrowed() -> F {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, P);
    f
}

#[test]
fn exact_interest_at_one_thirty_and_three_hundred_sixty_five_days() {
    for (days, index, inc) in [
        (1u64, 1_000_273_972_602_739_726i128, 27_397_260i128),
        (30, 1_008_219_178_082_191_780, 821_917_808),
        (365, 1_100_000_000_000_000_000, 10_000_000_000),
    ] {
        let f = borrowed();
        f.warp_fresh(days * 86_400, P60K);
        f.v().accrue(&f.owner);
        assert_eq!(f.raw_index().index, index, "index after {days}d");
        assert_eq!(f.raw_position().interest, inc, "interest after {days}d");
        // Interest is never folded into principal, so unpaid interest does not itself accrue.
        assert_eq!(f.raw_position().principal, P);
        f.check_books();
    }
}

/// Property **P5**, and the test that closes the griefing hole.
///
/// Without the carried remainder each call throws away its fraction: on this position that is
/// ~0.7 units per call, and a year of per-second `accrue` calls would suppress ~0.044 % of the
/// lender's revenue for the price of network fees. With the carry the two answers are not
/// "within one unit" — they are **identical**.
#[test]
fn accrual_is_path_independent() {
    let many = borrowed();
    for _ in 0..3600 {
        // No price refresh: accrual never reads the oracle, which is the point of the rule.
        many.warp(1);
        many.v().accrue(&many.owner);
    }

    let once = borrowed();
    once.warp(3600);
    once.v().accrue(&once.owner);

    assert_eq!(many.raw_position().interest, 1_141_552);
    assert_eq!(once.raw_position().interest, 1_141_552);
    many.check_books();
    once.check_books();
}

/// The rate does not depend on the vault's age. A loan opened after five years of index growth
/// accrues exactly what one opened at genesis does, because settlement divides by the index at
/// **its own** opening.
///
/// This is the claim that was worth checking rather than assuming: an index-based design invites
/// the intuition that a long-lived vault charges more.
#[test]
fn interest_does_not_depend_on_vault_age() {
    let young = borrowed();
    young.warp_fresh(365 * 86_400, P60K);
    young.v().accrue(&young.owner);

    let old = F::new();
    old.funded();
    // Five years of index growth with nothing borrowed. The grant comes afterwards — a grant is
    // bounded by MAX_GRANT_SECONDS and could not span five years.
    for _ in 0..5 {
        old.warp_fresh(365 * 86_400, P60K);
        old.v().accrue(&old.owner);
    }
    old.grant(20_000 * USD, 20_000 * USD, 5000);
    old.draw(1, P);
    old.warp_fresh(365 * 86_400, P60K);
    old.v().accrue(&old.owner);

    assert_eq!(young.raw_position().interest, 10_000_000_000);
    assert_eq!(old.raw_position().interest, 10_000_000_000);
}

#[test]
fn zero_elapsed_time_changes_nothing() {
    let f = borrowed();
    let before = f.raw_index();
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_index(), before);
    assert_eq!(f.raw_position().interest, 0);
}

/// With no principal there is nothing to accrue, so the position is moved forward to the current
/// index. Leaving `last_index` stale would charge the first draw afterwards interest from an
/// ancient index — which is exactly what `interest_does_not_depend_on_vault_age` proves does not
/// happen.
#[test]
fn zero_principal_advances_the_clock_without_interest() {
    let f = F::new();
    f.funded();
    f.warp_fresh(86_400, P60K);
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_position().interest, 0);
    assert_eq!(f.raw_index().last_accrual, START + 86_400);
    assert_eq!(f.raw_position().last_index, f.raw_index().index);
}

/// Accrual never reads the price. If it did, a dead oracle would block `repay`, which runs
/// accrual at step 3 — and risk-reducing actions are never blocked.
#[test]
fn accrue_works_with_a_dead_oracle() {
    let f = borrowed();
    f.warp(86_400);
    f.kill_oracle();
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_position().interest, 27_397_260);
}

/// Interest first, then principal. It matches industry convention, drives `interest` to zero
/// leaving clean state, and makes "pay everything off" a single call of `principal + interest`.
#[test]
fn repayment_pays_interest_first() {
    let f = borrowed();
    f.warp_fresh(30 * 86_400, P60K);
    f.v().accrue(&f.owner);
    let owed = 821_917_808i128;
    assert_eq!(f.raw_position().interest, owed);

    // Less than the interest: principal untouched.
    f.repay_as(&f.owner, &f.owner, &(owed - 1));
    assert_eq!(f.raw_position().interest, 1);
    assert_eq!(f.raw_position().principal, P);

    // Now cross into principal.
    f.repay_as(&f.owner, &f.owner, &(1 + 500 * USD));
    assert_eq!(f.raw_position().interest, 0);
    assert_eq!(f.raw_position().principal, P - 500 * USD);
    f.check_books();
}

/// Interest paid is revenue: it raises the lender's claim. It is **not** capital returned, so it
/// does not touch `grant.drawn` — `cap` is a ceiling on principal, and a line that shrank as
/// interest accrued would be unpredictable for the owner.
#[test]
fn interest_repaid_raises_the_claim_but_not_the_headroom() {
    let f = borrowed();
    f.warp_fresh(30 * 86_400, P60K);
    f.v().accrue(&f.owner);

    let before = f.raw_reserve();
    let drawn_before = f.grant_of(&f.owner).unwrap().drawn;

    f.repay_as(&f.owner, &f.owner, &100);
    let after = f.raw_reserve();
    assert_eq!(after.total, before.total + 100);
    assert_eq!(after.outstanding, before.outstanding);
    assert_eq!(after.committed, before.committed);
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, drawn_before);
    f.check_books();
}

#[test]
fn repay_with_no_debt_is_rejected() {
    let f = F::new();
    f.funded();
    assert_err!(
        f.v()
            .try_repay(&f.owner, &f.owner, &1, &Some(f.operator.clone())),
        Error::NoDebt
    );
}

#[test]
fn repay_above_outstanding_is_rejected() {
    let f = borrowed();
    assert_err!(
        f.v()
            .try_repay(&f.owner, &f.owner, &(P + 1), &Some(f.operator.clone())),
        Error::InvalidAmount
    );
    assert_eq!(f.raw_position().principal, P);
}

/// Refunds and chargebacks arrive from the card programme's treasury, not the owner's wallet, so
/// repayment is open to anyone.
#[test]
fn anyone_may_repay() {
    let f = borrowed();
    f.repay_as(&f.owner, &f.third, &(1_000 * USD));
    assert_eq!(f.raw_position().principal, P - 1_000 * USD);
    assert_eq!(f.debt_t().balance(&f.third), 10_000_000 * USD - 1_000 * USD);
    f.check_books();
}
