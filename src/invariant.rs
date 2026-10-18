//! Invariant assertions. Asserted at step 7 of every write function.
//!
//! These are not defensive noise. I1 is the deck's own invariant; I3′ is the strongest cheap
//! check the design still has; I4 catches a missing clamp in `restore_headroom` before it can
//! turn into an opaque overflow panic.
//!
//! I2 — that the contract's debt-token balance actually backs the undisbursed capital — needs a
//! cross-contract call, so it lives in `lib::withdraw_reserve`, the one function where a
//! discrepancy directly becomes a loss. The tests assert it after every scenario.
//!
//! # What multi-tenancy took away, and what replaces it
//!
//! When this vault served one customer, I3 was an equality anyone could check in one line:
//!
//! ```text
//! reserve.outstanding == position.principal
//! ```
//!
//! It was the strongest cheap check in the design, and it was true **only** because the state
//! was singular. With one position per customer it becomes `outstanding == Σ principal`, and no
//! single call can evaluate that sum — the contract does not know who its customers are, by
//! design, because enumerating them on chain is what makes a market vault stop scaling.
//!
//! The replacement is the same statement in **differential** form, which every call *can*
//! evaluate:
//!
//! ```text
//! Δoutstanding == Δprincipal
//! ```
//!
//! If every call preserves the delta and the market starts at `0 == 0`, the sum is preserved by
//! induction. That catches the identical class of bug at the identical place — a path that moves
//! one side without the other — and it is what the accounting mistakes in this contract have
//! actually looked like. What it cannot catch is drift introduced outside a call, which nothing
//! on chain could catch anyway.
//!
//! The absolute form does not disappear; it moves into the test suite, where `check_books` sums
//! `principal` over every owner a scenario created and compares it with `outstanding`. See
//! `src/test/harness.rs`.

use crate::errors::Error;
use crate::types::*;

/// I1, I3′ and I4 together, for a call that touched one customer's position.
///
/// Returning `InvariantViolated` rather than letting an arithmetic overflow panic is the point:
/// the runbook maps code 5 to *halt and investigate*, and an opaque host trap carries none of
/// that meaning.
pub fn check(
    r_before: &Reserve,
    r_after: &Reserve,
    p_before: &Position,
    p_after: &Position,
    idx: &IndexState,
) -> Result<(), Error> {
    // I4 — nothing goes negative.
    if r_after.total < 0
        || r_after.committed < 0
        || r_after.outstanding < 0
        || r_after.bad_debt < 0
        || p_after.deposit < 0
        || p_after.principal < 0
        || p_after.interest < 0
        || p_after.accrual_rem < 0
        || idx.index < INDEX_SCALE
    {
        return Err(Error::InvariantViolated);
    }

    // I1 — capital is not over-committed, across the whole market. A draw preserves this as an
    // *equality*: committed falls by exactly what outstanding rises by, so the sum never moves.
    //
    // The sum is checked rather than written bare: an overflow here would be a host trap, and
    // this function's whole purpose is to turn "the books are wrong" into code 5, which the
    // runbook maps to *halt and investigate*.
    let claimed = r_after
        .committed
        .checked_add(r_after.outstanding)
        .ok_or(Error::InvariantViolated)?;
    if r_after.total < claimed {
        return Err(Error::InvariantViolated);
    }

    // I3′ — this call moved `outstanding` by exactly what it moved `principal`. Interest is
    // deliberately excluded from both: including it would break I1 from the passage of time
    // alone, with nobody acting, and the market would seize up for everyone at once.
    let d_out = r_after
        .outstanding
        .checked_sub(r_before.outstanding)
        .ok_or(Error::InvariantViolated)?;
    let d_prin = p_after
        .principal
        .checked_sub(p_before.principal)
        .ok_or(Error::InvariantViolated)?;
    if d_out != d_prin {
        return Err(Error::InvariantViolated);
    }

    Ok(())
}

/// The same, for a call that touched no position at all — `fund_reserve`, `withdraw_reserve`.
///
/// I3′ degenerates to *"`outstanding` did not move"*, which is worth asserting rather than
/// skipping: a capital function that changed somebody's debt would be a serious bug and this is
/// the cheapest place it would surface.
pub fn check_shared(r_before: &Reserve, r_after: &Reserve, idx: &IndexState) -> Result<(), Error> {
    let zero = Position {
        deposit: 0,
        principal: 0,
        interest: 0,
        last_index: idx.index,
        accrual_rem: 0,
    };
    check(r_before, r_after, &zero, &zero, idx)
}
