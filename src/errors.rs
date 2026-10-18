//! The frozen error table. No logic.
//!
//! **A code number never changes after the first public testnet deployment.** The backend
//! branches on these numbers (see `docs/09-backend-integration.md`) — code 33 means *halt the
//! vault*, code 36 means *not an error at all*. Renumbering silently changes what an operator
//! does at three in the morning.
//!
//! New codes take unused numbers. Retired codes leave their number **permanently vacant**;
//! reassigning a code's meaning is more dangerous than reassigning its number, because an old
//! backend keeps the old interpretation.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    // ---- 1–9 · system ----
    /// A call reached an address that is not an initialised vault.
    ///
    /// Unreachable in practice: `__constructor` runs inside the deploy transaction, so no
    /// uninitialised vault ever exists. Retained for calls that land on a non-vault address.
    NotInitialized = 1,
    /// Wrong role on a *manual* role check — `freeze_grant` by an address that is neither that
    /// customer nor the lender. `require_auth` covers the rest.
    Unauthorized = 2,
    /// The constructor, or `propose_config`, rejected the configuration.
    InvalidConfig = 3,
    /// A multiply/divide exceeded `i128`, or was handed a negative operand or a divisor <= 0.
    /// One code for all three: they all mean a bug upstream.
    MathOverflow = 4,
    /// I1, I3′ or I4 broke. **Must never appear.** If it does, halt and investigate — do not
    /// retry. Covered by the property tests rather than by a test that triggers it.
    InvariantViolated = 5,
    // 6–9 vacant.

    // ---- 10–19 · position ----
    /// Withdrawing more deposit than the position holds.
    InsufficientDeposit = 10,
    /// Exceeds the **protocol** LTV ceiling. Distinct from 35, which is the grant's own ceiling.
    HealthTooLow = 11,
    /// `amount <= 0`, a repayment above the outstanding principal, or a lender liquidation
    /// above the debt.
    InvalidAmount = 12,
    /// A risk-increasing function while paused.
    Paused = 13,
    /// Liquidating or repaying a position with no debt. Distinct from 50: "nothing to do"
    /// rather than "not yet past the threshold".
    NoDebt = 14,
    // 15–19 vacant.

    // ---- 20–29 · capital ----
    /// Granting beyond `reserve.free` — rule R2.
    ReserveUnderfunded = 20,
    /// The lender tried to withdraw below `committed + outstanding`.
    ReserveLocked = 21,
    /// Beyond `vault_cap`.
    ExposureCapExceeded = 22,
    // 23–29 vacant. Reserved for a future velocity limit — see docs/07, threat T1.

    // ---- 30–39 · authority ----
    /// No grant, or the caller does not match `grant.operator`.
    GrantNotFound = 30,
    /// `now >= grant.expiry`.
    GrantExpired = 31,
    // 32 RETIRED (was `GrantRevoked`). Revocation deletes the entry, so a draw after
    // revocation hits 30. **Never reuse 32.**
    /// `drawn + amount > cap`.
    CapExceeded = 33,
    /// `amount > per_tx_limit`.
    PerTxLimitExceeded = 34,
    /// A **draw** would exceed the grant's own LTV ceiling. Not the same as 46.
    LtvCeilingExceeded = 35,
    /// `draw_id` already used. Backend treats this as success, not failure.
    DuplicateDrawId = 36,
    /// `cap <= 0`, `per_tx > cap`, expiry in the past or beyond `MAX_GRANT_SECONDS`,
    /// or `settlement == operator`.
    InvalidGrantParams = 37,
    /// Granting while a live grant exists. Closes the `drawn`-reset hole.
    GrantExists = 38,
    /// The grant is frozen.
    GrantFrozen = 39,

    // ---- 40–49 · price and lifecycle ----
    /// `now - timestamp > price_max_age`.
    PriceStale = 40,
    /// No price, `price <= 0`, a timestamp more than `FUTURE_PRICE_SLACK` in the future, or a
    /// proposed oracle that fails its liveness check.
    PriceInvalid = 41,
    /// More than `MAX_GRANTS_PER_OWNER` live grants.
    ///
    /// The bound exists because insolvency releases every grant's committed capital in one call,
    /// and an unbounded list makes that call's cost something the customer chooses — a position
    /// nobody can liquidate. Revoke one to make room.
    TooManyGrants = 47,
    /// `reap_expired` before expiry.
    ///
    /// Sits in the price band only because 30–39 is full; preserving the deck's numbering
    /// matters more than a tidy table.
    GrantNotExpired = 42,
    // 43 RETIRED (was `OracleUnchanged`), 44 RETIRED (was `ConfigWouldTighten`),
    // 45 RETIRED (was `ConfigNotReady`).
    //
    // All three belonged to the governance paths — `propose_config` / `accept_config` /
    // `propose_oracle` / `accept_oracle` — which a market vault cannot have. A parameter path
    // needs somebody to sign for the positions it affects, and in a market there are thousands
    // of them; the only alternative is a privileged address that can move a threshold under
    // other people's collateral. `Config` is frozen at construction instead, `oracle` included.
    //
    // **Never reuse 43, 44 or 45.** A backend built against the earlier single-customer
    // contract still holds their old meanings.
    /// `grant_authority` where the proposed grant's `max_ltv_bps` exceeds the market ceiling.
    ///
    /// Split from 35: that is a draw exceeding terms the customer set; this is the customer
    /// proposing terms above what the market allows. Opposite comparison, different actor.
    GrantCeilingTooHigh = 46,
    /// `draw`, `freeze_grant` or `unfreeze_grant` against a grant the operator has not accepted.
    ///
    /// Distinct from `30 GrantNotFound`, and the distinction matters to whoever is reading the
    /// refusal: 30 means the customer never authorised you (or revoked), 48 means they did and
    /// you have not taken it up. The remedy for one is to ask the customer; for the other it is
    /// to call `accept_grant`.
    GrantNotAccepted = 48,
    /// `accept_grant` or `reject_grant` against a grant already accepted.
    ///
    /// Acceptance is not idempotent, deliberately: it commits capital, so running it twice would
    /// commit twice for one line.
    GrantAlreadyAccepted = 49,

    // ---- 50–59 · liquidation ----
    /// `ltv < liq_threshold` — the position is healthy, or (for `lender_liquidate`) insolvent
    /// and therefore out of bounds for that route.
    PositionHealthy = 50,
    /// `repay > max_repay`.
    CloseFactorExceeded = 51,
    /// `seize < min_deposit_out`.
    SlippageExceeded = 52,
    /// Within the grace window after oracle recovery.
    LiquidationGrace = 53,
    // 54–59 vacant.
}
