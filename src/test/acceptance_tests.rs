//! The operator's say: propose, accept, reject.
//!
//! A grant used to be unilateral — the customer named an operator and capital was committed on
//! the spot. That is defensible for a permission, since the operator can simply never draw. It
//! stopped being defensible for two things the operator alone can judge, and neither is checkable
//! on chain:
//!
//! - **`settlement`.** The owner picks it and the contract can only check it is not the operator
//!   themselves. Whether it is an address the operator *controls* would need a registry on chain,
//!   which needs governance, which this market does not have. An operator drawing against a grant
//!   that settles to a stranger has delivered goods and paid a stranger for them.
//! - **`cap`.** The sum of caps granted to an operator is what moves if that operator's key is
//!   stolen. A customer must not be able to enlarge the target on somebody else's key, nor push a
//!   regulated provider past a per-customer exposure limit they are obliged to keep.
//!
//! So the decision belongs to the party that can check these off chain and then sign. Capital is
//! committed at acceptance, which is also the moment the line becomes usable — so the promise
//! that a card transaction is never declined because somebody else borrowed first is unaffected.

use super::harness::*;
use crate::Error;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::Address;

/// A proposal reserves nothing and can do nothing.
#[test]
fn a_proposal_commits_no_capital_and_cannot_be_drawn() {
    let f = F::new();
    f.funded();
    let free_before = f.v().get_reserve().free;

    f.propose(20_000 * USD, 5_000 * USD, 5000);

    let g = f.grant_of(&f.owner).expect("the proposal is stored");
    assert_eq!(g.accepted, false);
    assert_eq!(g.cap, 20_000 * USD);

    // Nothing moved. The lender's capital is still available to somebody else's line.
    let r = f.v().get_reserve();
    assert_eq!(r.committed, 0);
    assert_eq!(r.free, free_before);

    // And nothing can be drawn against it. Code 48, not 30 — the customer *did* authorise this
    // operator, and the remedy is to accept rather than to go back and ask.
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(100 * USD)),
        Error::GrantNotAccepted
    );
    f.check_books();
}

/// Acceptance is where the capital is committed and the line becomes usable.
#[test]
fn acceptance_commits_the_capital_and_opens_the_line() {
    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.accept(&f.owner);

    assert_eq!(f.grant_of(&f.owner).unwrap().accepted, true);
    assert_eq!(f.v().get_reserve().committed, 20_000 * USD);
    f.draw(1, 5_000 * USD);
    assert_eq!(f.raw_position().principal, 5_000 * USD);
    f.check_books();
}

/// Only the named operator may accept — asserted with a **real signature**, not under
/// `mock_all_auths`.
///
/// The fixture mocks every authorisation, which is right for tests about arithmetic and wrong for
/// this one: under it, `operator.require_auth()` passes for whoever calls, so a test written the
/// easy way would assert that a lookup failed and quietly prove nothing about who may sign. The
/// customer's own signature is supplied here and the call must still fail.
#[test]
fn nobody_but_the_operator_can_accept() {
    use soroban_sdk::testutils::{MockAuth, MockAuthInvoke};
    use soroban_sdk::IntoVal;

    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.e.mock_auths(&[MockAuth {
        address: &f.owner,
        invoke: &MockAuthInvoke {
            contract: &f.id,
            fn_name: "accept_grant",
            args: (f.owner.clone(), f.operator.clone()).into_val(&f.e),
            sub_invokes: &[],
        },
    }]);

    // The customer proposed the line and cannot take it up on the operator's behalf. Accepting
    // is where the operator says the settlement address is one they collect from and the cap is
    // one they will carry; a customer who could sign it for them has taken back both decisions.
    assert!(f.v().try_accept_grant(&f.owner, &f.operator).is_err());

    f.e.mock_all_auths();
    assert_eq!(f.grant_of(&f.owner).unwrap().accepted, false);
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

/// The same for rejection: a customer cannot decline on the operator's behalf.
#[test]
fn nobody_but_the_operator_can_reject() {
    use soroban_sdk::testutils::{MockAuth, MockAuthInvoke};
    use soroban_sdk::IntoVal;

    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.e.mock_auths(&[MockAuth {
        address: &f.owner,
        invoke: &MockAuthInvoke {
            contract: &f.id,
            fn_name: "reject_grant",
            args: (f.owner.clone(), f.operator.clone()).into_val(&f.e),
            sub_invokes: &[],
        },
    }]);
    assert!(f.v().try_reject_grant(&f.owner, &f.operator).is_err());

    f.e.mock_all_auths();
    assert!(f.grant_of(&f.owner).is_some(), "the proposal survives");
    f.check_books();
}

/// Accepting a grant that was never proposed to you is `30`, not a way in.
#[test]
fn an_operator_with_no_proposal_has_nothing_to_accept() {
    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    let stranger = Address::generate(&f.e);
    assert_err!(
        f.v().try_accept_grant(&f.owner, &stranger),
        Error::GrantNotFound
    );
    assert_eq!(f.grant_of(&f.owner).unwrap().accepted, false);
    f.check_books();
}

/// Accepting twice would commit the cap twice for one line.
#[test]
fn acceptance_is_not_idempotent() {
    let f = F::new();
    f.armed(); // proposed and accepted
    assert_eq!(f.v().get_reserve().committed, 20_000 * USD);

    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.operator),
        Error::GrantAlreadyAccepted
    );
    assert_eq!(f.v().get_reserve().committed, 20_000 * USD);
    f.check_books();
}

/// The first reason acceptance exists: the operator refuses a cap larger than they will carry.
///
/// Nothing on chain expresses "this operator will not take more than X from one customer" — it is
/// their own risk posture and their own regulator's limit. The contract's part is only to make
/// the refusal possible; the judgement is theirs, off chain, before they sign.
#[test]
fn an_operator_can_refuse_a_cap_that_is_too_large_for_them() {
    let f = F::new();
    f.funded();
    f.propose(100_000 * USD, 5_000 * USD, 5000);

    // The operator's own limit, whatever it is, is exceeded. They decline.
    f.v().reject_grant(&f.owner, &f.operator);

    assert_eq!(f.grant_of(&f.owner), None);
    assert_eq!(f.v().get_reserve().committed, 0);

    // And the customer can come back with terms the operator will take.
    f.grant(10_000 * USD, 5_000 * USD, 5000);
    assert_eq!(f.v().get_reserve().committed, 10_000 * USD);
    f.check_books();
}

/// The second reason, and the one that was a real hole: the settlement address.
///
/// The customer chooses where draws land. The contract can only refuse `settlement == operator`;
/// it cannot know whether the address belongs to the operator, because knowing would need a
/// registry and a registry needs governance. Before acceptance existed, an operator's own
/// submitter would sign a draw and send the funds wherever the customer had written — to the
/// customer themselves, if they had asked for that — while the operator had already delivered.
#[test]
fn an_operator_can_refuse_a_settlement_address_they_do_not_control() {
    let f = F::new();
    f.funded();

    // The customer proposes a line settling to their own wallet rather than the operator's.
    // Passes every check the contract can make: it is not the operator's address.
    f.v().grant_authority(
        &f.owner,
        &f.operator,
        &f.owner, // ← not the operator's address, and not the operator's to collect from
        &(20_000 * USD),
        &(5_000 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    assert_eq!(f.grant_of(&f.owner).unwrap().settlement, f.owner);

    // The operator reads it, sees where the money would go, and declines. This is the whole
    // point: the check is off chain, the refusal is on chain.
    f.v().reject_grant(&f.owner, &f.operator);
    assert_eq!(f.grant_of(&f.owner), None);

    // Nothing was ever drawable against it.
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(100 * USD)),
        Error::GrantNotFound
    );
    f.check_books();
}

/// An accepted line cannot be rejected — the operator has no unilateral exit.
///
/// Capital is committed against it and the customer may have drawn. Ending it is the customer's
/// call, with `revoke_authority`. An operator who wants out stops drawing, which costs them
/// nothing and the customer nothing.
#[test]
fn an_accepted_line_cannot_be_rejected() {
    let f = F::new();
    f.armed();
    f.draw(1, 1_000 * USD);

    assert_err!(
        f.v().try_reject_grant(&f.owner, &f.operator),
        Error::GrantAlreadyAccepted
    );
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 1_000 * USD);
    f.check_books();
}

/// The customer can withdraw a proposal before it is taken up, and it releases nothing because
/// nothing was ever committed.
#[test]
fn the_customer_can_revoke_a_proposal_and_it_releases_nothing() {
    let f = F::new();
    f.funded();
    let free_before = f.v().get_reserve().free;
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.revoke(&f.owner);

    assert_eq!(f.grant_of(&f.owner), None);
    let r = f.v().get_reserve();
    assert_eq!(r.committed, 0);
    assert_eq!(
        r.free, free_before,
        "nothing was committed, so nothing came back"
    );
    f.check_books();
}

/// A proposal left to expire is reaped like any other, and also releases nothing.
#[test]
fn an_expired_proposal_is_reaped_and_releases_nothing() {
    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.warp_fresh(31 * 86_400, P60K);
    f.reap(&f.owner);

    assert_eq!(f.grant_of(&f.owner), None);
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

/// An operator cannot sit on a proposal past its expiry and take it up afterwards.
///
/// The customer set a date believing the offer dies on it. Accepting after would commit capital
/// against a line they think is gone — and `cheap_gates` would refuse every draw with 31 anyway,
/// so the only effect would be capital locked until somebody reaped it.
#[test]
fn an_expired_proposal_cannot_be_accepted() {
    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    f.warp_fresh(31 * 86_400, P60K);

    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.operator),
        Error::GrantExpired
    );
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

/// A pending grant still occupies one of the customer's eight slots.
///
/// Otherwise proposals are free and unbounded, and the walk that `wipe_out` makes over a
/// customer's grants stops being bounded by anything the contract controls.
#[test]
fn pending_grants_count_against_the_bound() {
    let f = F::new();
    f.funded();

    let mut proposed = soroban_sdk::Vec::new(&f.e);
    for _ in 0..crate::types::MAX_GRANTS_PER_OWNER {
        let operator = Address::generate(&f.e);
        f.v().grant_authority(
            &f.owner,
            &operator,
            &Address::generate(&f.e),
            &(1_000 * USD),
            &(100 * USD),
            &(f.now() + 86_400),
            &5000,
        );
        proposed.push_back(operator);
    }
    // None accepted, so no capital is committed at all.
    assert_eq!(f.v().get_reserve().committed, 0);

    let one_more = Address::generate(&f.e);
    assert_err!(
        f.v().try_grant_authority(
            &f.owner,
            &one_more,
            &Address::generate(&f.e),
            &(1_000 * USD),
            &(100 * USD),
            &(f.now() + 86_400),
            &5000,
        ),
        Error::TooManyGrants
    );

    // A rejection frees the slot, exactly as a revocation does.
    f.v().reject_grant(&f.owner, &proposed.get_unchecked(0));
    f.v().grant_authority(
        &f.owner,
        &one_more,
        &Address::generate(&f.e),
        &(1_000 * USD),
        &(100 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    f.check_books();
}

/// Freezing a proposal is refused: there is nothing to stop.
#[test]
fn a_proposal_cannot_be_frozen() {
    let f = F::new();
    f.funded();
    f.propose(20_000 * USD, 5_000 * USD, 5000);

    assert_err!(
        f.v().try_freeze_grant(&f.owner, &f.operator, &f.lender),
        Error::GrantNotAccepted
    );
    assert_err!(
        f.v().try_unfreeze_grant(&f.owner, &f.operator),
        Error::GrantNotAccepted
    );
    f.check_books();
}

/// Insolvency releases accepted lines and drops pending ones, and the reserve arithmetic has to
/// tell them apart — releasing a pending grant's `cap − drawn` would subtract capital that was
/// never added.
#[test]
fn wiping_out_a_position_handles_pending_and_accepted_alike() {
    let f = F::new();
    f.funded();

    let pending_op = Address::generate(&f.e);
    f.grant(20_000 * USD, 20_000 * USD, 5000); // accepted
    f.v().grant_authority(
        &f.owner,
        &pending_op,
        &Address::generate(&f.e),
        &(5_000 * USD),
        &(1_000 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    assert_eq!(
        f.v().get_reserve().committed,
        20_000 * USD,
        "the pending one adds nothing"
    );

    f.draw(1, 10_000 * USD);
    f.set_price(P8K); // $8 000 of collateral against $10 000 of debt
    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    assert_eq!(f.v().get_grant_operators(&f.owner).len(), 0);
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}
