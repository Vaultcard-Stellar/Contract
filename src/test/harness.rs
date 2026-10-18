//! Fixture, mock token, and the books check.
//!
//! Three rules from `docs/10-testing.md` govern everything in `super`:
//!
//! 1. **Assert exact error codes** — `assert_eq!(err, Error::CapExceeded)`, never
//!    `assert!(r.is_err())`.
//! 2. **Assert exact numbers**, computed out of band. "Approximately right" cannot catch a
//!    rounding direction that is backwards.
//! 3. **Assert the negative** — after a `draw`, assert the operator's balance is *zero*, not
//!    merely that settlement received something.

#![allow(dead_code)]

use crate::types::Grant;
use crate::{CollateralVault, CollateralVaultClient};
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, Address, Env, MuxedAddress, Symbol,
};

// ---------------------------------------------------------------------------
// Mock SEP-41 token
// ---------------------------------------------------------------------------

/// The Stellar Asset Contract is fixed at 7 decimals, and the fixture needs an 8-decimal
/// collateral token — the whole point of the decimals tests is that the two differ. So the
/// suite carries its own token.
///
/// `transfer` takes a `MuxedAddress` because that is what SEP-41 declares in SDK 27; declaring
/// `Address` would work by accident today and break the day a caller passes a muxed one.
#[contracttype]
#[derive(Clone)]
pub enum TKey {
    Dec,
    Bal(Address),
}

#[contract]
pub struct MockToken;

#[contractimpl]
impl MockToken {
    pub fn __constructor(e: Env, decimals: u32) {
        e.storage().instance().set(&TKey::Dec, &decimals);
    }

    pub fn decimals(e: Env) -> u32 {
        e.storage().instance().get(&TKey::Dec).unwrap()
    }

    pub fn balance(e: Env, id: Address) -> i128 {
        e.storage().persistent().get(&TKey::Bal(id)).unwrap_or(0)
    }

    pub fn mint(e: Env, to: Address, amount: i128) {
        let b = Self::balance(e.clone(), to.clone());
        e.storage().persistent().set(&TKey::Bal(to), &(b + amount));
    }

    pub fn transfer(e: Env, from: Address, to: MuxedAddress, amount: i128) {
        from.require_auth();
        let to = to.address();
        let bf = Self::balance(e.clone(), from.clone());
        assert!(bf >= amount, "mock token: insufficient balance");
        let bt = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&TKey::Bal(from), &(bf - amount));
        e.storage().persistent().set(&TKey::Bal(to), &(bt + amount));
    }
}

// ---------------------------------------------------------------------------
// Fixture constants
// ---------------------------------------------------------------------------

pub const DEP_DEC: u32 = 8; // BTC-like
pub const DEBT_DEC: u32 = 7; // USDC-like
pub const ORACLE_DEC: u32 = 14;
pub const START: u64 = 1_700_000_000;

/// `deposit_decimals + price_decimals − USD_DECIMALS` = 8 + 14 − 7.
pub const SHIFT: u32 = 15;

pub const BTC: i128 = 100_000_000; // one deposit token
pub const USD: i128 = 10_000_000; // one debt token, and one internal USD unit

/// $60 000 at 14 decimals.
pub const P60K: i128 = 60_000 * 100_000_000_000_000;
pub const P16K: i128 = 16_000 * 100_000_000_000_000;
pub const P8K: i128 = 8_000 * 100_000_000_000_000;

pub const FEED: Symbol = symbol_short!("BTC");

pub fn feed() -> crate::Asset {
    crate::Asset::Other(FEED)
}

pub fn oracle_feed() -> mock_oracle::Asset {
    mock_oracle::Asset::Other(FEED)
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The fixture holds **three** customers, not one.
///
/// A market vault that is only ever exercised with a single customer is a single-customer vault
/// with extra parameters — every test would pass with the address ignored. `owner_b` and
/// `owner_c` exist so `check_books` can sum across positions and so the isolation tests have
/// somebody to be isolated from.
pub struct F {
    pub e: Env,
    /// The customer almost every test uses.
    pub owner: Address,
    /// A second and third customer in the same market, funded and ready.
    pub owner_b: Address,
    pub owner_c: Address,
    pub lender: Address,
    pub operator: Address,
    pub settlement: Address,
    /// One pinned destination per customer — see `settlement_for`.
    pub settlement_b: Address,
    pub settlement_c: Address,
    pub liquidator: Address,
    pub third: Address,
    pub oracle_admin: Address,
    pub dep: Address,
    pub debt: Address,
    pub oracle: Address,
    pub id: Address,
}

/// The fixture's risk parameters, from slide 05's BTC row.
///
/// `deposit_decimals` and `debt_decimals` are deliberately **wrong** here: the constructor is
/// specified to overwrite them from the token contracts, and passing plausible values would let
/// that bug through unnoticed.
pub fn cfg(f: &FParts) -> crate::Config {
    crate::Config {
        lender: f.lender.clone(),
        deposit_token: f.dep.clone(),
        deposit_decimals: 99,
        deposit_feed: feed(),
        debt_token: f.debt.clone(),
        debt_decimals: 99,
        oracle: f.oracle.clone(),
        haircut_bps: 1200,
        max_ltv_bps: 5000,
        liq_threshold_bps: 7000,
        full_liq_ltv_bps: 9000,
        liq_bonus_bps: 800,
        close_factor_bps: 5000,
        price_max_age: 300,
        rate_bps_year: 1000,
        vault_cap: 1_000_000 * USD,
        dust_threshold: 10 * USD,
    }
}

/// Everything a `Config` needs, before the vault itself exists.
pub struct FParts {
    pub lender: Address,
    pub dep: Address,
    pub debt: Address,
    pub oracle: Address,
}

impl F {
    /// Deploy tokens, oracle and vault; set the price to $60 000; mint working balances.
    pub fn new() -> F {
        Self::with(|c| c)
    }

    /// Same, with the configuration adjusted before deployment.
    pub fn with(f: impl FnOnce(crate::Config) -> crate::Config) -> F {
        Self::with_tokens(DEP_DEC, ORACLE_DEC, f)
    }

    /// The general form: choose the collateral token's decimals and the feed's, then adjust the
    /// configuration.
    ///
    /// The deck's slide 05 lists three asset profiles and slide 06's prototype values a basket of
    /// all three — BTC at 8 decimals, XLM and USDC at 7. A fixture nailed to one decimals value
    /// can only ever test one of the three, and the decimals boundary is exactly where a
    /// valuation goes wrong by a factor of ten.
    ///
    /// The initial mint scales with the token, so a 7-decimal fixture still has enough units to
    /// deposit 120 000 XLM.
    pub fn with_tokens(
        dep_dec: u32,
        oracle_dec: u32,
        f: impl FnOnce(crate::Config) -> crate::Config,
    ) -> F {
        Self::with_all(dep_dec, DEBT_DEC, oracle_dec, f)
    }

    /// The fullest form: the **debt** token's decimals too.
    ///
    /// Every other fixture pins `debt_decimals` at 7, where `debt_value_usd` is the identity and
    /// the whole rescaling path is a no-op. A market lending a 6- or 18-decimal token exercises
    /// arithmetic that no other test reaches end to end, and getting it wrong moves every gate by
    /// a power of ten while leaving the books internally consistent.
    pub fn with_all(
        dep_dec: u32,
        debt_dec: u32,
        oracle_dec: u32,
        f: impl FnOnce(crate::Config) -> crate::Config,
    ) -> F {
        let e = Env::default();
        e.mock_all_auths();
        e.ledger().with_mut(|l| {
            l.timestamp = START;
            l.sequence_number = 1000;
            // Mainnet's value. The default in the test env is far smaller than TTL_EXTEND, and
            // extend_ttl traps rather than clamping when asked to exceed it — so a test env
            // left at the default would report a TTL bug that does not exist on chain.
            l.max_entry_ttl = 3_110_400;
            l.min_persistent_entry_ttl = 100;
            l.min_temp_entry_ttl = 100;
        });

        let owner = Address::generate(&e);
        let owner_b = Address::generate(&e);
        let owner_c = Address::generate(&e);
        let lender = Address::generate(&e);
        let operator = Address::generate(&e);
        let settlement = Address::generate(&e);
        let settlement_b = Address::generate(&e);
        let settlement_c = Address::generate(&e);
        let liquidator = Address::generate(&e);
        let third = Address::generate(&e);
        let oracle_admin = Address::generate(&e);

        let dep = e.register(MockToken, (dep_dec,));
        let debt = e.register(MockToken, (debt_dec,));
        let oracle = e.register(mock_oracle::MockOracle, (oracle_admin.clone(), oracle_dec));

        let parts = FParts {
            lender: lender.clone(),
            dep: dep.clone(),
            debt: debt.clone(),
            oracle: oracle.clone(),
        };
        let id = e.register(CollateralVault, (f(cfg(&parts)),));

        let fx = F {
            e,
            owner,
            owner_b,
            owner_c,
            lender,
            operator,
            settlement,
            settlement_b,
            settlement_c,
            liquidator,
            third,
            oracle_admin,
            dep,
            debt,
            oracle,
            id,
        };
        fx.set_price(P60K);
        let debt_bag = 10_000_000 * fx.debt_unit();
        for who in [fx.owner.clone(), fx.owner_b.clone(), fx.owner_c.clone()] {
            fx.mint_dep(&who, fx.minted_dep());
            fx.mint_debt(&who, debt_bag);
        }
        fx.mint_debt(&fx.lender, debt_bag);
        fx.mint_debt(&fx.liquidator, debt_bag);
        fx.mint_debt(&fx.third, debt_bag);
        fx
    }

    /// One million whole collateral tokens, whatever the decimals. The starting balance of the
    /// owner, and the figure the withdrawal tests measure against.
    pub fn minted_dep(&self) -> i128 {
        1_000_000 * 10i128.pow(self.v().get_config().deposit_decimals)
    }

    /// One whole debt token, in that token's smallest units. `USD` is the same thing pinned at
    /// 7 decimals; use this wherever the fixture may have been built with something else.
    pub fn debt_unit(&self) -> i128 {
        10i128.pow(self.v().get_config().debt_decimals)
    }

    pub fn v(&self) -> CollateralVaultClient<'_> {
        CollateralVaultClient::new(&self.e, &self.id)
    }

    // ---------------------------------------------------------------------
    // Single-grant shorthands
    // ---------------------------------------------------------------------
    //
    // A grant is now keyed by `(owner, operator)`, so every call that touches one names the
    // operator. Most tests here are not about having several — they are about interest, or
    // liquidation, or a decimals mismatch — and threading `&f.operator` through each of them
    // would bury what they *are* about.
    //
    // These wrappers supply the fixture's own operator. A test that means several grants calls the
    // client directly and names them.

    pub fn repay_as(&self, owner: &Address, from: &Address, amount: &i128) {
        self.v()
            .repay(owner, from, amount, &Some(self.operator.clone()))
    }

    /// Repay without reopening anybody's headroom.
    ///
    /// What a third party does, and the only thing available once the grant is gone: naming a
    /// revoked grant is `GrantNotFound`, deliberately, so a payer expecting a line to reopen
    /// finds out that it did not.
    pub fn repay_only(&self, owner: &Address, from: &Address, amount: &i128) {
        self.v().repay(owner, from, amount, &None)
    }

    pub fn revoke(&self, owner: &Address) {
        self.v().revoke_authority(owner, &self.operator)
    }

    pub fn freeze(&self, owner: &Address, by: &Address) {
        self.v().freeze_grant(owner, &self.operator, by)
    }

    pub fn unfreeze(&self, owner: &Address) {
        self.v().unfreeze_grant(owner, &self.operator)
    }

    pub fn reap(&self, owner: &Address) {
        self.v().reap_expired(owner, &self.operator)
    }

    pub fn grant_of(&self, owner: &Address) -> Option<Grant> {
        self.v().get_grant(owner, &self.operator)
    }
    pub fn dep_t(&self) -> MockTokenClient<'_> {
        MockTokenClient::new(&self.e, &self.dep)
    }
    pub fn debt_t(&self) -> MockTokenClient<'_> {
        MockTokenClient::new(&self.e, &self.debt)
    }
    pub fn orc(&self) -> mock_oracle::MockOracleClient<'_> {
        mock_oracle::MockOracleClient::new(&self.e, &self.oracle)
    }

    pub fn mint_dep(&self, to: &Address, amount: i128) {
        self.dep_t().mint(to, &amount);
    }
    pub fn mint_debt(&self, to: &Address, amount: i128) {
        self.debt_t().mint(to, &amount);
    }

    /// Publish a price stamped with the current ledger time.
    pub fn set_price(&self, price: i128) {
        self.orc().set_price(&oracle_feed(), &price, &0);
    }
    /// Publish a price with an explicit timestamp — the staleness and future-price tests.
    pub fn set_price_at(&self, price: i128, ts: u64) {
        self.orc().set_price(&oracle_feed(), &price, &ts);
    }
    /// Make the feed answer `None`.
    pub fn kill_oracle(&self) {
        self.orc().clear_price(&oracle_feed());
    }

    pub fn now(&self) -> u64 {
        self.e.ledger().timestamp()
    }
    pub fn warp(&self, secs: u64) {
        let t = self.now() + secs;
        self.e.ledger().with_mut(|l| l.timestamp = t);
    }
    /// Move time forward **and** refresh the price, so a test about interest is not accidentally
    /// a test about staleness.
    pub fn warp_fresh(&self, secs: u64, price: i128) {
        self.warp(secs);
        self.set_price(price);
    }

    // -- raw storage, for assertions the views do not expose -----------------

    /// `owner`'s position as stored. Most tests want this one.
    pub fn raw_position(&self) -> crate::Position {
        self.raw_position_of(&self.owner)
    }

    /// Does this customer have a stored position at all?
    ///
    /// Distinct from `raw_position_of(...)` returning zeros: that getter **synthesises** a
    /// default for an address that has never transacted, seeding `last_index` from the live
    /// index. The rebuild test needs the difference, because an indexer that invented a customer
    /// out of nothing would otherwise look correct.
    pub fn has_position(&self, owner: &Address) -> bool {
        self.e.as_contract(&self.id, || {
            self.e
                .storage()
                .persistent()
                .has(&crate::DataKey::Position(owner.clone()))
        })
    }

    pub fn raw_position_of(&self, owner: &Address) -> crate::Position {
        self.e
            .as_contract(&self.id, || crate::storage::position(&self.e, owner))
    }
    pub fn raw_reserve(&self) -> crate::Reserve {
        self.e
            .as_contract(&self.id, || crate::storage::reserve(&self.e))
    }
    pub fn raw_index(&self) -> crate::IndexState {
        self.e
            .as_contract(&self.id, || crate::storage::index_state(&self.e))
    }
    pub fn raw_guard(&self) -> crate::OracleGuard {
        self.e
            .as_contract(&self.id, || crate::storage::oracle_guard(&self.e))
    }

    // -- common setups -------------------------------------------------------

    /// Reserve funded with $100 000, and 1 BTC deposited by `owner`. No grant yet.
    pub fn funded(&self) -> &F {
        self.v().fund_reserve(&(100_000 * USD));
        self.v().deposit(&self.owner, &BTC);
        self
    }

    /// `funded`, plus a grant for `owner`: cap $20 000, per-tx $5 000, 30 days, ceiling 5000 bps.
    pub fn armed(&self) -> &F {
        self.funded();
        self.grant(20_000 * USD, 5_000 * USD, 5000);
        self
    }

    pub fn grant(&self, cap: i128, per_tx: i128, max_ltv: u32) {
        self.grant_for(&self.owner, cap, per_tx, max_ltv);
    }

    /// The same, for any customer. Each gets their own settlement address, so a test that mixes
    /// them up shows it as money in the wrong place rather than as an equal balance.
    ///
    /// **Proposes and accepts**, because a grant now takes two signatures and almost no test here
    /// is about that. The ones that are call the client directly and check the state in between;
    /// for the rest, "the customer has a working line" is the setup they mean, and threading an
    /// `accept_grant` through every one of them would bury what they are actually testing.
    pub fn grant_for(&self, owner: &Address, cap: i128, per_tx: i128, max_ltv: u32) {
        self.propose_for(owner, cap, per_tx, max_ltv);
        self.v().accept_grant(owner, &self.operator);
    }

    /// Just the proposal — capital uncommitted, nothing drawable. For tests about acceptance.
    pub fn propose_for(&self, owner: &Address, cap: i128, per_tx: i128, max_ltv: u32) {
        self.v().grant_authority(
            owner,
            &self.operator,
            &self.settlement_for(owner),
            &cap,
            &per_tx,
            &(self.now() + 30 * 86_400),
            &max_ltv,
        );
    }

    pub fn propose(&self, cap: i128, per_tx: i128, max_ltv: u32) {
        self.propose_for(&self.owner, cap, per_tx, max_ltv);
    }

    pub fn accept(&self, owner: &Address) {
        self.v().accept_grant(owner, &self.operator)
    }

    /// A distinct pinned destination per customer.
    pub fn settlement_for(&self, owner: &Address) -> Address {
        if owner == &self.owner_b {
            self.settlement_b.clone()
        } else if owner == &self.owner_c {
            self.settlement_c.clone()
        } else {
            self.settlement.clone()
        }
    }

    pub fn draw(&self, n: u8, amount: i128) {
        self.draw_for(&self.owner, n, amount);
    }

    pub fn draw_for(&self, owner: &Address, n: u8, amount: i128) {
        self.v()
            .draw(owner, &self.operator, &id32(&self.e, n), &amount);
    }

    /// Deposit, grant and draw for a second or third customer in one line, so isolation tests do
    /// not drown in setup.
    pub fn arm_for(&self, owner: &Address, deposit: i128, cap: i128, draw_id: u8, amount: i128) {
        self.v().deposit(owner, &deposit);
        self.grant_for(owner, cap, cap, 5000);
        if amount > 0 {
            self.draw_for(owner, draw_id, amount);
        }
    }

    /// Every customer this fixture can create. `check_books` sums over it.
    pub fn owners(&self) -> [Address; 3] {
        [
            self.owner.clone(),
            self.owner_b.clone(),
            self.owner_c.clone(),
        ]
    }

    // -- the books -----------------------------------------------------------

    /// I1–I4 plus both token balances, across **every** customer the fixture can create.
    ///
    /// This is where invariant I3 went. On chain it is now differential — each call asserts that
    /// it moved `outstanding` by exactly what it moved that customer's `principal` — because no
    /// single call can sum a market's positions. Here the absolute form is checkable, and it is
    /// the statement that actually matters:
    ///
    /// ```text
    /// reserve.outstanding == Σ principal      over all customers
    /// deposit_token.balance(vault) == Σ deposit
    /// ```
    ///
    /// A differential check that was right on every step and still drifted would show up here and
    /// nowhere else.
    pub fn check_books(&self) {
        let r = self.raw_reserve();

        let mut sum_principal = 0i128;
        let mut sum_deposit = 0i128;
        for who in self.owners() {
            let p = self.raw_position_of(&who);
            assert!(
                p.deposit >= 0 && p.principal >= 0 && p.interest >= 0 && p.accrual_rem >= 0,
                "I4: {p:?}"
            );
            sum_principal += p.principal;
            sum_deposit += p.deposit;
            // The relationship every path except the netting route maintains: committed always
            // covers the undrawn part of a live line.
            if let Some(g) = self.grant_of(&who) {
                assert!(
                    g.cap - g.drawn >= 0,
                    "drawn {} exceeds cap {}",
                    g.drawn,
                    g.cap
                );
            }
        }

        assert!(r.total >= 0 && r.committed >= 0 && r.outstanding >= 0 && r.bad_debt >= 0);
        assert!(r.total >= r.committed + r.outstanding, "I1: {r:?}");
        assert_eq!(r.outstanding, sum_principal, "I3 across all customers");

        let debt_bal = self.debt_t().balance(&self.id);
        assert!(
            debt_bal >= r.total - r.outstanding,
            "I2: balance {debt_bal} < total {} - outstanding {}",
            r.total,
            r.outstanding
        );
        assert_eq!(
            self.dep_t().balance(&self.id),
            sum_deposit,
            "collateral balance vs the sum of every reported position"
        );

        // `committed` covers the undrawn part of every **accepted** line put together.
        //
        // Accepted only, and that is not a loosening. A pending grant reserves nothing — the
        // operator has not taken it up and `accept_grant` is what commits — so counting one here
        // would demand backing for a promise nobody has made. Summing every operator rather than
        // just the fixture's, because a customer may hold several.
        let mut undrawn = 0i128;
        for who in self.owners() {
            for op in self.v().get_grant_operators(&who).iter() {
                if let Some(g) = self.v().get_grant(&who, &op) {
                    if g.accepted {
                        undrawn += g.cap - g.drawn;
                    }
                }
            }
        }
        assert!(
            r.committed >= undrawn,
            "committed {} < Σ(cap − drawn) {undrawn}",
            r.committed
        );
    }
}

/// A deterministic 32-byte draw id.
pub fn id32(e: &Env, n: u8) -> soroban_sdk::BytesN<32> {
    let mut b = [0u8; 32];
    b[31] = n;
    soroban_sdk::BytesN::from_array(e, &b)
}

/// `try_*` returns `Result<Result<T, _>, Result<Error, InvokeError>>`. This unwraps to the
/// contract error, and panics loudly if the call unexpectedly succeeded.
macro_rules! assert_err {
    ($call:expr, $want:expr) => {{
        let got = $call;
        match got {
            Ok(_) => panic!("expected {:?}, call succeeded", $want),
            Err(e) => assert_eq!(e.unwrap(), $want),
        }
    }};
}
