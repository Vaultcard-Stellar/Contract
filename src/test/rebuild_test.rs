//! The rebuild test — slide 02's number-one architectural rule.
//!
//! Run a long scenario, throw the storage away, and rebuild every money field from the **event
//! stream alone**. If they do not match to the smallest unit, the backend can never reconcile,
//! and no amount of backend work can fix it — the missing data was never emitted.
//!
//! This catches the two failures nothing else catches: a write path that changes state without
//! emitting, and an event missing a field. Neither breaks a conventional test, because
//! conventional tests read storage directly.
//!
//! **The reducer below touches no storage.** Its only inputs are the events and the ledger
//! timestamp of the deploy — which a real indexer always has, since every event arrives stamped
//! with its ledger's close time. `DrawSeen` is excluded deliberately: it is Temporary, it
//! expires, and it is not accounting state.

extern crate std;

use super::harness::*;
use crate::{Grant, IndexState, OracleGuard, Position, Reserve};
use soroban_sdk::testutils::Events;
use soroban_sdk::xdr::{ContractEvent, ContractEventBody, ScVal};
use soroban_sdk::{Address, Env, Map, Symbol, TryFromVal, Val};

/// The event tape.
///
/// `Env::events().all()` holds only the most recent top-level invocation, so an indexer has to
/// be simulated the way a real one works: append after every transaction, never look back at
/// storage. `filter_by_contract` keeps the reducer honest — a reducer allowed to read the token
/// contract's own `transfer` events would be reading balances rather than rebuilding them.
struct Tape(std::vec::Vec<ContractEvent>);

impl Tape {
    fn new() -> Tape {
        Tape(std::vec::Vec::new())
    }
    /// Call immediately after each mutating invocation.
    fn snap(&mut self, f: &F) {
        self.0
            .extend_from_slice(f.e.events().all().filter_by_contract(&f.id).events());
    }
}

/// Run a vault call and record whatever it emitted.
macro_rules! step {
    ($t:expr, $f:expr, $call:expr) => {{
        let out = $call;
        $t.snap(&$f);
        out
    }};
}

// ---------------------------------------------------------------------------
// A simulated indexer
// ---------------------------------------------------------------------------

/// Market state as an indexer would hold it: shared figures once, per-customer figures keyed by
/// address.
///
/// Nothing on chain enumerates customers — deliberately — so this list is built **from the event
/// stream itself**, which is exactly how a real indexer discovers who exists. That is the reason
/// every position-scoped event carries `owner`.
struct Rebuilt {
    res: Reserve,
    idx: IndexState,
    guard: OracleGuard,
    pos: std::vec::Vec<(Address, Position)>,
    /// Keyed by `(owner, operator)`, because a customer may authorise several providers at once
    /// and each carries its own cap, its own `drawn` and its own freeze flag. Keying by owner
    /// alone — which is what this reducer did while the contract allowed one grant — silently
    /// merges an electricity supplier's line into a card issuer's.
    grants: std::vec::Vec<((Address, Address), Grant)>,
}

impl Rebuilt {
    /// The customer's position, created on first sight — a `Position` appears the moment its
    /// first event does.
    fn pos_mut(&mut self, e: &Env, owner: &Address) -> &mut Position {
        if !self.pos.iter().any(|(a, _)| a == owner) {
            self.pos.push((
                owner.clone(),
                Position {
                    deposit: 0,
                    principal: 0,
                    interest: 0,
                    last_index: crate::INDEX_SCALE,
                    accrual_rem: 0,
                },
            ));
        }
        let _ = e;
        &mut self.pos.iter_mut().find(|(a, _)| a == owner).unwrap().1
    }

    fn grant_of(&mut self, owner: &Address, operator: &Address) -> Option<&mut Grant> {
        self.grants
            .iter_mut()
            .find(|((a, o), _)| a == owner && o == operator)
            .map(|(_, g)| g)
    }

    fn set_grant(&mut self, owner: &Address, operator: &Address, g: Grant) {
        self.drop_grant(owner, operator);
        self.grants.push(((owner.clone(), operator.clone()), g));
    }

    fn drop_grant(&mut self, owner: &Address, operator: &Address) {
        self.grants
            .retain(|((a, o), _)| !(a == owner && o == operator));
    }

    /// Which operators this customer has live, discovered from the stream alone.
    ///
    /// The on-chain `GrantOperators` index is storage, not an event, so an indexer never sees it.
    /// It does not have to: `grant_new` adds and `grant_rvk` / `grant_reap` remove, and the
    /// insolvency branch emits one `grant_rvk` per grant precisely so this list can be emptied
    /// without the reducer knowing the bound.
    fn operators_of(&self, owner: &Address) -> std::vec::Vec<Address> {
        self.grants
            .iter()
            .filter(|((a, _), _)| a == owner)
            .map(|((_, o), _)| o.clone())
            .collect()
    }

    fn get_pos(&self, owner: &Address) -> Position {
        self.pos
            .iter()
            .find(|(a, _)| a == owner)
            .map(|(_, p)| p.clone())
            .unwrap_or(Position {
                deposit: 0,
                principal: 0,
                interest: 0,
                last_index: crate::INDEX_SCALE,
                accrual_rem: 0,
            })
    }

    /// Invariant I3 from the indexer's side. `outstanding` is a shared figure, and after any
    /// event that moved one customer's principal it must equal the sum over all of them — which
    /// is precisely the absolute form the contract can no longer check for itself.
    fn sum_principal(&self) -> i128 {
        self.pos.iter().map(|(_, p)| p.principal).sum()
    }

    fn knows(&self, owner: &Address) -> bool {
        self.pos.iter().any(|(a, _)| a == owner)
    }

    fn get_grant(&self, owner: &Address, operator: &Address) -> Option<Grant> {
        self.grants
            .iter()
            .find(|((a, o), _)| a == owner && o == operator)
            .map(|(_, g)| g.clone())
    }
}

fn i128_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> i128 {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    i128::try_from_val(e, &v).unwrap()
}
fn u64_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> u64 {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    u64::try_from_val(e, &v).unwrap()
}
fn u32_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> u32 {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    u32::try_from_val(e, &v).unwrap()
}
fn bool_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> bool {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    bool::try_from_val(e, &v).unwrap()
}
fn addr_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> Address {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    Address::try_from_val(e, &v).unwrap()
}
/// An `Option<Address>` field — `Repaid.operator`, which is `None` when the payer named nobody.
fn maybe_addr_of(e: &Env, m: &Map<Symbol, Val>, k: &str) -> Option<Address> {
    let v = m.get(Symbol::new(e, k)).expect("missing field");
    Option::<Address>::try_from_val(e, &v).unwrap()
}

/// Replay the tape. One `match` over the event name; no reads of anything else.
fn reduce(f: &F, tape: &Tape, deployed_at: u64) -> Rebuilt {
    let e = &f.e;
    let mut s = Rebuilt {
        res: Reserve {
            total: 0,
            committed: 0,
            outstanding: 0,
            bad_debt: 0,
        },
        idx: IndexState {
            index: crate::INDEX_SCALE,
            last_accrual: deployed_at,
        },
        guard: OracleGuard {
            outage_at: 0,
            grace_until: 0,
        },
        pos: std::vec::Vec::new(),
        grants: std::vec::Vec::new(),
    };

    for ev in tape.0.iter() {
        let ContractEventBody::V0(body) = &ev.body;
        let topic1: ScVal = body.topics.to_vec()[1].clone();
        let name = Symbol::try_from_val(e, &topic1).unwrap();
        let m = Map::<Symbol, Val>::try_from_val(e, &body.data).unwrap();
        let is = |k: &str| name == Symbol::new(e, k);
        // Every position-scoped event names its customer. That field is why this reducer can
        // exist at all: nothing on chain enumerates customers, so the stream is the only place
        // the set of them is written down.
        let who = |k: &str| addr_of(e, &m, k);

        if is("initialized") || is("paused") || is("unpaused") {
            // Config and the pause flag are not money; the comparison below does not cover them.
        } else if is("deposited") || is("withdrawn") {
            let o = who("owner");
            s.pos_mut(e, &o).deposit = i128_of(e, &m, "deposit_after");
        } else if is("accrued") {
            let o = who("owner");
            s.idx.index = i128_of(e, &m, "index");
            s.idx.last_accrual = u64_of(e, &m, "last_accrual");
            let idx = s.idx.index;
            let p = s.pos_mut(e, &o);
            p.interest = i128_of(e, &m, "interest_after");
            p.accrual_rem = i128_of(e, &m, "accrual_rem");
            // Settlement always leaves the position on the current index.
            p.last_index = idx;
        } else if is("grant_prop") || is("grant_acc") {
            // Both carry the whole terms, so both build the same struct and differ only in
            // whether the line is live. `grant_acc` re-reads the terms rather than flipping a
            // flag on whatever is there: a proposal may have been rejected and re-made between
            // the two, and reconstructing from the accepted event alone is what makes it
            // sufficient on its own.
            let o = who("owner");
            let accepted = is("grant_acc");
            let g = Grant {
                operator: addr_of(e, &m, "operator"),
                settlement: addr_of(e, &m, "settlement"),
                cap: i128_of(e, &m, "cap"),
                drawn: 0,
                per_tx_limit: i128_of(e, &m, "per_tx_limit"),
                expiry: u64_of(e, &m, "expiry"),
                max_ltv_bps: u32_of(e, &m, "max_ltv_bps"),
                frozen: false,
                accepted,
            };
            s.set_grant(&o, &who("operator"), g);
            // A proposal carries no `committed_after`, because it moves none.
            if accepted {
                s.res.committed = i128_of(e, &m, "committed_after");
            }
        } else if is("grant_rej") {
            // The operator declined. The entry goes, and `committed` is carried unchanged —
            // asserted rather than assumed, so a rejection that somehow moved capital shows up
            // here rather than at the next reconciliation.
            let o = who("owner");
            s.drop_grant(&o, &who("operator"));
            s.res.committed = i128_of(e, &m, "committed_after");
        } else if is("grant_rvk") || is("grant_reap") {
            let o = who("owner");
            s.drop_grant(&o, &who("operator"));
            s.res.committed = i128_of(e, &m, "committed_after");
        } else if is("grant_frz") || is("grant_unfz") {
            let o = who("owner");
            let frozen = is("grant_frz");
            if let Some(g) = s.grant_of(&o, &who("operator")) {
                g.frozen = frozen;
            }
        } else if is("drawn") {
            let o = who("owner");
            let drawn_after = i128_of(e, &m, "drawn_after");
            let principal_after = i128_of(e, &m, "principal_after");
            if let Some(g) = s.grant_of(&o, &who("operator")) {
                g.drawn = drawn_after;
            }
            s.pos_mut(e, &o).principal = principal_after;
            s.res.committed = i128_of(e, &m, "committed_after");
            s.res.outstanding = i128_of(e, &m, "outstanding_after");
        } else if is("repaid") {
            let o = who("owner");
            let restored = i128_of(e, &m, "headroom_restored");
            let principal_after = i128_of(e, &m, "principal_after");
            let interest_after = i128_of(e, &m, "interest_after");
            let interest_paid = i128_of(e, &m, "interest_paid");
            {
                let p = s.pos_mut(e, &o);
                p.principal = principal_after;
                p.interest = interest_after;
            }
            s.res.total += interest_paid;
            // Which line reopened is on the event; without that field the reducer would have to
            // guess, and any guess is wrong the moment a customer holds two grants.
            if let Some(op) = maybe_addr_of(e, &m, "operator") {
                if let Some(g) = s.grant_of(&o, &op) {
                    g.drawn -= restored;
                    s.res.committed += restored;
                }
            }
            s.res.outstanding = s.sum_principal();
        } else if is("res_fund") || is("res_wdraw") {
            s.res.total = i128_of(e, &m, "total_after");
        } else if is("liquidated") {
            let o = who("owner");
            let by_lender = bool_of(e, &m, "by_lender");
            let interest_paid = i128_of(e, &m, "interest_paid");
            let principal_paid = i128_of(e, &m, "principal_paid");
            let deposit_after = i128_of(e, &m, "deposit_after");
            let principal_after = i128_of(e, &m, "principal_after");
            {
                let p = s.pos_mut(e, &o);
                p.deposit = deposit_after;
                p.principal = principal_after;
                p.interest -= interest_paid;
            }

            // No grant is touched on either liquidation route, so there is nothing to apply here
            // and no `headroom_restored` field to read. On the insolvency branch the grants are
            // gone already: one `grant_rvk` per grant fired before this event and carried the
            // final `committed`.

            if by_lender {
                // No tokens arrived, so the claim falls by the principal and the interest is
                // realised as collateral rather than credited to the pool.
                s.res.total -= principal_paid;
            } else {
                s.res.total += interest_paid;
            }
            s.res.outstanding = s.sum_principal();
        } else if is("bad_debt") {
            s.res.bad_debt = i128_of(e, &m, "bad_debt_total");
            s.res.total = i128_of(e, &m, "total_after");
            s.res.outstanding = s.sum_principal();
        } else if is("orc_out") {
            s.guard.outage_at = u64_of(e, &m, "at");
        } else if is("orc_recov") {
            s.guard.grace_until = u64_of(e, &m, "grace_until");
            s.guard.outage_at = 0;
        } else {
            panic!("unhandled event — the reducer is out of date with events.rs");
        }
    }
    s
}

fn compare(f: &F, r: &Rebuilt) {
    // Shared state first — one figure each for the whole market.
    assert_eq!(r.res, f.raw_reserve(), "Reserve rebuilt from events");
    assert_eq!(r.idx, f.raw_index(), "IndexState rebuilt from events");
    assert_eq!(r.guard, f.raw_guard(), "OracleGuard rebuilt from events");

    // Then every customer the fixture can create. The set the reducer discovered must be exactly
    // the set that exists on chain — an indexer that invented a customer, or missed one, is as
    // wrong as one that got a balance wrong. Nothing enumerates customers on chain, so this list
    // came entirely from the `owner` field on the events.
    for who in f.owners() {
        assert_eq!(
            r.knows(&who),
            f.has_position(&who),
            "the reducer and the ledger disagree about whether this customer exists"
        );
        if f.has_position(&who) {
            assert_eq!(
                r.get_pos(&who),
                f.raw_position_of(&who),
                "Position rebuilt from events, per customer"
            );
        }
        // Every grant, not just the fixture's usual operator. The set has to match too: a
        // reducer that kept a revoked grant, or lost one of three, is as wrong as one that got a
        // cap wrong, and comparing a single operator would hide both.
        let on_chain = f.v().get_grant_operators(&who);
        let mut rebuilt_ops = r.operators_of(&who);
        assert_eq!(
            rebuilt_ops.len(),
            on_chain.len() as usize,
            "the reducer and the ledger disagree about how many grants this customer holds"
        );
        for op in on_chain.iter() {
            assert_eq!(
                r.get_grant(&who, &op),
                f.v().get_grant(&who, &op),
                "Grant rebuilt from events, per customer and operator"
            );
            rebuilt_ops.retain(|o| o != &op);
        }
        assert!(
            rebuilt_ops.is_empty(),
            "the reducer invented a grant that is not on chain"
        );
    }

    // And the sum the contract can no longer check for itself.
    assert_eq!(
        r.sum_principal(),
        f.raw_reserve().outstanding,
        "Σ principal from events == outstanding"
    );
}

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

/// Deposit, grant, several draws, accrue, partial repay, partial liquidation, the netting route,
/// an oracle outage and recovery, revoke, and a full unwind.
///
/// Every branch that writes money appears at least once. A path that changed state without
/// emitting would surface here as a mismatched field, naming itself.
#[test]
fn state_rebuilds_from_events_alone() {
    let f = F::new();
    let mut t = Tape::new();

    step!(t, f, f.v().fund_reserve(&(100_000 * USD)));
    step!(t, f, f.v().deposit(&f.owner, &BTC));
    step!(t, f, f.grant(20_000 * USD, 5_000 * USD, 5000));

    step!(t, f, f.draw(1, 5_000 * USD));
    f.warp_fresh(3_600, P60K);
    step!(t, f, f.draw(2, 3_000 * USD));

    // Freeze and unfreeze, so both flag transitions are on the tape.
    step!(t, f, f.freeze(&f.owner, &f.lender));
    step!(t, f, f.unfreeze(&f.owner));

    f.warp_fresh(10 * 86_400, P60K);
    step!(t, f, f.v().accrue(&f.owner));
    step!(t, f, f.v().deposit(&f.owner, &(BTC / 2)));
    step!(t, f, f.repay_as(&f.owner, &f.third, &(1_000 * USD)));

    // 1.5 BTC of collateral against ~$7 000 of debt: at $7 000/BTC the position is past the
    // threshold and still solvent, so both partial routes are available.
    f.set_price(7_000 * 100_000_000_000_000);
    // The netting route first — it seizes at market price, so it leaves the position
    // liquidatable for the ordinary route that follows.
    step!(t, f, f.v().lender_liquidate(&f.owner, &(1_000 * USD)));
    step!(
        t,
        f,
        f.v().liquidate(&f.owner, &f.liquidator, &(2_000 * USD), &0)
    );

    // An outage and a recovery, so the guard is exercised too.
    f.kill_oracle();
    step!(t, f, f.v().report_oracle_state());
    f.warp(600);
    f.set_price(7_000 * 100_000_000_000_000);
    step!(t, f, f.v().report_oracle_state());

    step!(t, f, f.revoke(&f.owner));
    step!(t, f, f.v().withdraw_reserve(&(10_000 * USD)));

    // Unwind: clear the debt, then take the collateral back.
    f.warp_fresh(86_400, P60K);
    step!(t, f, f.v().accrue(&f.owner));
    let owed = f.v().get_position(&f.owner).debt;
    step!(t, f, f.repay_as(&f.owner, &f.third, &owed));
    let held = f.raw_position().deposit;
    step!(t, f, f.v().withdraw(&f.owner, &held));

    // An accrual on a position with no principal: the index moves, no interest is added, and the
    // event still fires. Without that, `IndexState` would be unreconstructible from here on.
    f.warp_fresh(86_400, P60K);
    step!(t, f, f.v().accrue(&f.owner));

    f.check_books();
    compare(&f, &reduce(&f, &t, START));
}

/// The insolvency branch, rebuilt. Its event ordering is the awkward one — `GrantRevoked` fires
/// *before* `Liquidated`, carrying the final `committed`, so a reducer that blindly re-applied
/// `headroom_restored` afterwards would double-count.
#[test]
fn the_insolvency_branch_rebuilds_too() {
    let f = F::new();
    let mut t = Tape::new();

    step!(t, f, f.v().fund_reserve(&(100_000 * USD)));
    step!(t, f, f.v().deposit(&f.owner, &BTC));
    step!(t, f, f.grant(20_000 * USD, 20_000 * USD, 5000));
    step!(t, f, f.draw(1, 10_000 * USD));

    f.warp_fresh(365 * 86_400, P8K);
    step!(t, f, f.v().accrue(&f.owner));
    step!(t, f, f.v().liquidate(&f.owner, &f.liquidator, &USD, &0));

    assert_eq!(f.raw_reserve().bad_debt, 3_000 * USD);
    f.check_books();
    compare(&f, &reduce(&f, &t, START));
}

/// One customer, three providers, rebuilt.
///
/// The scenario the whole multi-grant change exists for, and the one that would have been
/// impossible to reconstruct before `Repaid` carried the operator: two grants are drawn against,
/// a repayment reopens exactly one of them, a third is frozen and then revoked, and every figure
/// still has to come back from the stream alone.
///
/// The reducer here holds no `GrantOperators` — that key is storage and never appears in an
/// event. It learns the set from `grant_new` and `grant_rvk` and nothing else, which is precisely
/// what a real indexer has to do.
#[test]
fn several_grants_for_one_customer_rebuild() {
    let f = F::new();
    let mut t = Tape::new();

    let ops: std::vec::Vec<(Address, Address)> = (0..3)
        .map(|_| {
            (
                <Address as soroban_sdk::testutils::Address>::generate(&f.e),
                <Address as soroban_sdk::testutils::Address>::generate(&f.e),
            )
        })
        .collect();

    step!(t, f, f.v().fund_reserve(&(100_000 * USD)));
    step!(t, f, f.v().deposit(&f.owner, &BTC));

    for (operator, settlement) in ops.iter() {
        step!(
            t,
            f,
            f.v().grant_authority(
                &f.owner,
                operator,
                settlement,
                &(5_000 * USD),
                &(5_000 * USD),
                &(f.now() + 30 * 86_400),
                &5000,
            )
        );
        // Both halves on the tape: the proposal commits nothing and the acceptance is where the
        // capital moves, so a reducer that applied `committed_after` from the wrong one would
        // diverge here rather than at somebody's reconciliation report.
        step!(t, f, f.v().accept_grant(&f.owner, operator));
    }

    step!(
        t,
        f,
        f.v()
            .draw(&f.owner, &ops[0].0, &id32(&f.e, 1), &(4_000 * USD))
    );
    f.warp_fresh(3_600, P60K);
    step!(
        t,
        f,
        f.v()
            .draw(&f.owner, &ops[1].0, &id32(&f.e, 2), &(3_000 * USD))
    );

    // A repayment naming the first line. The second must be untouched, and the only way the
    // reducer can know that is the `operator` field on `Repaid`.
    f.warp_fresh(10 * 86_400, P60K);
    step!(
        t,
        f,
        f.v()
            .repay(&f.owner, &f.third, &(2_000 * USD), &Some(ops[0].0.clone()))
    );
    // And one naming nobody — the third-party route, which reopens no line at all.
    step!(t, f, f.v().repay(&f.owner, &f.third, &(500 * USD), &None));

    // The third provider is frozen, then cut off entirely, while the other two keep running.
    step!(t, f, f.v().freeze_grant(&f.owner, &ops[2].0, &f.owner));
    step!(t, f, f.v().unfreeze_grant(&f.owner, &ops[2].0));
    step!(t, f, f.v().revoke_authority(&f.owner, &ops[2].0));

    // A draw on a line that is still live, after all of that.
    step!(
        t,
        f,
        f.v()
            .draw(&f.owner, &ops[1].0, &id32(&f.e, 3), &(2_000 * USD))
    );

    f.check_books();
    compare(&f, &reduce(&f, &t, START));
}

/// A vault that never borrows still has to rebuild — the clock advances, the reserve moves, and
/// the grant lifecycle runs without a single draw.
#[test]
fn a_vault_that_never_borrows_rebuilds() {
    let f = F::new();
    let mut t = Tape::new();

    step!(t, f, f.v().fund_reserve(&(50_000 * USD)));
    step!(t, f, f.v().deposit(&f.owner, &(BTC / 4)));
    f.warp_fresh(7 * 86_400, P60K);
    step!(t, f, f.v().accrue(&f.owner));

    step!(t, f, f.grant(5_000 * USD, 1_000 * USD, 5000));
    f.warp_fresh(30 * 86_400, P60K);
    step!(t, f, f.reap(&f.owner));

    step!(t, f, f.v().withdraw(&f.owner, &(BTC / 4)));
    step!(t, f, f.v().withdraw_reserve(&(50_000 * USD)));

    f.check_books();
    compare(&f, &reduce(&f, &t, START));
}

/// Two customers interleaved, rebuilt together.
///
/// A single-customer rebuild proves the events carry enough about *a* position. This proves they
/// carry enough about **which** position — and that the reducer, which has no other source, ends
/// up with the right set of customers and the right figure against each name.
#[test]
fn two_customers_interleaved_rebuild() {
    let f = F::new();
    let mut t = Tape::new();

    step!(t, f, f.v().fund_reserve(&(200_000 * USD)));

    // A and B open, transact against each other's capital, and are settled at different moments.
    step!(t, f, f.v().deposit(&f.owner, &BTC));
    step!(t, f, f.grant(20_000 * USD, 5_000 * USD, 5000));
    step!(t, f, f.draw(1, 5_000 * USD));

    f.warp_fresh(10 * 86_400, P60K);
    step!(t, f, f.v().deposit(&f.owner_b, &BTC));
    step!(
        t,
        f,
        f.grant_for(&f.owner_b, 20_000 * USD, 20_000 * USD, 5000)
    );
    step!(t, f, f.draw_for(&f.owner_b, 60, 10_000 * USD));

    f.warp_fresh(20 * 86_400, P60K);
    step!(t, f, f.v().accrue(&f.owner));
    step!(t, f, f.repay_as(&f.owner_b, &f.third, &(2_000 * USD)));
    step!(t, f, f.freeze(&f.owner, &f.lender));

    // One is liquidated; the other is not.
    f.set_price(12_000 * 100_000_000_000_000);
    step!(
        t,
        f,
        f.v()
            .liquidate(&f.owner_b, &f.liquidator, &(2_000 * USD), &0)
    );
    // The partial liquidation pulled B back to 6988 bps — a whisker under the threshold — so the
    // netting route needs one more step down to be reachable at all.
    f.set_price(11_000 * 100_000_000_000_000);
    step!(t, f, f.v().lender_liquidate(&f.owner_b, &(1_000 * USD)));

    // And one leaves entirely, while the other stays open.
    f.set_price(P60K);
    step!(t, f, f.v().accrue(&f.owner));
    let owed = f.v().get_position(&f.owner).debt;
    step!(t, f, f.repay_as(&f.owner, &f.third, &owed));
    step!(t, f, f.revoke(&f.owner));
    let held = f.raw_position_of(&f.owner).deposit;
    step!(t, f, f.v().withdraw(&f.owner, &held));

    // `owner_c` never appeared, and must rebuild as absent rather than as an empty position.
    assert!(!f.has_position(&f.owner_c));

    f.check_books();
    compare(&f, &reduce(&f, &t, START));
}
