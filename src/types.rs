//! Constants, structs, enums, storage keys. **No logic.**

use soroban_sdk::{contracttype, Address, BytesN, Symbol};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Basis points. All ratios are expressed in these; no floating point anywhere.
pub const BPS: i128 = 10_000;
/// The same value where a `u32` is wanted (config fields, comparisons).
pub const BPS_U32: u32 = 10_000;

/// The internal USD scale. Every value crossing into `health` is normalised to this.
///
/// 7 is chosen because it matches Stellar's default and USDC's, so `debt_value_usd` is the
/// identity on the path that matters most — the one with the fewest chances to be wrong.
pub const USD_DECIMALS: u32 = 7;

/// Scale of `IndexState.index`. `index == INDEX_SCALE` means 1.0.
///
/// 1e18 is large enough that at 10%/yr the index moves ~3.17e9 units per second, far above the
/// rounding unit, so the clock advances on virtually every call.
pub const INDEX_SCALE: i128 = 1_000_000_000_000_000_000;

pub const SECONDS_PER_YEAR: u64 = 31_536_000;

/// Ledgers per day at ~5s each.
pub const DAY: u32 = 17_280;
/// Extend TTL only when it drops below this. Avoids rewriting TTL on every transaction.
pub const TTL_THRESHOLD: u32 = 30 * DAY;
/// Extend to this. 120 days.
pub const TTL_EXTEND: u32 = 120 * DAY;
/// Replay guard lifetime. **Not 24h** — the backend's outbox has an `unknown` state whose exit
/// is a human, and humans have weekends. See `docs/09-backend-integration.md`.
pub const DRAW_SEEN_TTL: u32 = 30 * DAY;

/// Maximum grant lifetime, in **SECONDS**.
///
/// This bound exists so a grant can never outlive the TTL of its own storage entry — if it did,
/// the entry could archive while the grant is live, `revoke_authority` could not read it, and
/// `committed` would be locked forever.
///
/// Do **not** rewrite this as a comparison against `TTL_EXTEND`: that is a ledger count, this is
/// seconds, and the two only look compatible because of the ~5s/ledger factor.
/// The relationship is `90 days of seconds  <  120 days of ledgers`.
pub const MAX_GRANT_SECONDS: u64 = 90 * 24 * 60 * 60;

/// How many providers one customer may authorise at once.
///
/// The bound exists because insolvency has to release every grant's `committed` in one call, and
/// an unbounded list makes that call's cost something the customer chooses. A position that cannot
/// be liquidated because its owner authorised a thousand operators is a position the lender can
/// never recover from — the gas bomb Đ-02 refused, and it is refused here too, by a constant
/// rather than by allowing only one.
///
/// Eight is enough for electricity, water, gas, telecoms, a card, a public-services portal and two
/// spare. A customer who needs a ninth revokes one.
pub const MAX_GRANTS_PER_OWNER: u32 = 8;

/// How long liquidation stays blocked after the oracle recovers from an outage, in seconds.
///
/// A **code constant, not configuration** — so nobody can set it to zero after deployment.
pub const LIQUIDATION_GRACE: u64 = 1_800;

/// Clock skew tolerated between the feed and the ledger before a price counts as invalid.
pub const FUTURE_PRICE_SLACK: u64 = 60;

/// How long a pause may last before it lapses on its own, in seconds.
///
/// In a single-customer vault the brake was safe because it was asymmetric: either party could
/// pull it, but both had to agree to release it, so neither could hold the other's collateral
/// hostage. A market vault has no such pair — thousands of customers cannot co-sign — so only
/// the lender is left, and an unbounded lender-only pause would let one address freeze every
/// customer's withdrawal for as long as it liked.
///
/// Seven days turns that into an incident window — but it bounds **inattention, not the lender**.
/// `pause` restarts the clock, so a lender who means to keep the market shut can renew weekly.
/// What the expiry guarantees is that a pause nobody renews lapses on its own, with no
/// transaction and no counterparty. The residual power is accepted and recorded; see the risk
/// register in `docs/13-deck-conformance.md`.
pub const MAX_PAUSE_SECONDS: u64 = 7 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Oracle wire types — must match the feed's XDR shape exactly (SEP-40)
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// **Frozen at construction. Every field. There is no setter for any of them.**
///
/// # One market, one collateral asset, many customers
///
/// This contract is a *market vault*: **one instance per (`deposit_token`, `debt_token`) pair**,
/// shared by every customer who deposits that collateral. BTC/USDC is one market; BTC/USDT is a
/// different one, with its own reserve, its own clock and its own customers.
///
/// There is no `owner` field, because there is no single customer — each holds a [`Position`]
/// keyed by their own address. `lender` is the party that funds the shared [`Reserve`], not an
/// administrator.
///
/// # Why nothing here can change
///
/// A single-customer vault could safely carry a loosen-only parameter path signed by the
/// customer and the lender together, because the customer whose position was at stake had to
/// sign. In a market vault there is no such counterparty — thousands of customers cannot
/// co-sign — so any mutable parameter would need a privileged address, and that address could
/// move a threshold and put other people's collateral into liquidation.
///
/// Freezing everything removes the role entirely. **No address on earth can change the terms of
/// a position in this contract.** Recalibrating risk parameters, or following a price feed that
/// moved, means deploying a new market and inviting customers to migrate — which they do by
/// repaying and withdrawing, a path that works even when this market's oracle is dead.
///
/// `oracle` is frozen for the same reason and at a real cost: if the feed dies permanently,
/// `draw` and `liquidate` stop working here forever. What keeps that survivable is that `repay`
/// reads no price and `withdraw` reads none once the debt is clear, so the market can always
/// wind itself down. The lender carries the loss on anyone who declines to repay. See
/// `docs/06-oracle.md`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Funds the shared reserve. **Not an administrator** — cannot change a parameter, cannot
    /// touch a position except through `lender_liquidate`, which obeys the same gates everyone
    /// else obeys.
    pub lender: Address,

    /// SEP-41 token put up as collateral.
    pub deposit_token: Address,
    /// Read **from the token contract** at construction, never trusted from the caller.
    pub deposit_decimals: u32,
    /// SEP-40 lookup key. **Not** the token address — Reflector keys on `Other("BTC")`.
    pub deposit_feed: Asset,

    /// SEP-41 token borrowed. Assumed worth exactly 1 USD.
    pub debt_token: Address,
    /// Read **from the token contract** at construction.
    pub debt_decimals: u32,

    pub oracle: Address,

    /// Discount applied to market value before anything may be borrowed against it.
    pub haircut_bps: u32,
    pub max_ltv_bps: u32,
    pub liq_threshold_bps: u32,
    /// At or above this LTV the close factor is dropped and the whole debt may be closed.
    /// Without it, a partial liquidation above `1/(1+bonus)` *raises* LTV and the position
    /// spirals. The constructor enforces the break-even relationship.
    pub full_liq_ltv_bps: u32,
    pub liq_bonus_bps: u32,
    pub close_factor_bps: u32,
    pub price_max_age: u64,

    pub rate_bps_year: u32,
    /// Ceiling on `committed + outstanding` for the **whole market**, across every customer.
    /// **USD scale (7 decimals)** — the same units as `dust_threshold`, and *not* the units the
    /// reserve is kept in.
    ///
    /// `Reserve` counts debt-token units, so the gate rescales the exposure before comparing.
    /// The two are the same number only when `debt_decimals == 7`; comparing them raw moved the
    /// market's ceiling by a power of ten for any other token, in silence, because the books
    /// stay internally consistent either way. Every threshold in this struct is a USD figure —
    /// that consistency is the point.
    ///
    /// There is deliberately no per-customer ceiling: how the market's capacity is allocated
    /// between customers is the backend's policy, and a limit the contract cannot see the
    /// reason for is a limit it will eventually enforce wrongly.
    pub vault_cap: i128,
    /// Below this debt, a full close is allowed regardless of LTV, so no liquidator is left
    /// staring at a $3 remainder. **USD scale (7 decimals).**
    pub dust_threshold: i128,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The interest clock. Monotonic, shared by nothing — this vault has one position.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexState {
    /// Scaled by `INDEX_SCALE`. Starts at 1.0.
    pub index: i128,
    pub last_accrual: u64,
}

/// One customer's stake in the market. Keyed by their address — see [`DataKey::Position`].
///
/// # The default must carry a live `last_index`
///
/// No position is written at construction; one appears the first time that address deposits,
/// which may be years after the market opened. A `last_index` of `0` would make the first
/// settlement divide by zero, and a `last_index` of `INDEX_SCALE` would charge the newcomer
/// every hour of interest the market had ever accrued. `storage::position()` therefore seeds
/// the default from the **current** index. See `storage.rs`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Position {
    /// Deposit-token smallest units.
    pub deposit: i128,
    /// Debt-token smallest units. A plain integer — never multiplied or divided, which is what
    /// keeps invariant I3 exactly true rather than approximately true.
    pub principal: i128,
    /// Accrued, unpaid. Debt-token smallest units.
    pub interest: i128,
    /// Index value at the last settlement.
    pub last_index: i128,
    /// Carried remainder of the settlement division. Never discarded — this is what bounds the
    /// cumulative rounding error at one smallest unit for the life of the vault, and what stops
    /// anyone suppressing the lender's revenue by spamming `accrue`.
    pub accrual_rem: i128,
}

/// The lender's capital, **shared by every customer in the market**.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reserve {
    /// The lender's claim: funded − withdrawn + interest collected − principal written off.
    /// Not the token balance; the balance is lower by `outstanding`.
    pub total: i128,
    /// Promised to live grants and not yet drawn, summed over **all** customers. This is what
    /// makes the deck's promise true — a card transaction is never declined because somebody
    /// else borrowed first, because their line was funded the moment it was granted.
    pub committed: i128,
    /// **PRINCIPAL** disbursed and unpaid, summed over all customers. Never interest —
    /// including interest here would break invariant I1 from the passage of time alone.
    ///
    /// The per-customer equality `outstanding == position.principal` that held when this vault
    /// served one customer is now `outstanding == Σ principal`, which no single call can check.
    /// Invariant I3 therefore moved to its differential form; see `invariant.rs`.
    pub outstanding: i128,
    /// Cumulative written-off principal. Increase only; there is no on-chain write-off path.
    pub bad_debt: i128,
}

impl Reserve {
    /// Capital the lender may still commit or withdraw.
    ///
    /// One definition, never recomputed inline — it is a gate operand, an event field and a
    /// view field, and three copies would eventually disagree.
    ///
    /// The arithmetic stays bare, and that is safe rather than sloppy: invariant I1 holds at the
    /// end of every write, so `total >= committed + outstanding`, and the expression evaluates
    /// left to right through two non-negative intermediates. Nothing here can leave `i128`.
    pub fn free(&self) -> i128 {
        self.total - self.committed - self.outstanding
    }
}

/// One customer's spending authority. Keyed by the customer — see [`DataKey::Grant`].
///
/// `operator` stays a field rather than moving into `Config`, even though in practice every
/// grant in a market will name the same platform key. The customer signs *their own* terms:
/// their cap, their per-transaction limit, their expiry, their settlement address. A compromised
/// operator key can therefore reach every customer who authorised it, but only up to each one's
/// own cap and only into each one's own pinned address.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub operator: Address,
    /// **PINNED at grant time. There is no setter.**
    ///
    /// `draw` takes no destination parameter and reads this instead, so a compromised operator
    /// can still only move funds to the address the owner signed for. Changing it means
    /// revoking and re-granting.
    pub settlement: Address,
    pub cap: i128,
    /// Principal created under this grant and still outstanding. Falls as principal is repaid —
    /// the line revolves.
    pub drawn: i128,
    pub per_tx_limit: i128,
    /// Unix seconds. Bounded by `MAX_GRANT_SECONDS` so the grant cannot outlive its own storage.
    pub expiry: u64,
    pub max_ltv_bps: u32,
    /// Blocks `draw` without destroying the grant. The blunt alternative — revoke — forces the
    /// owner to re-sign from scratch, and a user made to do that over false alarms stops
    /// responding to alarms.
    pub frozen: bool,
    /// Whether the operator has taken this authority up.
    ///
    /// A grant is created **pending**. The owner names an operator, but naming somebody is not
    /// the same as them agreeing to be named, and two things only the operator knows make the
    /// difference:
    ///
    /// - **The blast radius of their own key.** The sum of caps granted to an operator is what
    ///   moves if that key is stolen. Letting customers enlarge it unilaterally lets them
    ///   enlarge the target on somebody else's key.
    /// - **`settlement` is chosen by the owner**, and the contract can only check that it is not
    ///   the operator themselves. Whether it is an address the operator actually controls is
    ///   unknowable here — there is no registry on chain and there cannot be one without
    ///   governance. An operator that draws against a grant settling somewhere they do not
    ///   control has delivered goods and been paid to a stranger.
    ///
    /// So the operator checks the terms and calls `accept_grant`, and **only then** is capital
    /// committed from the reserve. A pending grant reserves nothing, blocks nothing, and can be
    /// revoked by the owner or rejected by the operator at any time.
    pub accepted: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleGuard {
    /// When the feed was first seen unusable. `0` means no outage on record.
    pub outage_at: u64,
    /// Liquidation is blocked until this timestamp, giving the owner a chance to react after a
    /// price gap. `0` means no grace window.
    pub grace_until: u64,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// Three of these are keyed by customer. The rest are the market's own state, shared by
/// everyone in it.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Instance. Read in nearly every call, and never written after construction.
    Config,
    /// Instance. `Option<u64>` — the timestamp the brake was pulled, or `None`. Split from
    /// `Config` because `Config` is frozen and this is not.
    Paused,
    /// Persistent. **Shared.** One interest clock for the whole market; each position settles
    /// against it from its own `last_index`.
    BorrowIndex,
    /// Persistent. **Shared.** The lender's capital, drawn on by every customer.
    Reserve,
    /// Persistent. **Shared.** Losing it loses the liquidation grace window.
    OracleGuard,
    /// Persistent, **keyed by customer**. Money.
    Position(Address),
    /// Persistent, keyed by **(customer, operator)**. Losing one orphans that grant's `committed`.
    ///
    /// A customer may authorise several providers at once — an electricity supplier, a card
    /// issuer, a public-services portal — and each is a separate grant with its own cap,
    /// per-transaction limit, expiry and settlement address. The deck keyed this by operator
    /// alone, which works only when a vault serves one customer; a market vault serves many, so
    /// the key is the pair.
    ///
    /// This reverses Đ-02, which allowed one grant per customer. That decision's fourth reason
    /// was "the product has one operator", and it stopped being true. Its first reason — which
    /// grant's headroom reopens on a repayment — is answered by making the caller name it rather
    /// than by choosing a policy.
    Grant(Address, Address),
    /// Persistent, keyed by customer. The operators this customer has live grants with.
    ///
    /// **Bounded** by [`MAX_GRANTS_PER_OWNER`], and the bound is the whole difference from the
    /// deck's `GrantIndex: Vec<Address>`. An unbounded vector must be walked to release capital on
    /// insolvency, so its length is a gas cost an attacker chooses — grant to a thousand operators
    /// and the position can never be liquidated. Bounded, the walk is a constant.
    ///
    /// Nothing else reads it. `committed` is maintained incrementally, exactly as it was with one
    /// grant, so no write path pays for the length.
    GrantOperators(Address),
    /// **Temporary**, `DRAW_SEEN_TTL`. Accumulates with every capture; Persistent would grow
    /// without bound and could never be cleaned.
    DrawSeen(BytesN<32>),
}

// ---------------------------------------------------------------------------
// View types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Band {
    Healthy,
    Watch,
    Warning,
    Liquidatable,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PositionView {
    pub deposit: i128,
    pub principal: i128,
    pub interest: i128,
    /// `principal + interest`, in debt-token units.
    pub debt: i128,
    /// USD scale.
    pub market_value: i128,
    /// USD scale.
    pub eligible_value: i128,
    /// USD scale, rescaled from debt-token units.
    pub debt_value: i128,
    /// Display only — gates never use this.
    pub ltv_bps: u32,
    /// Display only. `10_000` is exactly the liquidation boundary.
    pub health_bps: u32,
    /// **Oracle price scale** — directly comparable to the feed's `price`.
    pub liq_price: i128,
    pub band: Band,
    /// Seconds since the price was published. `u64::MAX` when there is no price at all.
    pub price_age: u64,
    /// True when the price failed any freshness check. Views never panic; they flag instead.
    pub price_stale: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveView {
    pub total: i128,
    pub committed: i128,
    pub outstanding: i128,
    pub free: i128,
    pub bad_debt: i128,
}

/// Which ceiling is the binding constraint on further borrowing.
///
/// The two move independently: `Cap` only changes when debt is drawn or repaid, `Ltv` changes
/// with every price tick. The UI must say which one is biting, because the remedies are
/// opposite — repay versus deposit more.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Binding {
    Cap,
    Ltv,
    None,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrawQuote {
    pub allowed: bool,
    /// The error code a real call would hit, so callers need not submit a failing transaction
    /// to discover it.
    ///
    /// **`36 DuplicateDrawId` can never appear here**, because `quote_draw` takes no `draw_id`
    /// — the replay guard is invisible to a quote. A caller that has already submitted an id
    /// must track that itself; the contract will still reject the duplicate.
    ///
    /// A stale price reports `40` here even where the strict path would raise `41`: the
    /// non-strict loader collapses "no price at all" into the same `stale` flag.
    pub blocked_reason: Option<u32>,
    /// `cap − drawn`, in debt-token units.
    pub cap_headroom: i128,
    /// What the current price allows, in debt-token units.
    pub ltv_headroom: i128,
    pub binding: Binding,
    pub ltv_after_bps: u32,
    pub price_age: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiquidationQuote {
    pub liquidatable: bool,
    pub ltv_bps: u32,
    /// Debt-token units.
    pub max_repay: i128,
    /// Deposit-token units the liquidator would receive for `max_repay`.
    pub seize_for_max_repay: i128,
    /// `14`, `40`, `53` or `50` — whichever a real call would hit first.
    ///
    /// **Not `41`.** The view takes the non-strict price path, which collapses "no price at all"
    /// and "price rejected" into one `stale` flag, and that flag reports `40`. A liquidator who
    /// needs the distinction gets it from the failing call itself.
    pub blocked_reason: Option<u32>,
}
