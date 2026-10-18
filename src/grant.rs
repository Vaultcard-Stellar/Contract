//! Grant gates and headroom accounting.
//!
//! Pure functions over already-loaded structs; `lib` reads state, hands it down, writes it back.

use crate::errors::Error;
use crate::math;
use crate::types::*;
use soroban_sdk::Address;

/// The gates that need only the grant, in **fixed order**.
///
/// Order is part of the specification, not an implementation detail. The backend branches on
/// which code comes back — 33 means *halt the vault*, 35 means *release headroom and carry on*
/// — so a call violating several conditions must return a deterministic code, and the tests
/// assert exactly that.
///
/// Everything here is a storage read already in hand. The expensive checks — accrual, the
/// cross-contract price call, the LTV comparison — come after, so a draw rejected on one of
/// these never pays for an oracle round trip.
pub fn cheap_gates(g: &Grant, operator: &Address, amount: i128, now: u64) -> Result<(), Error> {
    if &g.operator != operator {
        return Err(Error::GrantNotFound);
    }
    // Before `frozen`, and the order carries meaning: a pending grant is not a grant that has
    // been stopped, it is one that was never taken up. The operator's remedy is `accept_grant`,
    // not asking the customer to unfreeze something.
    if !g.accepted {
        return Err(Error::GrantNotAccepted);
    }
    if g.frozen {
        return Err(Error::GrantFrozen);
    }
    if now >= g.expiry {
        return Err(Error::GrantExpired);
    }
    if math::checked_add(g.drawn, amount)? > g.cap {
        return Err(Error::CapExceeded);
    }
    if amount > g.per_tx_limit {
        return Err(Error::PerTxLimitExceeded);
    }
    Ok(())
}

/// `cap − drawn`. What the static ceiling still allows, before the price is consulted.
pub fn cap_headroom(g: &Grant) -> i128 {
    let h = g.cap - g.drawn;
    if h < 0 {
        0
    } else {
        h
    }
}

/// Give back headroom for principal repaid. Returns the amount actually restored.
///
/// **One call site: `repay`, and only when the payer names a grant.**
///
/// A customer may hold several — an electricity supplier, a card issuer, a portal — and a
/// repayment of 10,000 has to reopen exactly one of them. Choosing by age, pro rata, or by whoever
/// drew last is a policy, and every version of it needs a loop over the customer's grants on the
/// money path. So the payer names it. That was Đ-02's first and strongest objection to several
/// grants, and naming answers it without choosing anything.
///
/// The liquidation paths restore **nothing**, which is why they no longer call this. Nobody in
/// those paths can say which authority should reopen — the liquidator is a stranger — and a
/// forced repayment reopening a customer's spending power is not obviously right even if it
/// could. `committed` and `outstanding` both stay put, so invariant I1 is unaffected.
///
/// # Why `min(...)`
///
/// `position.principal` can exceed the *current* grant's `drawn`: grant A draws 30 000 and is
/// revoked, grant B draws 10 000, principal is 40 000 but `grant_B.drawn` is 10 000. Without the
/// clamp, `drawn` goes negative.
///
/// # Why only a live grant
///
/// Revocation already released that grant's `committed`. Crediting it again on repayment would
/// double-count capital and inflate `committed` out of nothing.
pub fn restore_headroom(
    grant: &mut Grant,
    reserve: &mut Reserve,
    principal_paid: i128,
) -> Result<i128, Error> {
    let r = math::min_i128(principal_paid, grant.drawn);
    if r <= 0 {
        return Ok(0);
    }
    grant.drawn = math::checked_sub(grant.drawn, r)?;
    reserve.committed = math::checked_add(reserve.committed, r)?;
    Ok(r)
}

/// Release the unused commitment and hand back the figure for the event.
///
/// Used by `revoke_authority`, `reap_expired`, and the insolvency branch. Calling it twice must
/// be safe — the second call finds no grant and releases nothing, which is why the caller
/// deletes the entry rather than flagging it.
///
/// **A pending grant releases nothing**, because `accept_grant` is what commits and it never
/// ran. Releasing `cap − drawn` for one would subtract capital that was never added, driving
/// `committed` below what the live lines actually promise — and the reserve would then report
/// free capital it has already lent to somebody else.
pub fn release(g: &Grant, reserve: &mut Reserve) -> Result<i128, Error> {
    if !g.accepted {
        return Ok(0);
    }
    let released = cap_headroom(g);
    reserve.committed = math::checked_sub(reserve.committed, released)?;
    Ok(released)
}
