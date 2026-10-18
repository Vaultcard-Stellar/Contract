//! Several authorities at once, one per provider.
//!
//! This is the capability the deck specified — `Grant(Address) // key = operator` alongside a
//! `GrantIndex` — and that decision Đ-02 deliberately traded away for
//! a single live grant. That trade rested on four reasons, and the fourth ("the product has one
//! operator") stopped being true the moment a customer could choose between an electricity
//! company, a card issuer and a water utility. The doc said to reopen the decision before that
//! happened; this module is the evidence it was reopened correctly rather than merely reversed.
//!
//! The three reasons that were *not* about the operator count still have to be answered, and each
//! has a test here:
//!
//! - **Which grant does a repayment reopen?** The payer names it. No policy, no loop.
//! - **Gas.** The index is bounded at [`MAX_GRANTS_PER_OWNER`], and no money path iterates it.
//! - **LTV.** Grants share one position and therefore one ceiling — accepted, and pinned down by
//!   [`grants_share_the_position_and_therefore_its_ltv`] so nobody later reads the split caps as a
//!   split of collateral.

use super::harness::*;
use crate::types::MAX_GRANTS_PER_OWNER;
use crate::Error;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::Address;

/// A provider: somebody who draws, and the pinned address they settle to.
///
/// Two accounts, always distinct — the contract refuses a grant where they are the same, because
/// an operator settling to itself is an operator paying itself.
struct Provider {
    operator: Address,
    settlement: Address,
}

fn provider(f: &F) -> Provider {
    Provider {
        operator: Address::generate(&f.e),
        settlement: Address::generate(&f.e),
    }
}

impl F {
    /// Propose and accept, because most tests here are about having several lines rather than
    /// about the two-signature handshake. The ones that are about it call the client directly.
    fn grant_to(&self, owner: &Address, p: &Provider, cap: i128, per_tx: i128) {
        self.propose_to(owner, p, cap, per_tx);
        self.v().accept_grant(owner, &p.operator);
    }

    fn propose_to(&self, owner: &Address, p: &Provider, cap: i128, per_tx: i128) {
        self.v().grant_authority(
            owner,
            &p.operator,
            &p.settlement,
            &cap,
            &per_tx,
            &(self.now() + 30 * 86_400),
            &5000,
        );
    }
}

/// The customer's own example: deposit once, then split the spending authority three ways.
///
/// Note what is *not* split — the collateral. There is one position and one deposit; the caps
/// divide how much each provider may draw against it, which is the thing a customer actually
/// wants to control.
#[test]
fn one_deposit_backs_three_providers_at_once() {
    let f = F::new();
    f.funded();

    let electricity = provider(&f);
    let card = provider(&f);
    let water = provider(&f);

    f.grant_to(&f.owner, &electricity, 5_000 * USD, 1_000 * USD);
    f.grant_to(&f.owner, &card, 5_000 * USD, 2_000 * USD);
    f.grant_to(&f.owner, &water, 10_000 * USD, 1_000 * USD);

    // All three are live, and each carries its own terms.
    let ops = f.v().get_grant_operators(&f.owner);
    assert_eq!(ops.len(), 3);
    assert_eq!(
        f.v()
            .get_grant(&f.owner, &electricity.operator)
            .unwrap()
            .settlement,
        electricity.settlement
    );
    assert_eq!(
        f.v()
            .get_grant(&f.owner, &card.operator)
            .unwrap()
            .per_tx_limit,
        2_000 * USD
    );

    // The reserve committed the sum of the caps, once each.
    assert_eq!(f.v().get_reserve().committed, 20_000 * USD);

    // Each draws on its own line, and the draws land in the right settlement accounts.
    f.v().draw(
        &f.owner,
        &electricity.operator,
        &id32(&f.e, 1),
        &(900 * USD),
    );
    f.v()
        .draw(&f.owner, &card.operator, &id32(&f.e, 2), &(2_000 * USD));
    f.v()
        .draw(&f.owner, &water.operator, &id32(&f.e, 3), &(1_000 * USD));

    assert_eq!(f.debt_t().balance(&electricity.settlement), 900 * USD);
    assert_eq!(f.debt_t().balance(&card.settlement), 2_000 * USD);
    assert_eq!(f.debt_t().balance(&water.settlement), 1_000 * USD);

    // One position carries the whole debt — the grants divide authority, not the loan.
    assert_eq!(f.raw_position().principal, 3_900 * USD);
    f.check_books();
}

/// One provider's line is exhausted; the others are untouched.
///
/// This is the property that makes several caps worth having. Under a single grant the customer
/// chooses one number and every provider spends against it, so a runaway utility can consume the
/// card's headroom.
#[test]
fn exhausting_one_line_does_not_touch_another() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 2_000 * USD, 2_000 * USD);
    f.grant_to(&f.owner, &b, 5_000 * USD, 5_000 * USD);

    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(2_000 * USD));

    // A is spent.
    assert_eq!(
        f.v().try_draw(&f.owner, &a.operator, &id32(&f.e, 2), &USD),
        Err(Ok(Error::CapExceeded))
    );
    // B is not.
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 3), &(5_000 * USD));
    assert_eq!(
        f.v().get_grant(&f.owner, &b.operator).unwrap().drawn,
        5_000 * USD
    );
    f.check_books();
}

/// An operator may only draw on its own grant, and holding one grant is not a key to another's.
#[test]
fn an_operator_cannot_draw_on_another_providers_line() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 5_000 * USD, 5_000 * USD);

    // B has no grant at all: not "a smaller line", none.
    assert_eq!(
        f.v()
            .try_draw(&f.owner, &b.operator, &id32(&f.e, 1), &(100 * USD)),
        Err(Ok(Error::GrantNotFound))
    );

    // And revoking B's non-existent grant does not disturb A's.
    assert_eq!(
        f.v().try_revoke_authority(&f.owner, &b.operator),
        Err(Ok(Error::GrantNotFound))
    );
    assert!(f.v().get_grant(&f.owner, &a.operator).is_some());
    f.check_books();
}

/// Revoking one provider leaves the rest running, and returns exactly that provider's headroom.
///
/// The fire alarm gets more useful with several grants, not less: a customer who suspects one
/// provider can cut that one off without losing the electricity supply.
#[test]
fn revoking_one_provider_leaves_the_others_running() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 4_000 * USD, 4_000 * USD);
    f.grant_to(&f.owner, &b, 6_000 * USD, 6_000 * USD);
    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(1_000 * USD));

    let before = f.v().get_reserve();
    f.v().revoke_authority(&f.owner, &a.operator);
    let after = f.v().get_reserve();

    // Only A's *undrawn* headroom comes back — the drawn 1 000 is a real loan and stays out.
    assert_eq!(after.committed, before.committed - 3_000 * USD);
    assert_eq!(after.outstanding, before.outstanding);

    assert!(f.v().get_grant(&f.owner, &a.operator).is_none());
    assert_eq!(f.v().get_grant_operators(&f.owner).len(), 1);
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 2), &(6_000 * USD));
    f.check_books();
}

/// A repayment reopens the line it names, and only that one.
///
/// This is Đ-02's first objection answered. The contract does not decide whether a repayment goes
/// to the oldest line or the largest or the one drawn most recently — the payer says which, and
/// the answer costs one storage read.
#[test]
fn a_repayment_reopens_the_line_it_names() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 5_000 * USD, 5_000 * USD);
    f.grant_to(&f.owner, &b, 5_000 * USD, 5_000 * USD);
    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(3_000 * USD));
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 2), &(3_000 * USD));

    f.v().repay(
        &f.owner,
        &f.owner,
        &(2_000 * USD),
        &Some(a.operator.clone()),
    );

    assert_eq!(
        f.v().get_grant(&f.owner, &a.operator).unwrap().drawn,
        1_000 * USD
    );
    assert_eq!(
        f.v().get_grant(&f.owner, &b.operator).unwrap().drawn,
        3_000 * USD
    );
    f.check_books();
}

/// Naming nobody pays the debt down and reopens nothing.
///
/// What a third party does — somebody settling a bill on the customer's behalf has no view of
/// their arrangements and should not be guessing which line to reopen.
#[test]
fn repaying_without_naming_a_grant_reopens_nothing() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    f.grant_to(&f.owner, &a, 5_000 * USD, 5_000 * USD);
    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(3_000 * USD));
    let before = f.v().get_reserve();

    f.repay_only(&f.owner, &f.third, &(2_000 * USD));

    assert_eq!(
        f.v().get_grant(&f.owner, &a.operator).unwrap().drawn,
        3_000 * USD
    );
    assert_eq!(f.v().get_reserve().committed, before.committed);
    // The capital still came back to the pool; it just did not become spendable again.
    assert_eq!(
        f.v().get_reserve().outstanding,
        before.outstanding - 2_000 * USD
    );
    f.check_books();
}

/// Granting twice to the *same* operator is still refused; to a different one is not.
///
/// The duplicate check narrowed from "any grant" to "this operator's grant", and that narrowing is
/// the whole change. `drawn` living inside the struct means an overwrite would zero it, so the
/// refusal has to stay.
#[test]
fn a_second_grant_to_the_same_operator_is_still_refused() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 5_000 * USD, 5_000 * USD);

    assert_eq!(
        f.v().try_grant_authority(
            &f.owner,
            &a.operator,
            &a.settlement,
            &(9_000 * USD),
            &(1_000 * USD),
            &(f.now() + 86_400),
            &5000,
        ),
        Err(Ok(Error::GrantExists))
    );

    // A different operator is a different grant, and is allowed.
    f.grant_to(&f.owner, &b, 5_000 * USD, 5_000 * USD);
    assert_eq!(f.v().get_grant_operators(&f.owner).len(), 2);
    f.check_books();
}

/// The index is bounded, and the bound is enforced rather than documented.
///
/// Đ-02's third objection was a gas bomb: an unbounded `Vec<Address>` that `wipe_out` and the TTL
/// bump would have to walk. Bounded at eight, neither walk can grow without limit, and no path
/// that moves money walks it at all.
#[test]
fn the_number_of_simultaneous_grants_is_bounded() {
    let f = F::new();
    f.funded();

    let mut providers = soroban_sdk::Vec::new(&f.e);
    for _ in 0..MAX_GRANTS_PER_OWNER {
        let p = provider(&f);
        f.grant_to(&f.owner, &p, 1_000 * USD, 100 * USD);
        providers.push_back(p.operator);
    }
    assert_eq!(
        f.v().get_grant_operators(&f.owner).len(),
        MAX_GRANTS_PER_OWNER
    );

    let one_too_many = provider(&f);
    assert_eq!(
        f.v().try_grant_authority(
            &f.owner,
            &one_too_many.operator,
            &one_too_many.settlement,
            &(1_000 * USD),
            &(100 * USD),
            &(f.now() + 86_400),
            &5000,
        ),
        Err(Ok(Error::TooManyGrants))
    );

    // A customer who needs a ninth revokes one. The bound is a limit on simultaneity, not on how
    // many providers a customer may ever use.
    f.v()
        .revoke_authority(&f.owner, &providers.get_unchecked(0));
    f.grant_to(&f.owner, &one_too_many, 1_000 * USD, 100 * USD);
    assert_eq!(
        f.v().get_grant_operators(&f.owner).len(),
        MAX_GRANTS_PER_OWNER
    );
    f.check_books();
}

/// `wipe_out` releases every grant, not the first one it finds.
///
/// The bounded index earns its keep here: without it there is no way to find a customer's grants
/// to release, and their `committed` capital would be stranded in the reserve forever.
#[test]
fn wiping_out_a_position_releases_every_grant() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    let c = provider(&f);
    f.grant_to(&f.owner, &a, 3_000 * USD, 3_000 * USD);
    f.grant_to(&f.owner, &b, 3_000 * USD, 3_000 * USD);
    f.grant_to(&f.owner, &c, 3_000 * USD, 3_000 * USD);
    assert_eq!(f.v().get_reserve().committed, 9_000 * USD);

    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(3_000 * USD));
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 2), &(3_000 * USD));
    f.v()
        .draw(&f.owner, &c.operator, &id32(&f.e, 3), &(3_000 * USD));

    // $8 000 of collateral against $9 000 of debt: insolvent, so `liquidate` takes the wipe-out
    // branch and closes the position outright.
    f.set_price(P8K);
    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    assert_eq!(f.v().get_grant_operators(&f.owner).len(), 0);
    assert!(f.v().get_grant(&f.owner, &a.operator).is_none());
    assert!(f.v().get_grant(&f.owner, &b.operator).is_none());
    assert!(f.v().get_grant(&f.owner, &c.operator).is_none());
    // Every committed unit is released — none stranded behind a grant nobody can reach.
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

/// Grants share the position, and therefore share its LTV ceiling.
///
/// Đ-02's second objection, and the one this design **accepts** rather than answers. Three caps of
/// $5 000 do not mean $15 000 of borrowing power: the collateral is one pool, the ceiling applies
/// to the whole debt, and the third provider is refused by LTV with its own cap barely touched.
///
/// Pinned down here because the customer-facing wording depends on it. "1 BTC to electricity,
/// 1 BTC to water" is a way of describing caps, not a division of collateral, and a screen that
/// implied otherwise would be promising borrowing power that does not exist.
#[test]
fn grants_share_the_position_and_therefore_its_ltv() {
    let f = F::new();
    f.funded(); // 1 BTC at $60 000, 12% haircut, 50% ceiling → $26 400 borrowable
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 20_000 * USD, 20_000 * USD);
    f.grant_to(&f.owner, &b, 20_000 * USD, 20_000 * USD);

    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 1), &(20_000 * USD));

    // B's own cap is untouched, and B is still refused: the ceiling is the position's.
    assert_eq!(f.v().get_grant(&f.owner, &b.operator).unwrap().drawn, 0);
    assert_eq!(
        f.v()
            .try_draw(&f.owner, &b.operator, &id32(&f.e, 2), &(10_000 * USD)),
        Err(Ok(Error::LtvCeilingExceeded))
    );

    // What is left under the ceiling is available to B, and to whoever asks first.
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 3), &(6_000 * USD));
    f.check_books();
}

/// Freezing one provider stops that one and no other.
#[test]
fn freezing_is_per_provider() {
    let f = F::new();
    f.funded();
    let a = provider(&f);
    let b = provider(&f);
    f.grant_to(&f.owner, &a, 5_000 * USD, 5_000 * USD);
    f.grant_to(&f.owner, &b, 5_000 * USD, 5_000 * USD);

    f.v().freeze_grant(&f.owner, &a.operator, &f.owner);

    assert_eq!(
        f.v()
            .try_draw(&f.owner, &a.operator, &id32(&f.e, 1), &(100 * USD)),
        Err(Ok(Error::GrantFrozen))
    );
    f.v()
        .draw(&f.owner, &b.operator, &id32(&f.e, 2), &(100 * USD));

    f.v().unfreeze_grant(&f.owner, &a.operator);
    f.v()
        .draw(&f.owner, &a.operator, &id32(&f.e, 3), &(100 * USD));
    f.check_books();
}
