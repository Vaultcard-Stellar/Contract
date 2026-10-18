//! The event catalogue. 21 events, topic 0 always `"vault"`.
//!
//! # Why these carry so many fields
//!
//! The architectural rule the whole system rests on is *"wipe the database, rebuild from ledger
//! 0, get the same result"*. That reads like a backend requirement; it is a hard requirement on
//! this file. The backend can only rebuild if **every** state change emits an event carrying
//! **enough** to reproduce it. One write path without an event, or one event missing a field,
//! and the test can never pass — and cannot be fixed on the backend side.
//!
//! So every event carries both the **delta** and the **resulting value**. `Repaid { amount }`
//! alone would force the backend to reimplement the allocation rule, and two implementations of
//! the same rule diverge on the day the reconciliation report goes red.
//!
//! # Rules
//!
//! - **Never change the fields of a deployed event.** Same rule as error codes. Need more data?
//!   Add a new event.
//! - **Emit at step 8** — before the transfer and before `bump`. Emitting after a transfer would
//!   let a hostile token interleave its own events and corrupt ordering.
//! - **Amounts are in the token's smallest unit**, never converted to USD. A USD value can be
//!   recomputed from a price; a token amount converted away cannot be recovered.
//! - **Every position-scoped event names its `owner`.** This is a market vault: an event that
//!   says a thousand dollars of principal now exists, without saying whose, is not a projection
//!   of anything. It is also how the indexer learns who the customers are — nothing enumerates
//!   them on chain, deliberately.
//!
//! # Why there are no configuration events
//!
//! `Config` is frozen at construction, `oracle` included, so `ConfigProposed`, `ConfigChanged`,
//! `OracleProposed` and `OracleChanged` have nothing to report. Their absence is the interface
//! saying that no address can change the terms of a position here. See `types::Config`.

use crate::types::Config;
use soroban_sdk::{contractevent, Address, BytesN};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Carries the **whole** config: a rebuild from ledger 0 needs the risk parameters to compute
/// LTV, and — now that nothing can ever change them — this is the *only* place they will ever
/// appear in the stream.
#[contractevent(topics = ["vault", "initialized"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Initialized {
    pub config: Config,
}

#[contractevent(topics = ["vault", "paused"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Paused {
    pub by: Address,
    pub at: u64,
    /// When this pause stops applying on its own. Carried so the indexer never has to know
    /// `MAX_PAUSE_SECONDS` — a constant it would otherwise have to keep in step with the
    /// contract, and would eventually get wrong.
    pub until: u64,
}

#[contractevent(topics = ["vault", "unpaused"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unpaused {
    pub at: u64,
}

// ---------------------------------------------------------------------------
// Deposit
// ---------------------------------------------------------------------------

#[contractevent(topics = ["vault", "deposited"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deposited {
    /// Whose position this is. Also the payer — `deposit` is authorised by the customer, so
    /// there is no third-party deposit path and no separate `from` to carry.
    pub owner: Address,
    pub amount: i128,
    pub deposit_after: i128,
}

/// `ltv_after_bps` is derivable and therefore redundant for a rebuild — but it is a cheap
/// cross-check: if the backend's computed LTV differs from the event's, one of the two has a
/// bug, and you find out immediately rather than at reconciliation.
#[contractevent(topics = ["vault", "withdrawn"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Withdrawn {
    /// Whose position this is.
    pub owner: Address,
    pub to: Address,
    pub amount: i128,
    pub deposit_after: i128,
    pub ltv_after_bps: u32,
}

// ---------------------------------------------------------------------------
// Authority
// ---------------------------------------------------------------------------

/// A customer names an operator and the terms. **Nothing is committed yet.**
///
/// The operator has not agreed to anything at this point — naming somebody is not the same as
/// them accepting — so no capital is reserved and no `committed_after` is carried. There is
/// nothing to report: it did not move.
///
/// This is the event an operator's own service watches to learn it has been offered a line.
/// Nothing else announces it: the contract has no notification mechanism and cannot have one.
#[contractevent(topics = ["vault", "grant_prop"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantProposed {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub settlement: Address,
    pub cap: i128,
    pub per_tx_limit: i128,
    pub expiry: u64,
    pub max_ltv_bps: u32,
}

/// The operator takes the line up, and **this** is where capital is committed.
///
/// Carries the whole terms again rather than only the delta. A reducer could join back to
/// `GrantProposed`, but the two may be days apart and the proposal may have been re-made in
/// between; carrying them makes this event sufficient on its own, which is the rule the rest of
/// this catalogue follows.
#[contractevent(topics = ["vault", "grant_acc"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantAccepted {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub settlement: Address,
    pub cap: i128,
    pub per_tx_limit: i128,
    pub expiry: u64,
    pub max_ltv_bps: u32,
    pub committed_after: i128,
}

/// The operator declines a proposal. Deletes it; moves no capital.
///
/// Separate from `GrantRevoked` despite deleting the same entry, and the reason is the same one
/// that keeps `GrantReaped` separate: the backend must be able to tell an operator saying no from
/// a customer changing their mind. One is a decision about the customer's terms — a cap above
/// what the operator will carry, a settlement address they do not control — and is worth showing
/// the customer as a reason. The other is not.
#[contractevent(topics = ["vault", "grant_rej"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantRejected {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub cap: i128,
    /// Unchanged. Carried so a reducer is told rather than left to reason that a rejection moved
    /// no capital.
    pub committed_after: i128,
}

/// `released` is the `committed` actually freed. It differs from `cap − drawn` on a second
/// call, where it is zero — carrying both lets the backend spot a duplicate without inferring it.
#[contractevent(topics = ["vault", "grant_rvk"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantRevoked {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub cap: i128,
    pub drawn: i128,
    pub released: i128,
    pub committed_after: i128,
}

/// Separate from `GrantRevoked` despite the identical effect: the backend needs to distinguish
/// an owner deliberately revoking — possibly an incident signal worth alerting on — from an
/// expired grant being tidied away.
#[contractevent(topics = ["vault", "grant_reap"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantReaped {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub cap: i128,
    pub drawn: i128,
    pub released: i128,
    pub committed_after: i128,
}

#[contractevent(topics = ["vault", "grant_frz"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantFrozen {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
    pub by: Address,
}

#[contractevent(topics = ["vault", "grant_unfz"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantUnfrozen {
    /// Whose position this is.
    pub owner: Address,
    pub operator: Address,
}

// ---------------------------------------------------------------------------
// Debt
// ---------------------------------------------------------------------------

/// `draw_id` is the join key back to the backend's `authorizations` row; without it the draw
/// lifecycle never closes.
///
/// `settlement` is derivable from `GrantAccepted`, but it is **audit evidence**: one event shows
/// where the money went, without joining two. Worth the bytes.
#[contractevent(topics = ["vault", "drawn"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Drawn {
    /// Whose position this is.
    pub owner: Address,
    pub draw_id: BytesN<32>,
    pub operator: Address,
    pub settlement: Address,
    pub amount: i128,
    pub drawn_after: i128,
    pub principal_after: i128,
    pub committed_after: i128,
    pub outstanding_after: i128,
}

#[contractevent(topics = ["vault", "repaid"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repaid {
    /// Whose position this is.
    pub owner: Address,
    pub from: Address,
    /// Which authority's line reopened, if the payer named one.
    ///
    /// The debt is the position's, but the headroom belongs to a grant, and a customer may hold
    /// several. Without this field `headroom_restored` says *how much* reopened and nothing says
    /// *whose* — so an indexer rebuilding from the ledger would have to guess, and the guess is
    /// wrong as soon as there are two grants. That breaks the rule the whole system rests on:
    /// wipe the database, rebuild from the event stream, get the same answer.
    ///
    /// `None` means the payer named nobody, and then `headroom_restored` is 0.
    pub operator: Option<Address>,
    pub amount: i128,
    pub interest_paid: i128,
    pub principal_paid: i128,
    pub headroom_restored: i128,
    pub principal_after: i128,
    pub interest_after: i128,
}

/// Emitted when the **index moves**, which is not the same as `amount > 0`.
///
/// The obvious rule — emit only when interest was actually added — leaves a vault with no
/// principal advancing its clock in silence, and `IndexState` then cannot be rebuilt from the
/// event stream at all. The rebuild test compares it field by field, so that gap would have to
/// be papered over by excluding the field, which is the kind of exclusion that hides real bugs.
///
/// The condition still skips the case worth skipping: accrual doubles as the TTL keep-alive, and
/// repeated calls inside one ledger move nothing and emit nothing.
///
/// `last_accrual` and `accrual_rem` are here for the same reason — so the accrual clock is
/// compared on every field rather than quietly excluded.
#[contractevent(topics = ["vault", "accrued"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accrued {
    /// Whose position this is.
    pub owner: Address,
    pub amount: i128,
    pub interest_after: i128,
    pub index: i128,
    pub last_accrual: u64,
    pub accrual_rem: i128,
}

// ---------------------------------------------------------------------------
// Capital
// ---------------------------------------------------------------------------

#[contractevent(topics = ["vault", "res_fund"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveFunded {
    pub amount: i128,
    pub total_after: i128,
}

#[contractevent(topics = ["vault", "res_wdraw"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveWithdrawn {
    pub amount: i128,
    pub total_after: i128,
    pub free_after: i128,
}

// ---------------------------------------------------------------------------
// Liquidation
// ---------------------------------------------------------------------------

/// One event with a `by_lender` flag rather than two: both routes have the same accounting
/// effect, and one event keeps the indexer simpler.
///
/// **No `headroom_restored`.** Liquidation restores none: a customer may hold several grants and
/// nobody on this path — least of all a liquidator who is a stranger to the customer's
/// arrangements — can say whose line a forced repayment should reopen. The field used to carry
/// the restored amount, and it is removed rather than pinned at zero, because a number that can
/// only be zero is a number somebody will eventually read as meaningful.
///
/// `ltv_before_bps` is audit evidence — it proves the position genuinely exceeded the threshold
/// at the moment of liquidation. Without it a dispute has no on-chain answer.
#[contractevent(topics = ["vault", "liquidated"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Liquidated {
    /// Whose position this is.
    pub owner: Address,
    pub liquidator: Address,
    pub by_lender: bool,
    pub repay_amount: i128,
    pub interest_paid: i128,
    pub principal_paid: i128,
    pub seized: i128,
    pub ltv_before_bps: u32,
    pub ltv_after_bps: u32,
    pub principal_after: i128,
    pub deposit_after: i128,
}

#[contractevent(topics = ["vault", "bad_debt"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BadDebtRealised {
    /// Whose position this is.
    pub owner: Address,
    pub lost_principal: i128,
    pub recovered: i128,
    pub bad_debt_total: i128,
    pub total_after: i128,
}

// ---------------------------------------------------------------------------
// Oracle guard
// ---------------------------------------------------------------------------

/// Entering an outage is a state change, so it emits — otherwise `OracleGuard.outage_at` would
/// be unreachable from the event stream and the rebuild test could not compare it.
#[contractevent(topics = ["vault", "orc_out"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleOutage {
    pub at: u64,
}

#[contractevent(topics = ["vault", "orc_recov"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleRecovered {
    pub grace_until: u64,
}
