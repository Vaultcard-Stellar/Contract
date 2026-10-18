//! Every money computation in the contract goes through this module.
//!
//! Two rules the rest of the codebase depends on:
//!
//! 1. **No bare `*` or `/` on the money path outside this file.** It is a grep-able review rule
//!    (`docs/07-invariants-and-security.md`), not something to remember.
//! 2. **Every `a * b / c` uses a 256-bit intermediate.** `qty × price` overflows `i128` well
//!    inside plausible balances once token and price decimals are both large.
//!
//! `overflow-checks = true` in the release profile is the backstop behind this, not the
//! mechanism — an overflow panic yields an opaque error, whereas these return a named code.

use crate::errors::Error;
use soroban_sdk::{Env, U256};

/// `a * b / d`, truncated toward zero.
pub fn mul_div_floor(e: &Env, a: i128, b: i128, d: i128) -> Result<i128, Error> {
    let (n, den) = prepare(e, a, b, d)?;
    to_i128(n.div(&den))
}

/// `a * b / d`, rounded away from zero.
///
/// Implemented as `(a*b + d - 1) / d` in 256-bit space, so the `+ d - 1` cannot itself overflow.
pub fn mul_div_ceil(e: &Env, a: i128, b: i128, d: i128) -> Result<i128, Error> {
    let (n, den) = prepare(e, a, b, d)?;
    let one = U256::from_u128(e, 1);
    // n + d - 1
    let bumped = n.add(&den).sub(&one);
    to_i128(bumped.div(&den))
}

/// `a * b / d` where the caller picks the direction. Convenience for the many places that carry
/// a rounding decision from the table in `docs/03-credit-math.md`.
pub fn mul_div(e: &Env, a: i128, b: i128, d: i128, up: bool) -> Result<i128, Error> {
    if up {
        mul_div_ceil(e, a, b, d)
    } else {
        mul_div_floor(e, a, b, d)
    }
}

/// `10^n` as `i128`.
///
/// Capped at 38 because `10^39` exceeds `i128::MAX`. Anything asking for more is a decimals
/// value that cannot be real.
pub fn pow10(n: u32) -> Result<i128, Error> {
    if n > 38 {
        return Err(Error::MathOverflow);
    }
    let mut r: i128 = 1;
    for _ in 0..n {
        r = r.checked_mul(10).ok_or(Error::MathOverflow)?;
    }
    Ok(r)
}

/// Rescale `v` by `10^shift`, dividing when `shift` is positive and multiplying when it is
/// negative.
///
/// **Both branches honour `up`.** This is the classic place to implement one direction
/// correctly and silently round the other one down — the negative branch is exercised whenever
/// `token_decimals + price_decimals < USD_DECIMALS`.
pub fn scale(e: &Env, v: i128, shift: i32, up: bool) -> Result<i128, Error> {
    if v < 0 {
        return Err(Error::MathOverflow);
    }
    match shift {
        0 => Ok(v),
        s if s > 0 => {
            let d = pow10(s as u32)?;
            mul_div(e, v, 1, d, up)
        }
        s => {
            let m = pow10((-s) as u32)?;
            // Exact: multiplication cannot need rounding.
            mul_div(e, v, m, 1, up)
        }
    }
}

pub fn checked_add(a: i128, b: i128) -> Result<i128, Error> {
    a.checked_add(b).ok_or(Error::MathOverflow)
}

pub fn checked_sub(a: i128, b: i128) -> Result<i128, Error> {
    a.checked_sub(b).ok_or(Error::MathOverflow)
}

pub fn checked_mul(a: i128, b: i128) -> Result<i128, Error> {
    a.checked_mul(b).ok_or(Error::MathOverflow)
}

pub fn min_i128(a: i128, b: i128) -> i128 {
    if a < b {
        a
    } else {
        b
    }
}

/// Compare `a * b` against `c * d` without dividing.
///
/// Every gate uses this instead of computing an LTV. `ltv_bps` rounds up by as much as a basis
/// point — on a 5000 bps ceiling against $57 200 of eligible value that is roughly $5.72 of
/// borrowing power, the coarsest rounding effect anywhere in the contract, and it is entirely
/// avoidable by not dividing.
pub fn cross_le(e: &Env, a: i128, b: i128, c: i128, d: i128) -> Result<bool, Error> {
    let lhs = wide(e, a, b)?;
    let rhs = wide(e, c, d)?;
    Ok(lhs <= rhs)
}

/// `a * b >= c * d`, no division. See [`cross_le`].
pub fn cross_ge(e: &Env, a: i128, b: i128, c: i128, d: i128) -> Result<bool, Error> {
    let lhs = wide(e, a, b)?;
    let rhs = wide(e, c, d)?;
    Ok(lhs >= rhs)
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn wide(e: &Env, a: i128, b: i128) -> Result<U256, Error> {
    if a < 0 || b < 0 {
        return Err(Error::MathOverflow);
    }
    Ok(U256::from_u128(e, a as u128).mul(&U256::from_u128(e, b as u128)))
}

fn prepare(e: &Env, a: i128, b: i128, d: i128) -> Result<(U256, U256), Error> {
    if d <= 0 {
        return Err(Error::MathOverflow);
    }
    let n = wide(e, a, b)?;
    Ok((n, U256::from_u128(e, d as u128)))
}

/// Two-stage narrowing.
///
/// `U256` has no `to_i128`, and `to_u128` only proves the value fits 128 **unsigned** bits — so
/// the explicit `> i128::MAX` check is required, not belt-and-braces.
fn to_i128(v: U256) -> Result<i128, Error> {
    let u = v.to_u128().ok_or(Error::MathOverflow)?;
    if u > i128::MAX as u128 {
        return Err(Error::MathOverflow);
    }
    Ok(u as i128)
}

/// `(a * b * 10^shift) / d`, with `shift` allowed to be negative.
///
/// The whole expression is evaluated in 256-bit space, so no intermediate can overflow even
/// when `shift` is large — the case that made a stepwise `i128` implementation impossible for
/// `liq_price`, where the numerator can reach ~1e57 before any division happens.
pub fn mul_scaled_div(
    e: &Env,
    a: i128,
    b: i128,
    shift: i32,
    d: i128,
    up: bool,
) -> Result<i128, Error> {
    if d <= 0 {
        return Err(Error::MathOverflow);
    }
    let mut n = wide(e, a, b)?;
    let mut den = U256::from_u128(e, d as u128);

    match shift {
        0 => {}
        s if s > 0 => n = n.mul(&pow10_u256(e, s as u32)?),
        s => den = den.mul(&pow10_u256(e, (-s) as u32)?),
    }
    div_round(n, den, up)
}

/// `((a * b) + c) / d` together with `((a * b) + c) % d`.
///
/// One call because interest settlement needs both halves and must not compute the product
/// twice. The remainder is what bounds the cumulative rounding error at a single smallest unit
/// for the life of the vault — discard it and anyone can suppress the lender's revenue by
/// calling `accrue` in a loop.
pub fn mul_add_div_rem(e: &Env, a: i128, b: i128, c: i128, d: i128) -> Result<(i128, i128), Error> {
    if c < 0 || d <= 0 {
        return Err(Error::MathOverflow);
    }
    let n = wide(e, a, b)?.add(&U256::from_u128(e, c as u128));
    let den = U256::from_u128(e, d as u128);
    let q = to_i128(n.div(&den))?;
    // r < d <= i128::MAX, so this narrowing is always safe.
    let r = to_i128(n.rem_euclid(&den))?;
    Ok((q, r))
}

fn pow10_u256(e: &Env, n: u32) -> Result<U256, Error> {
    // 10^78 is the ceiling of U256; nothing legitimate comes close, and a larger exponent means
    // a decimals value that cannot be real.
    if n > 60 {
        return Err(Error::MathOverflow);
    }
    let ten = U256::from_u32(e, 10);
    Ok(ten.pow(n))
}

fn div_round(n: U256, den: U256, up: bool) -> Result<i128, Error> {
    let q = n.div(&den);
    if !up {
        return to_i128(q);
    }
    if n.rem_euclid(&den) == U256::from_u32(q.env(), 0) {
        to_i128(q)
    } else {
        to_i128(q.add(&U256::from_u32(q.env(), 1)))
    }
}
