//! PropAMM quote tracking: link a swap to the maker's quote-update tx it
//! consumed.
//!
//! A proprietary AMM (propAMM) has no passive curve liquidity -- makers push
//! quotes on-chain by writing price levels into storage (either the amm
//! contract's own, or a shared quote-store contract the amms read at swap
//! time), and swaps then execute against the stored quote; the builder places
//! the latest quote-update tx immediately before the taker tx in the block.
//! Re-executing every tx exposes exactly which storage slots a swap READ and
//! which tx last WROTE them, so the update-then-swap pair is linked without
//! knowing the contract's storage layout:
//!
//!   * a tx that writes a tracked amm's storage and moves no tokens is a
//!     [`QuoteUpdate`] (the maker pushing a price);
//!   * a tx with a detected swap that reads slots last written by a quote
//!     update yields a [`QuoteLink`] back to that update tx.
//!
//! Writes made by swap txs themselves (inventory, nonces) are indexed but
//! never count as quote updates, so a swap does not link to a previous swap.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use yevm_base::{Acc, Int};
use yevm_core::trace::{Event, Target, Trace};

use crate::Alerts;
use crate::analyse::{is_live, undone_ranges};

/// Reference to a transaction in the stream.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxRef {
    pub block: u64,
    pub index: u64,
    pub hash: Int,
    /// The tx sender (for a quote update: the maker, or its ops wallet).
    pub sender: Acc,
}

/// A tx that wrote a tracked amm's storage without moving any tokens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteUpdate {
    /// The tracked contract whose storage holds the quote: the amm itself, or
    /// a shared quote store the amms read at swap time.
    pub amm: Acc,
    pub maker: Acc,
    /// Number of distinct storage slots written.
    pub slots: usize,
}

/// A swap tx consumed storage on a tracked amm that an earlier quote-update tx
/// wrote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteLink {
    pub amm: Acc,
    /// How many of the slots this tx read were written by `update`.
    pub slots: usize,
    /// The quote-update tx whose price this swap executed against.
    pub update: TxRef,
}

/// What [`QuoteTracker::observe`] found in one transaction.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub updates: Vec<QuoteUpdate>,
    pub links: Vec<QuoteLink>,
}

struct Write {
    tx: TxRef,
    quote_update: bool,
}

/// Rolling (contract, slot) -> last-writer index. Feed it every transaction,
/// in stream order, via [`observe`].
///
/// Built with [`all`], it indexes storage writes of EVERY contract, so a quote
/// store does not have to be known in advance: makers run their own feed
/// contracts, and the swap's storage READS are what reveal which one was
/// consumed. Bound memory with [`evict_before`] (quote updates land in the
/// same block as the swap that consumes them, right before it, so a short
/// window suffices). [`new`] with a non-empty list restricts indexing to those
/// contracts.
///
/// NOTE: entries from re-orged blocks are not evicted automatically; call
/// [`purge_from`] when the receiver detects a reorg.
///
/// [`all`]: QuoteTracker::all
/// [`new`]: QuoteTracker::new
/// [`observe`]: QuoteTracker::observe
/// [`evict_before`]: QuoteTracker::evict_before
/// [`purge_from`]: QuoteTracker::purge_from
pub struct QuoteTracker {
    tracked: Vec<Acc>,
    writes: HashMap<(Acc, Int), Write>,
}

impl QuoteTracker {
    pub fn new(tracked: impl IntoIterator<Item = Acc>) -> Self {
        Self {
            tracked: tracked.into_iter().collect(),
            writes: HashMap::new(),
        }
    }

    /// Track storage writes of every contract.
    pub fn all() -> Self {
        Self::new([])
    }

    /// Process one transaction: `traces` are its trace stream and `alerts` the
    /// result of [`crate::analyse`] over them. Returns the quote updates this
    /// tx performed and the quote links its swaps consumed.
    pub fn observe(
        &mut self,
        block: u64,
        index: u64,
        hash: Int,
        traces: &[Trace],
        alerts: &Alerts,
    ) -> Observation {
        let undone = undone_ranges(traces);
        let tracks = |acc: &Acc| self.tracked.is_empty() || self.tracked.contains(acc);
        let mut reads: Vec<(Acc, Int)> = Vec::new();
        let mut writes: Vec<(Acc, Int)> = Vec::new();
        let mut first_caller: Option<Acc> = None;
        for t in traces {
            if !is_live(t, &undone) {
                continue;
            }
            match &t.event {
                Event::Call(call, _) => {
                    if first_caller.is_none() {
                        first_caller = Some(call.by);
                    }
                }
                Event::Get(Target::Store { acc, key, .. }) if tracks(acc) => {
                    reads.push((*acc, *key));
                }
                Event::Put(Target::Store { acc, key, .. }, _) if tracks(acc) => {
                    writes.push((*acc, *key));
                }
                _ => {}
            }
        }
        reads.sort();
        reads.dedup();
        writes.sort();
        writes.dedup();

        let sender = alerts
            .fee
            .as_ref()
            .map(|f| f.sender)
            .or(first_caller)
            .unwrap_or_default();

        // Link BEFORE recording this tx's own writes, so a swap that also
        // touches the amm's storage never links to itself.
        let mut links: Vec<QuoteLink> = Vec::new();
        if !alerts.swaps.is_empty() {
            let mut latest: HashMap<Acc, TxRef> = HashMap::new();
            for (amm, slot) in &reads {
                if let Some(w) = self.writes.get(&(*amm, *slot))
                    && w.quote_update
                {
                    latest
                        .entry(*amm)
                        .and_modify(|cur| {
                            if (w.tx.block, w.tx.index) > (cur.block, cur.index) {
                                *cur = w.tx;
                            }
                        })
                        .or_insert(w.tx);
                }
            }
            for (amm, tx) in &latest {
                let slots = reads
                    .iter()
                    .filter(|(a, s)| {
                        a == amm
                            && self
                                .writes
                                .get(&(*a, *s))
                                .map(|w| {
                                    w.quote_update
                                        && w.tx.block == tx.block
                                        && w.tx.index == tx.index
                                })
                                .unwrap_or(false)
                    })
                    .count();
                links.push(QuoteLink {
                    amm: *amm,
                    slots,
                    update: *tx,
                });
            }
            links.sort_by_key(|l| l.amm);
        }

        // A quote update pushes state but moves nothing. Approvals are also
        // excluded: an approve writes an allowance slot that the approved swap
        // later reads, which would otherwise masquerade as update-then-consume.
        let quote_update = !writes.is_empty()
            && alerts.swaps.is_empty()
            && alerts.erc20_transfers.is_empty()
            && alerts.erc721_transfers.is_empty()
            && alerts.erc20_approvals.is_empty();
        let mut updates: Vec<QuoteUpdate> = Vec::new();
        if quote_update {
            let mut counts: Vec<(Acc, usize)> = Vec::new();
            for (amm, _) in &writes {
                match counts.iter_mut().find(|(a, _)| a == amm) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((*amm, 1)),
                }
            }
            for (amm, slots) in counts {
                updates.push(QuoteUpdate {
                    amm,
                    maker: sender,
                    slots,
                });
            }
        }

        let tx = TxRef {
            block,
            index,
            hash,
            sender,
        };
        for (amm, slot) in writes {
            self.writes.insert((amm, slot), Write { tx, quote_update });
        }

        Observation { updates, links }
    }

    /// Drop indexed writes from `block` onward (reorg recovery).
    pub fn purge_from(&mut self, block: u64) {
        self.writes.retain(|_, w| w.tx.block < block);
    }

    /// Drop indexed writes older than `block` (rolling memory bound when
    /// tracking every contract). Quote updates land in the same block as the
    /// swap that consumes them, so even a short window loses nothing.
    pub fn evict_before(&mut self, block: u64) {
        self.writes.retain(|_, w| w.tx.block >= block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Swap, SwapProtocol};
    use yevm_core::evm::CallMode;
    use yevm_misc::buf::Buf;

    fn addr(hex: &str) -> Acc {
        let b = hex::decode(hex.trim_start_matches("0x")).unwrap();
        Acc::from(b.as_slice())
    }

    fn trace(seq: usize, event: Event) -> Trace {
        Trace {
            seq,
            event,
            depth: 0,
            reverted: false,
        }
    }

    fn call(by: Acc, to: Acc) -> Event {
        Event::Call(
            yevm_core::Call {
                by,
                to: Some(to),
                gas: 100_000,
                eth: Int::ZERO,
                data: Buf::default(),
            },
            CallMode::Call(0, 0),
        )
    }

    fn put(acc: Acc, key: u64, val: u64, next: u64) -> Event {
        Event::Put(
            Target::Store {
                acc,
                key: Int::from(key),
                val: Int::from(val),
            },
            Int::from(next),
        )
    }

    fn get(acc: Acc, key: u64) -> Event {
        Event::Get(Target::Store {
            acc,
            key: Int::from(key),
            val: Int::ZERO,
        })
    }

    fn fee(sender: Acc) -> Event {
        let coinbase = addr("0x9999999999999999999999999999999999999999");
        Event::Fee(sender, coinbase, Int::ZERO, Int::ZERO, 100_000)
    }

    fn swap_alert(swapper: Acc) -> Alerts {
        Alerts {
            swaps: vec![Swap {
                swapper,
                recipient: swapper,
                sold: vec![],
                bought: vec![],
                pools: vec![],
                legs: 0,
                protocol: SwapProtocol::Unknown,
            }],
            ..Default::default()
        }
    }

    const AMM: Acc = yevm_base::acc("0x5979458912f80b96d30d4220af8e2e4925a33320");
    const MAKER: Acc = yevm_base::acc("0x1111111111111111111111111111111111111111");
    const USER: Acc = yevm_base::acc("0x2222222222222222222222222222222222222222");

    fn update_tx(seq0: usize) -> Vec<Trace> {
        vec![
            trace(seq0, call(MAKER, AMM)),
            trace(seq0 + 1, put(AMM, 7, 100, 101)),
            trace(seq0 + 2, put(AMM, 8, 200, 202)),
            trace(seq0 + 3, Event::Return(Buf::default(), 21_000)),
            trace(seq0 + 4, fee(MAKER)),
        ]
    }

    #[test]
    fn detects_quote_update_and_links_swap() {
        let mut tracker = QuoteTracker::new([AMM]);

        // tx 1: the maker pushes a quote (2 slots), no token movement.
        let t1 = update_tx(0);
        let a1 = Alerts::default();
        let obs1 = tracker.observe(100, 1, Int::from(0xaau64), &t1, &a1);
        assert_eq!(
            obs1.updates,
            vec![QuoteUpdate {
                amm: AMM,
                maker: MAKER,
                slots: 2
            }]
        );
        assert!(obs1.links.is_empty());

        // tx 2: a swap that reads one of the quoted slots.
        let t2 = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(AMM, 7)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs2 = tracker.observe(100, 5, Int::from(0xbbu64), &t2, &swap_alert(USER));
        assert!(obs2.updates.is_empty());
        assert_eq!(obs2.links.len(), 1);
        let l = &obs2.links[0];
        assert_eq!(l.amm, AMM);
        assert_eq!(l.slots, 1);
        assert_eq!(l.update.block, 100);
        assert_eq!(l.update.index, 1);
        assert_eq!(l.update.sender, MAKER);
    }

    #[test]
    fn shared_quote_store_links_swap_on_another_amm() {
        // Titan pAMM shape: the maker's update tx writes the shared quote
        // store, and the swap reads the store from inside the amm's frame.
        let store = addr("0xda7afeed021eafc1c1af9c362de477dad0396b81");
        let mut tracker = QuoteTracker::new([store, AMM]);

        let update = vec![
            trace(0, call(MAKER, store)),
            trace(1, put(store, 7, 100, 101)),
            trace(2, Event::Return(Buf::default(), 21_000)),
            trace(3, fee(MAKER)),
        ];
        let obs = tracker.observe(100, 4, Int::from(0xaau64), &update, &Alerts::default());
        assert_eq!(obs.updates.len(), 1);
        assert_eq!(obs.updates[0].amm, store);

        let swap = vec![
            trace(10, call(USER, AMM)),
            trace(11, call(AMM, store)), // amm STATICCALLs the store
            trace(12, get(store, 7)),
            trace(13, Event::Return(Buf::default(), 1_000)),
            trace(14, Event::Return(Buf::default(), 90_000)),
            trace(15, fee(USER)),
        ];
        let obs = tracker.observe(100, 5, Int::from(0xbbu64), &swap, &swap_alert(USER));
        assert_eq!(obs.links.len(), 1);
        assert_eq!(obs.links[0].amm, store);
        assert_eq!(obs.links[0].update.sender, MAKER);
        assert_eq!(obs.links[0].update.index, 4);
    }

    #[test]
    fn swap_writes_are_not_quote_updates() {
        let mut tracker = QuoteTracker::new([AMM]);

        // A swap tx that also writes the amm's storage (inventory update).
        let t1 = vec![
            trace(0, call(USER, AMM)),
            trace(1, put(AMM, 3, 0, 5)),
            trace(2, Event::Return(Buf::default(), 21_000)),
            trace(3, fee(USER)),
        ];
        let obs1 = tracker.observe(100, 1, Int::from(0xaau64), &t1, &swap_alert(USER));
        assert!(obs1.updates.is_empty(), "a swap is not a quote update");

        // A later swap reading that slot must NOT link back to the first swap.
        let t2 = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(AMM, 3)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs2 = tracker.observe(100, 2, Int::from(0xbbu64), &t2, &swap_alert(USER));
        assert!(obs2.links.is_empty());
    }

    #[test]
    fn latest_update_wins() {
        let mut tracker = QuoteTracker::new([AMM]);
        tracker.observe(
            100,
            1,
            Int::from(0xaau64),
            &update_tx(0),
            &Alerts::default(),
        );
        tracker.observe(
            101,
            4,
            Int::from(0xbbu64),
            &update_tx(20),
            &Alerts::default(),
        );

        let t = vec![
            trace(40, call(USER, AMM)),
            trace(41, get(AMM, 7)),
            trace(42, get(AMM, 8)),
            trace(43, Event::Return(Buf::default(), 21_000)),
            trace(44, fee(USER)),
        ];
        let obs = tracker.observe(102, 0, Int::from(0xccu64), &t, &swap_alert(USER));
        assert_eq!(obs.links.len(), 1);
        assert_eq!(obs.links[0].update.block, 101);
        assert_eq!(obs.links[0].update.index, 4);
        assert_eq!(obs.links[0].slots, 2);
    }

    #[test]
    fn untracked_contracts_are_ignored() {
        let other = addr("0x3333333333333333333333333333333333333333");
        let mut tracker = QuoteTracker::new([AMM]);
        let t = vec![
            trace(0, call(MAKER, other)),
            trace(1, put(other, 7, 0, 1)),
            trace(2, Event::Return(Buf::default(), 21_000)),
            trace(3, fee(MAKER)),
        ];
        let obs = tracker.observe(100, 1, Int::from(0xaau64), &t, &Alerts::default());
        assert!(obs.updates.is_empty());
    }

    #[test]
    fn tracks_every_contract_without_a_whitelist() {
        // A maker-specific quote store not known in advance: with `all()` the
        // update-then-consume pair still links, revealed purely by the swap's
        // storage reads.
        let store = addr("0x0109aa912b58508886a2a707204b0f8c8b164ccc");
        let mut tracker = QuoteTracker::all();

        let update = vec![
            trace(0, call(MAKER, store)),
            trace(1, put(store, 7, 100, 101)),
            trace(2, Event::Return(Buf::default(), 21_000)),
            trace(3, fee(MAKER)),
        ];
        let obs = tracker.observe(100, 1, Int::from(0xaau64), &update, &Alerts::default());
        assert_eq!(obs.updates.len(), 1);
        assert_eq!(obs.updates[0].amm, store);

        let swap = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(store, 7)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs = tracker.observe(100, 2, Int::from(0xbbu64), &swap, &swap_alert(USER));
        assert_eq!(obs.links.len(), 1);
        assert_eq!(obs.links[0].amm, store);
        assert_eq!(obs.links[0].update.sender, MAKER);
    }

    #[test]
    fn approval_tx_is_not_a_quote_update() {
        // approve() writes an allowance slot the later swap reads -- it must
        // not register as a quote update.
        let token = addr("0x4444444444444444444444444444444444444444");
        let mut tracker = QuoteTracker::all();
        let t = vec![
            trace(0, call(USER, token)),
            trace(1, put(token, 9, 0, 500)),
            trace(2, Event::Return(Buf::default(), 21_000)),
            trace(3, fee(USER)),
        ];
        let alerts = Alerts {
            erc20_approvals: vec![crate::Erc20Approval {
                token,
                owner: USER,
                spender: AMM,
                allowance: Some(Int::from(500u64)),
            }],
            ..Default::default()
        };
        let obs = tracker.observe(100, 1, Int::from(0xaau64), &t, &alerts);
        assert!(obs.updates.is_empty(), "an approval is not a quote update");

        let swap = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(token, 9)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs = tracker.observe(100, 2, Int::from(0xbbu64), &swap, &swap_alert(USER));
        assert!(obs.links.is_empty(), "a swap must not link to an approval");
    }

    #[test]
    fn evict_before_drops_old_writes() {
        let mut tracker = QuoteTracker::all();
        tracker.observe(
            100,
            1,
            Int::from(0xaau64),
            &update_tx(0),
            &Alerts::default(),
        );
        tracker.evict_before(101);

        let t = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(AMM, 7)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs = tracker.observe(150, 0, Int::from(0xbbu64), &t, &swap_alert(USER));
        assert!(obs.links.is_empty(), "evicted update must not link");
    }

    #[test]
    fn purge_from_drops_reorged_writes() {
        let mut tracker = QuoteTracker::new([AMM]);
        tracker.observe(
            100,
            1,
            Int::from(0xaau64),
            &update_tx(0),
            &Alerts::default(),
        );
        tracker.purge_from(100);

        let t = vec![
            trace(10, call(USER, AMM)),
            trace(11, get(AMM, 7)),
            trace(12, Event::Return(Buf::default(), 21_000)),
            trace(13, fee(USER)),
        ];
        let obs = tracker.observe(101, 0, Int::from(0xbbu64), &t, &swap_alert(USER));
        assert!(obs.links.is_empty(), "reorged update must not link");
    }
}
