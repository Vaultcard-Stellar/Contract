//! Typed accessors, durability classes, and TTL extension.
//!
//! Nothing here makes a decision — it reads and writes. Two pieces of judgement live in this
//! file and are argued where they sit: the `Position` default, which must not be
//! `Position::default()` (see [`position`]), and what [`bump`] is allowed to touch.
//!
//! # Shared state and per-customer state
//!
//! `Config`, `Paused`, `BorrowIndex`, `Reserve` and `OracleGuard` belong to the market. Every
//! customer reads the same ones. `Position` and `Grant` are keyed by the customer's address, and
//! **every accessor for them takes that address** — there is no way to reach a position without
//! naming whose it is, which is the property the whole multi-tenant design rests on.

use crate::errors::Error;
use crate::types::*;
use soroban_sdk::{Address, BytesN, Env, Vec};

// ---------------------------------------------------------------------------
// Instance — the market's identity and its brake
// ---------------------------------------------------------------------------

pub fn config(e: &Env) -> Result<Config, Error> {
    e.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(Error::NotInitialized)
}

/// Written **once**, by the constructor. There is no other call site, and adding one would give
/// the market an administrator it is designed not to have.
pub fn set_config(e: &Env, c: &Config) {
    e.storage().instance().set(&DataKey::Config, c);
}

/// When the brake was pulled, or `None`. Absence is the natural "never paused" — no sentinel
/// timestamp to mistake for a real one.
pub fn paused_at(e: &Env) -> Option<u64> {
    e.storage().instance().get(&DataKey::Paused)
}

/// Is the market paused **right now**?
///
/// A pause lapses on its own after `MAX_PAUSE_SECONDS`. Nobody has to call anything; the stored
/// timestamp simply stops applying.
///
/// Note what that does and does not bound: a pause nobody renews ends by itself, but `pause`
/// restarts the clock, so it is not a ceiling on the lender. See [`MAX_PAUSE_SECONDS`].
pub fn paused(e: &Env) -> bool {
    match paused_at(e) {
        None => false,
        Some(at) => e.ledger().timestamp() < at.saturating_add(MAX_PAUSE_SECONDS),
    }
}

pub fn set_paused_at(e: &Env, at: u64) {
    e.storage().instance().set(&DataKey::Paused, &at);
}

pub fn clear_pause(e: &Env) {
    e.storage().instance().remove(&DataKey::Paused);
}

// ---------------------------------------------------------------------------
// Persistent · shared by the whole market
// ---------------------------------------------------------------------------

/// The market's interest clock. Written at construction, so the fallback should be unreachable —
/// but a default of `INDEX_SCALE` (1.0) is the only safe value if it ever is reached, because a
/// zero index would make every settlement divide by zero.
pub fn index_state(e: &Env) -> IndexState {
    e.storage()
        .persistent()
        .get(&DataKey::BorrowIndex)
        .unwrap_or(IndexState {
            index: INDEX_SCALE,
            last_accrual: e.ledger().timestamp(),
        })
}

pub fn set_index_state(e: &Env, s: &IndexState) {
    e.storage().persistent().set(&DataKey::BorrowIndex, s);
}

pub fn reserve(e: &Env) -> Reserve {
    e.storage()
        .persistent()
        .get(&DataKey::Reserve)
        .unwrap_or(Reserve {
            total: 0,
            committed: 0,
            outstanding: 0,
            bad_debt: 0,
        })
}

pub fn set_reserve(e: &Env, r: &Reserve) {
    e.storage().persistent().set(&DataKey::Reserve, r);
}

pub fn oracle_guard(e: &Env) -> OracleGuard {
    e.storage()
        .persistent()
        .get(&DataKey::OracleGuard)
        .unwrap_or(OracleGuard {
            outage_at: 0,
            grace_until: 0,
        })
}

pub fn set_oracle_guard(e: &Env, g: &OracleGuard) {
    e.storage().persistent().set(&DataKey::OracleGuard, g);
}

// ---------------------------------------------------------------------------
// Persistent · one customer's own state
// ---------------------------------------------------------------------------

/// # The default is not `Position::default()`
///
/// A position appears the first time an address deposits, which in a market vault may be years
/// after the contract was deployed. Every field starts at zero except `last_index`, which
/// **must** start at the index as it stands *now*:
///
/// ```text
/// inc = (principal × (index − last_index) + accrual_rem) / INDEX_SCALE
/// ```
///
/// `last_index = 0` bills the newcomer against the full distance the index has travelled since
/// the market opened. `last_index = INDEX_SCALE` does the same thing wherever the index started
/// above 1.0, which is everywhere after the first accrual. Seeding from the live index makes the
/// delta zero, and it is why this getter takes `&Env` rather than being a `Default` impl.
///
/// The divisor is the constant `INDEX_SCALE`, **not** `last_index` — see `interest::settle`. An
/// earlier draft of this comment said otherwise; the index is additive, so dividing by
/// `last_index` would make interest depend on when the market opened.
pub fn position(e: &Env, owner: &Address) -> Position {
    e.storage()
        .persistent()
        .get(&DataKey::Position(owner.clone()))
        .unwrap_or_else(|| Position {
            deposit: 0,
            principal: 0,
            interest: 0,
            last_index: index_state(e).index,
            accrual_rem: 0,
        })
}

pub fn set_position(e: &Env, owner: &Address, p: &Position) {
    e.storage()
        .persistent()
        .set(&DataKey::Position(owner.clone()), p);
}

pub fn grant(e: &Env, owner: &Address, operator: &Address) -> Option<Grant> {
    e.storage()
        .persistent()
        .get(&DataKey::Grant(owner.clone(), operator.clone()))
}

pub fn set_grant(e: &Env, owner: &Address, operator: &Address, g: &Grant) {
    e.storage()
        .persistent()
        .set(&DataKey::Grant(owner.clone(), operator.clone()), g);
}

/// The operators this customer has live grants with.
///
/// Bounded by `MAX_GRANTS_PER_OWNER`. Read on exactly two paths — adding a grant, and releasing
/// every grant's capital on insolvency — and never on the money path, where `committed` is
/// maintained incrementally as it always was.
pub fn grant_operators(e: &Env, owner: &Address) -> Vec<Address> {
    e.storage()
        .persistent()
        .get(&DataKey::GrantOperators(owner.clone()))
        .unwrap_or_else(|| Vec::new(e))
}

pub fn set_grant_operators(e: &Env, owner: &Address, ops: &Vec<Address>) {
    let key = DataKey::GrantOperators(owner.clone());
    if ops.is_empty() {
        // Removed rather than stored empty. An empty vector is an entry with a TTL that has to be
        // extended forever for a customer who has no grants.
        e.storage().persistent().remove(&key);
    } else {
        e.storage().persistent().set(&key, ops);
    }
}

/// Revocation and reaping **delete** the entry rather than flagging it.
///
/// Keeping a tombstone would mean making the `committed` release idempotent against a flag that
/// outlives the grant — more state to get wrong than it saves. History lives in the
/// `GrantRevoked` event, which is the right place for it. This is also why code
/// `32 GrantRevoked` is retired: there is no revoked state left to report, so a draw afterwards
/// hits `30 GrantNotFound`.
pub fn remove_grant(e: &Env, owner: &Address, operator: &Address) {
    e.storage()
        .persistent()
        .remove(&DataKey::Grant(owner.clone(), operator.clone()));
}

// ---------------------------------------------------------------------------
// Temporary — replay guard
// ---------------------------------------------------------------------------

/// **Not** keyed by owner. A `draw_id` is unique across the whole market — the backend derives it
/// as `sha256(auth_id ‖ vault)` — and scoping the guard per customer would let the same id be
/// replayed against a different position.
pub fn draw_seen(e: &Env, id: &BytesN<32>) -> bool {
    e.storage().temporary().has(&DataKey::DrawSeen(id.clone()))
}

/// Written once and never extended. Temporary entries that expire are gone for good, which is
/// exactly why `DRAW_SEEN_TTL` is 30 days and not the deck's 24 hours.
pub fn mark_draw_seen(e: &Env, id: &BytesN<32>) {
    let k = DataKey::DrawSeen(id.clone());
    e.storage().temporary().set(&k, &true);
    e.storage()
        .temporary()
        .extend_ttl(&k, DRAW_SEEN_TTL, DRAW_SEEN_TTL);
}

// ---------------------------------------------------------------------------
// TTL
// ---------------------------------------------------------------------------

/// Step 10 of every write function that touches a position.
///
/// Persistent entries archive when their TTL runs out, and an archived `Position` means that
/// customer cannot withdraw. The failure needs no bug and no attacker: deposit, borrow, repay in
/// full, walk away for eight months.
///
/// # It touches five keys, never more
///
/// The single-customer version extended every persistent key it knew about. That cannot survive
/// multi-tenancy — a market with ten thousand positions would do ten thousand extensions on
/// every write, and the cost would grow with the customer base until nobody could afford to
/// transact.
///
/// So it extends the market's shared state plus **the one customer this call touched**. Every
/// other position is refreshed by its own owner's next call, and `accrue(owner)` is
/// permissionless partly so that anyone can do it for them cheaply.
///
/// The `has()` guard keeps absent keys free. `DrawSeen` is deliberately absent: it is Temporary
/// and gets its TTL once, at `mark_draw_seen`.
pub fn bump(e: &Env, owner: &Address) {
    bump_shared(e);

    for key in [
        DataKey::Position(owner.clone()),
        DataKey::GrantOperators(owner.clone()),
    ] {
        if e.storage().persistent().has(&key) {
            e.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
        }
    }

    // Every grant this customer holds, which is why the count is bounded: an unbounded list would
    // make the cost of every write grow with the number of providers the customer has authorised.
    // A grant whose entry archives is capital nobody can release, so skipping any of them here
    // would trade a bounded cost now for capital locked forever later.
    for operator in grant_operators(e, owner).iter() {
        let key = DataKey::Grant(owner.clone(), operator.clone());
        if e.storage().persistent().has(&key) {
            e.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
        }
    }
}

/// The market's shared state only — for the write paths that touch no position.
pub fn bump_shared(e: &Env) {
    e.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND);

    for key in [DataKey::BorrowIndex, DataKey::Reserve, DataKey::OracleGuard] {
        if e.storage().persistent().has(&key) {
            e.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
        }
    }
}

// ---------------------------------------------------------------------------
// Role checks
// ---------------------------------------------------------------------------

/// `by` must be this position's own customer or the lender, and must have authorised.
///
/// Used by `freeze_grant` — risk-reducing, so whichever party notices trouble first can act
/// without waiting for the other. Note *this position's* customer: in a market vault one
/// customer has no say whatever over another's grant.
pub fn require_owner_or_lender(cfg: &Config, owner: &Address, by: &Address) -> Result<(), Error> {
    if by != owner && by != &cfg.lender {
        return Err(Error::Unauthorized);
    }
    by.require_auth();
    Ok(())
}
