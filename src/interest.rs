//! The interest clock and position settlement.
//!
//! # The index accumulates a rate, it does not compound a factor
//!
//! `index` starts at `INDEX_SCALE` and **adds** `INDEX_SCALE × rate × dt / D` on every advance.
//! Settlement then charges `principal × (index − last_index) / INDEX_SCALE`. The divisor is a
//! constant, so the sum telescopes and the total over any span is exactly
//! `principal × rate × elapsed` — **independent of how often anybody calls anything.**
//!
//! # This was wrong until the vault became a market, and the market exposed it
//!
//! The earlier version multiplied: `index ← index × (1 + r·dt)`, and settlement divided by the
//! position's own `last_index`. In a single-customer vault those two always happened together —
//! every advance settled the only position — so the division cancelled the compounding exactly
//! and the total came out linear. The property held by coincidence.
//!
//! In a market it does not. **Another customer's transaction advances the index without settling
//! my position**, so my next settlement spans several index steps and pays the compounded
//! product rather than the linear sum:
//!
//! ```text
//! two steps, settled once:  P·((1 + r·dt₁)(1 + r·dt₂) − 1)  =  P·r·(dt₁+dt₂) + P·r²·dt₁·dt₂
//! two steps, settled twice: P·r·dt₁ + P·r·dt₂
//! ```
//!
//! The cross term is the whole problem. Two identical positions over identical spans owed
//! different amounts depending on how often each happened to settle — and since settling is
//! permissionless and free, a borrower could shave their own bill by calling `accrue` in a loop.
//! That is the same griefing hole the carried remainder closes, running in the opposite
//! direction.
//!
//! Making the index additive removes it at the root: there is no product to accumulate, so
//! cadence cannot matter. Every figure the tests pin is unchanged, because over a **single**
//! step the two forms are identical — which is exactly why a single-customer vault never
//! noticed.
//!
//! The trade is that this is simple interest at an annual rate, not continuous compounding. That
//! matches what the deck specifies (`rate_bps_year`, and no compounding formula anywhere) and
//! what `docs/04-interest.md` documents. A market that wants compounding needs the deck's
//! `debt_scaled` shape, where the debt *is* `scaled × index` and cadence is irrelevant for the
//! opposite reason — see decision Đ-06 for why that shape was not used.
//!
//! Interest is never folded into principal, so unpaid interest does not itself accrue.

use crate::errors::Error;
use crate::math;
use crate::types::*;
use soroban_sdk::Env;

/// `BPS × SECONDS_PER_YEAR` — the denominator of the rate.
fn denom() -> i128 {
    BPS * SECONDS_PER_YEAR as i128
}

/// Advance the global clock to `now`.
///
/// # Why the clock does not move when the index does not
///
/// Integer division truncates. If `last_accrual` advanced on a call that produced no index
/// movement, that slice of time would be gone for good, and anyone could hold the index still
/// by calling in a tight loop. Leaving `last_accrual` alone means the next call sees the full
/// elapsed span. The index is its own memory.
///
/// Returns whether the index actually moved — the caller emits `Accrued` on that, not on
/// `inc > 0`. A vault with no principal still advances its clock, and if that went unannounced
/// the `IndexState` could not be rebuilt from events alone.
pub fn advance(e: &Env, cfg: &Config, s: &mut IndexState) -> Result<bool, Error> {
    let now = e.ledger().timestamp();
    if now <= s.last_accrual {
        return Ok(false);
    }
    let dt = now - s.last_accrual;
    // INDEX_SCALE × rate × dt / D, rounded down. **Added**, not multiplied — see the module
    // note. The scale factor is the constant `INDEX_SCALE` rather than the running index, which
    // is what makes the accumulated total independent of how the interval was split.
    let step = math::mul_div_floor(
        e,
        INDEX_SCALE,
        math::checked_mul(cfg.rate_bps_year as i128, dt as i128)?,
        denom(),
    )?;
    let next = math::checked_add(s.index, step)?;

    if next > s.index {
        s.index = next;
        s.last_accrual = now;
        return Ok(true);
    }
    Ok(false)
}

/// Settle one position against the current index. Returns the interest added.
///
/// # Why the remainder is carried
///
/// `accrue` is permissionless. Without the carry, every call throws away its fraction: on a
/// $50 000 position accruing ~1585.7 units per second, rounding down loses 0.7 units per call,
/// and a year of per-second calls suppresses ~0.044% of the lender's revenue for the price of
/// network fees. Carrying bounds the cumulative error at **one smallest unit for the life of
/// the vault**.
///
/// # The `principal == 0` case
///
/// With no principal there is nothing to accrue, so the position is moved forward to the current
/// index and the remainder cleared. Leaving `last_index` stale would mean the first draw
/// afterwards is charged interest from an ancient index — and in a market vault that ancient
/// index may predate the customer's arrival by years. `storage::position` seeds the same field
/// the same way for the same reason.
pub fn settle(e: &Env, p: &mut Position, index: i128) -> Result<i128, Error> {
    if p.principal == 0 {
        p.last_index = index;
        p.accrual_rem = 0;
        return Ok(0);
    }
    if index <= p.last_index {
        return Ok(0);
    }
    let delta = math::checked_sub(index, p.last_index)?;
    // Divided by the **constant** `INDEX_SCALE`, not by `p.last_index`. Dividing by the running
    // index is what made the charge depend on settlement cadence; see the module note.
    let (inc, rem) = math::mul_add_div_rem(e, p.principal, delta, p.accrual_rem, INDEX_SCALE)?;

    p.accrual_rem = rem;
    p.interest = math::checked_add(p.interest, inc)?;
    p.last_index = index;
    Ok(inc)
}

/// Split a payment: **interest first, then principal**.
///
/// Interest first matches industry convention, so it surprises neither auditors nor users; it
/// drives `interest` to zero leaving clean state; and it makes "pay everything off" a single
/// call of `principal + interest`.
///
/// Returns `(interest_paid, principal_paid)`.
///
/// The subtraction is bare because `interest_paid = min(amount, interest)` bounds it: both
/// operands are non-negative and the result cannot leave `i128`.
pub fn split(amount: i128, p: &Position) -> (i128, i128) {
    let interest_paid = math::min_i128(amount, p.interest);
    (interest_paid, amount - interest_paid)
}
