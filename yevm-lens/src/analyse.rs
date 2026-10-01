use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use yevm_base::{Acc, Int};
use yevm_core::trace::{Event, Target, Trace};
use yevm_misc::{buf::Buf, hex::parse};

use crate::{
    Alerts, ETH, Erc20Approval, Erc20Transfer, Erc721Transfer, EthChange, FeeInfo, ForgedTransfer,
    ProxyUpgrade, Swap, SwapProtocol, TokenAmount,
};

// keccak256("Transfer(address,address,uint256)")
const TOPIC_TRANSFER: [u8; 32] =
    parse("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
// keccak256("Approval(address,address,uint256)")
const TOPIC_APPROVAL: [u8; 32] =
    parse("8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");
// keccak256("Swap(address,uint256,uint256,uint256,uint256,address)") -- Uniswap V2 pair
const TOPIC_SWAP_V2: [u8; 32] =
    parse("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
// keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)") -- Uniswap V3 pool
const TOPIC_SWAP_V3: [u8; 32] =
    parse("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
// keccak256("Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)") -- Uniswap V4 PoolManager
const TOPIC_SWAP_V4: [u8; 32] =
    parse("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
// keccak256("Swap(bytes32,address,address,uint256,uint256)") -- Balancer V2 Vault
const TOPIC_SWAP_BALANCER_V2: [u8; 32] =
    parse("2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b");
// keccak256("Swap(address,address,address,uint256,uint256,uint256,uint256)") -- Balancer V3 Vault
const TOPIC_SWAP_BALANCER_V3: [u8; 32] =
    parse("0874b2d545cb271cdbda4e093020c452328b24af12382ed62c4d00f5c26709db");
// keccak256("TokenExchange(address,int128,uint256,int128,uint256)") -- Curve stable pool
const TOPIC_EXCHANGE_CURVE: [u8; 32] =
    parse("8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140");
// keccak256("TokenExchange(address,uint256,uint256,uint256,uint256)") -- Curve crypto pool
const TOPIC_EXCHANGE_CURVE_CRYPTO: [u8; 32] =
    parse("b2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98");
// keccak256("Deposit(address,uint256)") -- WETH9-style wrap (mint)
const TOPIC_DEPOSIT: [u8; 32] =
    parse("e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c");
// keccak256("Withdrawal(address,uint256)") -- WETH9-style unwrap (burn)
const TOPIC_WITHDRAWAL: [u8; 32] =
    parse("7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65");

// ABI-encoded address: 12 zero bytes + 20-byte address
fn abi_addr(int: &Int) -> Option<Acc> {
    let b = int.as_ref();
    if b.len() != 32 {
        return None;
    }
    if b[..12] != [0u8; 12] {
        return None;
    }
    Some(Acc::from(&b[12..]))
}

// Address stored as a storage value: exactly 12 leading zero bytes, then a non-zero address.
// Requires byte[12] != 0 to reject small integers (token balances, counters) that also
// have 12+ leading zero bytes when stored as 32-byte words.
fn storage_addr(val: &Int) -> Option<Acc> {
    let b = val.as_ref();
    if b[..12] != [0u8; 12] {
        return None;
    }
    if b[12] == 0 {
        return None;
    } // rejects small integers that pad with extra zeros
    Some(Acc::from(&b[12..]))
}

// True for a 20-byte Ethereum address stored as a topic (> 2^144).
// Filters out 18-byte position IDs (Uniswap v4 etc.) which max out below 2^144.
fn is_addr_topic(int: &Int) -> bool {
    let b = int.as_ref();
    if b.len() != 32 {
        return false;
    }
    // bytes 12..32 must not all be zero (non-null address)
    if b[12..] == [0u8; 20] {
        return false;
    }
    // the first non-zero byte must be at position 12 (12 leading zero bytes)
    b[..12] == [0u8; 12]
}

struct BalanceWrite {
    // Up to two holder candidates: Solidity hashes (holder, slot), Vyper
    // hashes (slot, holder). The log cross-check picks the right one.
    holders: [Option<Acc>; 2],
    contract: Acc,
    sign: Ordering, // balance delta direction (Less = decreased)
    log_matched: bool,
}

// A confirmed, directional value move used for swap reconstruction: native ETH
// call values and WETH-style wrap/unwrap legs. (Confirmed ERC-20 transfers are
// kept in `alerts.erc20_transfers` and merged in later.)
struct Flow {
    from: Acc,
    to: Acc,
    token: Acc,
    amount: Int,
    dead: bool, // the carrying frame reverted; the value transfer was undone
}

// One call-stack entry: the executing address, and the index of the ETH flow
// this frame's call value created (cancelled if the frame reverts).
struct Frame {
    addr: Acc,
    eth_flow: Option<usize>,
}

// Collect Undo ranges: traces stream out before reverts are applied, so the
// `reverted` flag on arriving traces is unreliable; the Undo events carry the
// authoritative [from, to) seq ranges.
pub(crate) fn undone_ranges(traces: &[Trace]) -> Vec<(usize, usize)> {
    let mut undone = Vec::new();
    for t in traces {
        if let Event::Undo(from, to) = &t.event {
            undone.push((*from, *to));
        }
    }
    undone
}

// A trace neither pre-marked reverted nor covered by an Undo range.
pub(crate) fn is_live(t: &Trace, undone: &[(usize, usize)]) -> bool {
    !t.reverted
        && !undone
            .iter()
            .any(|(from, to)| t.seq >= *from && t.seq < *to)
}

pub fn analyse(traces: &[Trace]) -> Alerts {
    let undone = undone_ranges(traces);
    let live = |t: &Trace| is_live(t, &undone);

    // Pass 1: collect hash preimages (ERC-20 balance slot identification) and
    // the set of addresses that were actually called or had their code fetched.
    // The latter is used to confirm proxy implementation swaps: if the old impl
    // was never called/loaded, the storage write is likely a plain state update.
    let mut preimages: HashMap<Int, [Option<Acc>; 2]> = Default::default();
    let mut interacted: HashSet<Acc> = Default::default();
    for t in traces {
        if !live(t) {
            continue;
        }
        match &t.event {
            Event::Hash(input, output) => {
                let b = input.as_slice();
                if b.len() == 64 {
                    let mut holders = [None, None];
                    // Solidity mapping(address => uint): keccak(holder . slot)
                    if b[..12] == [0u8; 12] {
                        let h = Acc::from(&b[12..32]);
                        if h != Acc::default() {
                            holders[0] = Some(h);
                        }
                    }
                    // Vyper HashMap[address, uint]: keccak(slot . holder)
                    if b[32..44] == [0u8; 12] {
                        let h = Acc::from(&b[44..64]);
                        if h != Acc::default() {
                            holders[1] = Some(h);
                        }
                    }
                    if holders.iter().any(Option::is_some) {
                        preimages.insert(*output, holders);
                    }
                }
            }
            Event::Call(call, _) => {
                if let Some(to) = call.to {
                    interacted.insert(to);
                }
            }
            Event::Get(Target::Code { acc, .. }) => {
                interacted.insert(*acc);
            }
            _ => {}
        }
    }

    let mut alerts = Alerts::default();
    let mut balance_writes: Vec<BalanceWrite> = Vec::new();
    let mut ctx_stack: Vec<Frame> = Vec::new();

    // Swap reconstruction inputs. We do NOT trust Swap log events to mean a
    // swap happened -- any contract can emit one. We only record (emitter,
    // protocol) for each, and later cross-check them against confirmed token
    // flows; the swap itself is derived from flows. `ancestry` (who was on the
    // call stack when an address was entered), `first_entered` (entry order)
    // and `active` (executed something in its own frame, so not a plain
    // ETH-receiving EOA) feed the pool-vs-swapper classification; `flows`
    // carries the non-ERC-20 legs (native ETH call values, WETH-style wraps).
    let mut swap_logs: Vec<(Acc, SwapProtocol)> = Vec::new();
    let mut first_caller: Option<Acc> = None;
    let mut flows: Vec<Flow> = Vec::new();
    let mut ancestry: HashSet<(Acc, Acc)> = Default::default();
    let mut first_entered: HashMap<Acc, usize> = Default::default();
    let mut active: HashSet<Acc> = Default::default();

    for t in traces {
        // The call structure is tracked for ALL traces: a reverting frame's own
        // Call event lands outside its Undo range while its closing Revert
        // lands inside, so filtering pushes/pops by liveness would desync the
        // stack. State- and flow-effecting events are filtered below instead.

        // A frame that writes storage, hashes, logs or calls out executed real
        // code. Value-transfer Puts are excluded: they stream between a Call
        // and its frame's first own event, and would mark plain ETH recipients.
        let executes = matches!(
            &t.event,
            Event::Log(..)
                | Event::Hash(..)
                | Event::Call(..)
                | Event::Put(Target::Store { .. }, _)
        );
        if executes
            && live(t)
            && let Some(f) = ctx_stack.last()
        {
            active.insert(f.addr);
        }

        match &t.event {
            Event::Call(call, mode) => {
                use yevm_core::evm::CallMode;
                if first_caller.is_none() {
                    first_caller = Some(call.by);
                }
                let exec_addr = match mode {
                    CallMode::Delegate(..) | CallMode::CallCode(..) => {
                        ctx_stack.last().map(|f| f.addr).unwrap_or(call.by)
                    }
                    CallMode::Create(addr) | CallMode::Create2(addr) => *addr,
                    _ => call.to.unwrap_or(call.by),
                };
                let mut frame = Frame {
                    addr: exec_addr,
                    eth_flow: None,
                };
                if live(t) {
                    let idx = first_entered.len();
                    first_entered.entry(exec_addr).or_insert(idx);
                    for f in &ctx_stack {
                        if f.addr != exec_addr {
                            ancestry.insert((f.addr, exec_addr));
                        }
                    }
                    // Native ETH leg: plain CALL (and CREATE) values move ETH.
                    // Delegate/callcode/static transfer nothing.
                    let moves_eth = matches!(
                        mode,
                        CallMode::Call(..) | CallMode::Create(_) | CallMode::Create2(_)
                    );
                    if moves_eth && !call.eth.is_zero() && call.by != exec_addr {
                        frame.eth_flow = Some(flows.len());
                        flows.push(Flow {
                            from: call.by,
                            to: exec_addr,
                            token: ETH,
                            amount: call.eth,
                            dead: false,
                        });
                    }
                }
                ctx_stack.push(frame);
            }

            Event::Return(..) => {
                ctx_stack.pop();
            }

            Event::Revert(..) | Event::Halt(..) => {
                // The frame's call value is undone together with the frame.
                if let Some(frame) = ctx_stack.pop()
                    && let Some(i) = frame.eth_flow
                {
                    flows[i].dead = true;
                }
            }

            _ if !live(t) => {}

            Event::Put(target, next) => {
                match target {
                    Target::Store { acc, key, val } => {
                        // Proxy implementation swap: slot 0 (or any slot) changes from one
                        // address-like value to another.
                        if let (Some(old_impl), Some(new_impl)) =
                            (storage_addr(val), storage_addr(next))
                        {
                            // Require the old impl to have been called or code-loaded:
                            // real proxy upgrades route through the old impl first.
                            if old_impl != new_impl && interacted.contains(&old_impl) {
                                alerts.proxy_upgrades.push(ProxyUpgrade {
                                    proxy: *acc,
                                    slot: *key,
                                    old_impl,
                                    new_impl,
                                });
                            }
                        }

                        // ERC-20 balance write: storage key matches a known hash preimage
                        if let Some(holders) = preimages.get(key) {
                            let sign = next.cmp(val);
                            if sign != Ordering::Equal {
                                balance_writes.push(BalanceWrite {
                                    holders: *holders,
                                    contract: *acc,
                                    sign,
                                    log_matched: false,
                                });
                            }
                        }
                    }

                    Target::Value { acc, val }
                        // ETH balance change (skip fee-only dust moves)
                        if val != next => {
                            alerts.eth_changes.push(EthChange {
                                acc: *acc,
                                before: *val,
                                after: *next,
                            });
                        }

                    _ => {}
                }
            }

            Event::Fee(sender, coinbase, _, _, gas) => {
                alerts.fee = Some(FeeInfo {
                    sender: *sender,
                    coinbase: *coinbase,
                    gas_used: *gas,
                });
            }

            Event::Log(topics, payload) => {
                let emitter = ctx_stack.last().map(|f| f.addr).unwrap_or_default();

                if topics.is_empty() {
                    continue;
                }
                let t0 = topics[0].as_ref();

                // Swap topic: record the emitter as a *candidate* pool. Whether
                // it was a real swap is decided later from token flows, not here.
                let kind = if t0 == TOPIC_SWAP_V2 {
                    Some(SwapProtocol::UniswapV2)
                } else if t0 == TOPIC_SWAP_V3 {
                    Some(SwapProtocol::UniswapV3)
                } else if t0 == TOPIC_SWAP_V4 {
                    Some(SwapProtocol::UniswapV4)
                } else if t0 == TOPIC_SWAP_BALANCER_V2 || t0 == TOPIC_SWAP_BALANCER_V3 {
                    Some(SwapProtocol::Balancer)
                } else if t0 == TOPIC_EXCHANGE_CURVE || t0 == TOPIC_EXCHANGE_CURVE_CRYPTO {
                    Some(SwapProtocol::Curve)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    swap_logs.push((emitter, kind));
                    continue;
                }

                // ERC-721 Transfer: 4 topics, empty payload
                if t0 == TOPIC_TRANSFER
                    && topics.len() == 4
                    && (payload.as_slice().is_empty() || payload.as_slice() == [0u8; 32])
                {
                    let from = abi_addr(&topics[1]);
                    let to = abi_addr(&topics[2]);
                    if let (Some(from), Some(to)) = (from, to) {
                        let zero = Acc::default();
                        if is_addr_topic(&topics[1])
                            || from == zero
                            || is_addr_topic(&topics[2])
                            || to == zero
                        {
                            let token_id = {
                                let b = topics[3].as_ref();
                                if b.len() <= 32 { Some(topics[3]) } else { None }
                            };
                            alerts.erc721_transfers.push(Erc721Transfer {
                                token: emitter,
                                from,
                                to,
                                token_id,
                            });
                        }
                    }
                    continue;
                }

                // WETH-style wrap/unwrap: Deposit(dst, wad) mints, Withdrawal
                // (src, wad) burns. Confirmed by a balance write, like Transfer;
                // the native ETH counter-leg arrives via the call values.
                if (t0 == TOPIC_DEPOSIT || t0 == TOPIC_WITHDRAWAL) && topics.len() == 2 {
                    if let (Some(who), Some(amount)) = (abi_addr(&topics[1]), buf_to_int(payload)) {
                        let sign = if t0 == TOPIC_DEPOSIT {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        };
                        let w = balance_writes.iter_mut().find(|w| {
                            !w.log_matched
                                && w.contract == emitter
                                && w.holders.contains(&Some(who))
                                && w.sign == sign
                        });
                        if let Some(w) = w {
                            w.log_matched = true;
                            let (from, to) = if t0 == TOPIC_DEPOSIT {
                                (emitter, who)
                            } else {
                                (who, emitter)
                            };
                            flows.push(Flow {
                                from,
                                to,
                                token: emitter,
                                amount,
                                dead: false,
                            });
                        }
                    }
                    continue;
                }

                // ERC-20 Transfer: 3 topics
                if t0 == TOPIC_TRANSFER && topics.len() == 3 {
                    let from = abi_addr(&topics[1]);
                    let to = abi_addr(&topics[2]);
                    if let (Some(from), Some(to)) = (from, to) {
                        let amount = buf_to_int(payload);
                        // Find matching balance writes to confirm state change
                        let from_w = balance_writes.iter_mut().find(|w| {
                            !w.log_matched
                                && w.contract == emitter
                                && w.holders.contains(&Some(from))
                                && w.sign == Ordering::Less
                        });
                        let has_from = from_w.is_some();
                        if let Some(w) = from_w {
                            w.log_matched = true;
                        }

                        let to_w = balance_writes.iter_mut().find(|w| {
                            !w.log_matched
                                && w.contract == emitter
                                && w.holders.contains(&Some(to))
                                && w.sign == Ordering::Greater
                        });
                        let has_to = to_w.is_some();
                        if let Some(w) = to_w {
                            w.log_matched = true;
                        }

                        if has_from || has_to {
                            alerts.erc20_transfers.push(Erc20Transfer {
                                token: emitter,
                                from,
                                to,
                                amount,
                            });
                        } else {
                            alerts.forged_transfers.push(ForgedTransfer {
                                token: emitter,
                                from,
                                to,
                            });
                        }
                    }
                    continue;
                }

                // ERC-20 Approval: 3+ topics
                if t0 == TOPIC_APPROVAL && topics.len() >= 3 {
                    let owner = abi_addr(&topics[1]);
                    let spender = abi_addr(&topics[2]);
                    if let (Some(owner), Some(spender)) = (owner, spender) {
                        alerts.erc20_approvals.push(Erc20Approval {
                            token: emitter,
                            owner,
                            spender,
                            allowance: buf_to_int(payload),
                        });
                    }
                }
            }

            _ => {}
        }
    }

    // Reconstruct swaps from CONFIRMED, balance-verified flows -- never from
    // Swap log events alone, which any contract can emit. Swap logs only
    // *label* the protocol, and are cross-checked: a Swap log whose emitter
    // shows no real token-for-token flow is reported as `unverified_swaps`.
    //
    // Every account that net-converted one token into another is a *candidate*:
    // that shape fits both an AMM pool and a contract-held swapper (a bot or a
    // settlement contract trading its own funds). The call graph tells them
    // apart -- a swapper initiates calls into its pools, a pool is on the
    // receiving end -- so the swapper is derived, not assumed to be the tx
    // sender.
    {
        let add = yevm_base::math::lift(|[a, b]| a + b);
        let sub = yevm_base::math::lift(|[a, b]| a - b);
        // Keyed (holder, token). Built from confirmed erc20_transfers plus the
        // native-ETH and wrap/unwrap flows, so forged Transfer logs (no backing
        // balance write) never enter the flow.
        let mut inflow: HashMap<(Acc, Acc), Int> = Default::default();
        let mut outflow: HashMap<(Acc, Acc), Int> = Default::default();
        let mut appearance: Vec<Acc> = Vec::new();
        let mut touched: HashSet<(Acc, Acc)> = Default::default(); // direct flow counterparties
        let moves = alerts
            .erc20_transfers
            .iter()
            .filter_map(|tr| tr.amount.map(|amt| (tr.from, tr.to, tr.token, amt)))
            .chain(
                flows
                    .iter()
                    .filter(|f| !f.dead)
                    .map(|f| (f.from, f.to, f.token, f.amount)),
            );
        for (from, to, token, amt) in moves {
            for h in [from, to] {
                if !appearance.contains(&h) {
                    appearance.push(h);
                }
            }
            touched.insert((from, to));
            touched.insert((to, from));
            let o = outflow.entry((from, token)).or_insert(Int::ZERO);
            *o = add([*o, amt]);
            let i = inflow.entry((to, token)).or_insert(Int::ZERO);
            *i = add([*i, amt]);
        }

        // Net (sold, bought) for one holder from the flow maps.
        let net = |holder: Acc| -> (Vec<TokenAmount>, Vec<TokenAmount>) {
            let mut tokens: Vec<Acc> = Vec::new();
            for (h, tok) in inflow.keys().chain(outflow.keys()) {
                if *h == holder && !tokens.contains(tok) {
                    tokens.push(*tok);
                }
            }
            tokens.sort();
            let (mut sold, mut bought) = (Vec::new(), Vec::new());
            for tok in tokens {
                let i = inflow.get(&(holder, tok)).copied().unwrap_or(Int::ZERO);
                let o = outflow.get(&(holder, tok)).copied().unwrap_or(Int::ZERO);
                if o > i {
                    sold.push(TokenAmount {
                        token: tok,
                        amount: sub([o, i]),
                    });
                } else if i > o {
                    bought.push(TokenAmount {
                        token: tok,
                        amount: sub([i, o]),
                    });
                }
            }
            (sold, bought)
        };

        let sender = alerts.fee.as_ref().map(|f| f.sender).or(first_caller);

        // Candidates: converted one token into another. Routers net to zero
        // per token and drop out on their own. The zero address is a mint/burn
        // sink, never a party to a swap.
        let candidates: Vec<Acc> = appearance
            .iter()
            .copied()
            .filter(|a| *a != Acc::default())
            .filter(|a| {
                let (sold, bought) = net(*a);
                !sold.is_empty() && !bought.is_empty()
            })
            .collect();

        // X dominates Y when X was on the call stack when Y was entered, and Y
        // was not likewise above X -- on a cycle (a pool calling back into its
        // payer, a V4 unlock callback) the one entered FIRST is upstream. Entry
        // order is unambiguous, unlike trace depth.
        let first = |a: &Acc| first_entered.get(a).copied().unwrap_or(usize::MAX);
        let dominates = |x: &Acc, y: &Acc| {
            ancestry.contains(&(*x, *y)) && (!ancestry.contains(&(*y, *x)) || first(x) < first(y))
        };

        // Classify each candidate. The tx sender and passive accounts (never
        // executed code in their own frame -- EOA holders, plain ETH
        // recipients) cannot be pools; a verified Swap log emitter always is
        // one. Otherwise: dominated by an active peer candidate -> pool (the
        // peer is the swapper above it); dominating an active peer -> swapper;
        // no relation to any peer but called during the tx -> a called contract
        // that converted tokens, i.e. a pool (wrappers land here).
        let (mut pools, mut swappers): (Vec<Acc>, Vec<Acc>) = (Vec::new(), Vec::new());
        for c in &candidates {
            let peers = || candidates.iter().filter(|x| *x != c && active.contains(x));
            let is_pool = if Some(*c) == sender || !active.contains(c) {
                false
            } else if swap_logs.iter().any(|(e, _)| e == c) || peers().any(|x| dominates(x, c)) {
                true
            } else if peers().any(|x| dominates(c, x)) {
                false
            } else {
                first_entered.contains_key(c)
            };
            if is_pool {
                pools.push(*c);
            } else {
                swappers.push(*c);
            }
        }

        // Cross-check Swap logs against the flow-derived pools: matches set the
        // protocol label; any that don't correspond to a real swap are flagged.
        for (emitter, _) in &swap_logs {
            if !pools.contains(emitter) && !alerts.unverified_swaps.contains(emitter) {
                alerts.unverified_swaps.push(*emitter);
            }
        }
        let protocol_of = |sel: &[Acc]| {
            let mut kinds: Vec<SwapProtocol> = Vec::new();
            for (emitter, kind) in &swap_logs {
                if sel.contains(emitter) && !kinds.contains(kind) {
                    kinds.push(*kind);
                }
            }
            match kinds.as_slice() {
                [] => SwapProtocol::Unknown,
                [one] => *one,
                _ => SwapProtocol::Mixed,
            }
        };

        // One Swap per derived swapper. Pools are attributed by call ancestry;
        // for the tx sender (an EOA makes no calls of its own) the whole route
        // is theirs; a passive NON-sender swapper is a counterparty filling in
        // place (an RFQ maker, a settled batch order), so it only gets pools it
        // directly traded with -- usually none.
        if !pools.is_empty() {
            for sw in &swappers {
                let mine: Vec<Acc> = pools
                    .iter()
                    .copied()
                    .filter(|p| ancestry.contains(&(*sw, *p)))
                    .collect();
                let s_pools = if !mine.is_empty() {
                    mine
                } else if Some(*sw) == sender {
                    pools.clone()
                } else {
                    pools
                        .iter()
                        .copied()
                        .filter(|p| touched.contains(&(*sw, *p)))
                        .collect()
                };
                let (sold, bought) = net(*sw);
                let protocol = protocol_of(&s_pools);
                let legs = s_pools.len();
                alerts.swaps.push(Swap {
                    swapper: *sw,
                    recipient: *sw,
                    sold,
                    bought,
                    pools: s_pools,
                    legs,
                    protocol,
                });
            }
        }

        // Payer and recipient can be separate accounts (router `to` params,
        // relayed swaps, fee-skimming payouts): the payer is one-sided sold,
        // the recipient one-sided bought, and neither is a candidate. Pair them
        // around the pools. This runs alongside candidate swappers -- their
        // fills (e.g. an RFQ maker's) do not tell the sender's own swap.
        if !pools.is_empty() {
            let sender_acc = sender.unwrap_or_default();
            let coinbase = alerts.fee.as_ref().map(|f| f.coinbase);
            // Is `tok` a token the route itself converts -- taken in (inbound)
            // or given out by one of its pools or counterparty fills?
            let route_token = |tok: &Acc, inbound: bool| {
                let map = if inbound { &inflow } else { &outflow };
                map.iter().any(|((h, t), amt)| {
                    t == tok && *amt != Int::ZERO && (pools.contains(h) || swappers.contains(h))
                })
            };
            // The one-sided account with the largest single net amount on the
            // given side; dwarfs fee-skim outputs of the same route. In strict
            // mode (relayed pairing) the account must trade a token the route
            // converts, and the fee payer and coinbase are excluded: gas
            // refunds and bribes are not swap legs.
            let largest_one_sided =
                |sold_side: bool, skip: Option<Acc>, strict: bool| -> Option<Acc> {
                    let mut best: Option<(Acc, Int)> = None;
                    for a in &appearance {
                        if *a == Acc::default()
                            || pools.contains(a)
                            || swappers.contains(a)
                            || Some(*a) == skip
                            || (strict && (*a == sender_acc || Some(*a) == coinbase))
                        {
                            continue;
                        }
                        let (sold, bought) = net(*a);
                        let side = if sold_side {
                            if !bought.is_empty() {
                                continue;
                            }
                            &sold
                        } else {
                            if !sold.is_empty() {
                                continue;
                            }
                            &bought
                        };
                        for ta in side {
                            if strict && !route_token(&ta.token, sold_side) {
                                continue;
                            }
                            if best.map(|(_, b)| ta.amount > b).unwrap_or(true) {
                                best = Some((*a, ta.amount));
                            }
                        }
                    }
                    best.map(|(a, _)| a)
                };
            let (payer, relayed) = if swappers.contains(&sender_acc) {
                // the sender's own swap is already reported above
                (None, false)
            } else if !net(sender_acc).0.is_empty() {
                (Some(sender_acc), false)
            } else if swappers.is_empty() {
                let p = largest_one_sided(true, None, false).unwrap_or(sender_acc);
                (Some(p), false)
            } else {
                // Relayed routes (ERC-4337 bundles, meta-txs): the fee payer
                // is a bundler/relayer and the real payer is a one-sided
                // account elsewhere in the flow.
                (largest_one_sided(true, None, true), true)
            };
            if let Some(payer) = payer {
                let recipient = if !net(payer).1.is_empty() {
                    payer
                } else {
                    largest_one_sided(false, Some(payer), relayed).unwrap_or(payer)
                };
                let sold = net(payer).0;
                let bought = net(recipient).1;
                // A relayed pair needs both ends -- a lone one-sided flow next
                // to a candidate's fill is a payout or a skim, not a swap.
                let emit = if relayed {
                    !sold.is_empty() && !bought.is_empty()
                } else {
                    !sold.is_empty() || !bought.is_empty()
                };
                if emit {
                    let protocol = protocol_of(&pools);
                    let legs = pools.len();
                    alerts.swaps.push(Swap {
                        swapper: payer,
                        recipient,
                        sold,
                        bought,
                        pools,
                        legs,
                        protocol,
                    });
                }
            }
        }
    }

    alerts
}

fn buf_to_int(b: &Buf) -> Option<Int> {
    let s = b.as_slice();
    if s.is_empty() || s.len() > 32 {
        return None;
    }
    Some(Int::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yevm_core::{evm::CallMode, trace::Trace};
    use yevm_misc::buf::Buf;

    fn trace(seq: usize, event: Event) -> Trace {
        Trace {
            seq,
            event,
            depth: 0,
            reverted: false,
        }
    }

    fn reverted(seq: usize, event: Event) -> Trace {
        Trace {
            seq,
            event,
            depth: 0,
            reverted: true,
        }
    }

    fn addr(hex: &str) -> Acc {
        let b = hex::decode(hex.trim_start_matches("0x")).unwrap();
        Acc::from(b.as_slice())
    }

    // Encode an address as an ABI topic (32 bytes: 12 zeros + 20-byte address)
    fn topic(a: &Acc) -> Int {
        let mut t = [0u8; 32];
        t[12..].copy_from_slice(a.as_ref());
        Int::from(t.as_ref())
    }

    fn call_ctx(by: Acc, to: Acc) -> Event {
        call_ctx_eth(by, to, Int::ZERO)
    }

    fn call_ctx_eth(by: Acc, to: Acc, eth: Int) -> Event {
        Event::Call(
            yevm_core::Call {
                by,
                to: Some(to),
                gas: 100_000,
                eth,
                data: Buf::default(),
            },
            CallMode::Call(0, 0),
        )
    }

    fn ret() -> Event {
        Event::Return(Buf::default(), 21_000)
    }

    fn addr_as_storage(a: &Acc) -> Int {
        let mut v = [0u8; 32];
        v[12..].copy_from_slice(a.as_ref());
        Int::from(v.as_ref())
    }

    #[test]
    fn detects_proxy_upgrade() {
        let old_impl = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let new_impl = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let proxy = addr("0xcccccccccccccccccccccccccccccccccccccccc");

        // A real proxy upgrade: the old impl is called (or code-loaded) before the slot changes.
        let traces = vec![
            trace(
                0,
                Event::Get(Target::Code {
                    acc: old_impl,
                    hash: Int::ZERO,
                }),
            ),
            trace(
                1,
                Event::Put(
                    Target::Store {
                        acc: proxy,
                        key: Int::from(0u64),
                        val: addr_as_storage(&old_impl),
                    },
                    addr_as_storage(&new_impl),
                ),
            ),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.proxy_upgrades.len(), 1);
        assert_eq!(alerts.proxy_upgrades[0].old_impl, old_impl);
        assert_eq!(alerts.proxy_upgrades[0].new_impl, new_impl);
        assert_eq!(alerts.proxy_upgrades[0].proxy, proxy);
    }

    #[test]
    fn no_proxy_swap_without_interaction() {
        let old_impl = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let new_impl = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let proxy = addr("0xcccccccccccccccccccccccccccccccccccccccc");

        // Old impl never called or code-fetched → plain state update, not a proxy swap.
        let traces = vec![trace(
            0,
            Event::Put(
                Target::Store {
                    acc: proxy,
                    key: Int::from(0u64),
                    val: addr_as_storage(&old_impl),
                },
                addr_as_storage(&new_impl),
            ),
        )];

        assert_eq!(analyse(&traces).proxy_upgrades.len(), 0);
    }

    #[test]
    fn no_proxy_swap_when_not_changing() {
        let proxy = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        let impl_addr = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let traces = vec![trace(
            0,
            Event::Put(
                Target::Store {
                    acc: proxy,
                    key: Int::from(0u64),
                    val: addr_as_storage(&impl_addr),
                },
                addr_as_storage(&impl_addr), // same → no swap
            ),
        )];
        assert_eq!(analyse(&traces).proxy_upgrades.len(), 0);
    }

    #[test]
    fn skips_reverted_traces() {
        let proxy = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        let old_impl = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let new_impl = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let traces = vec![reverted(
            0,
            Event::Put(
                Target::Store {
                    acc: proxy,
                    key: Int::from(0u64),
                    val: addr_as_storage(&old_impl),
                },
                addr_as_storage(&new_impl),
            ),
        )];
        assert_eq!(analyse(&traces).proxy_upgrades.len(), 0);
    }

    // Build a Hash trace + balance Store trace for one ERC-20 holder at mapping slot 0.
    fn balance_traces(seq: &mut usize, token: Acc, holder: Acc, old: u64, new: u64) -> Vec<Trace> {
        use yevm_misc::keccak256;
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(holder.as_ref());
        let slot_hash = Int::from(keccak256(&preimage).as_ref());
        let hash_trace = trace(*seq, Event::Hash(Buf::from(preimage.to_vec()), slot_hash));
        *seq += 1;
        let put_trace = trace(
            *seq,
            Event::Put(
                Target::Store {
                    acc: token,
                    key: slot_hash,
                    val: Int::from(old),
                },
                Int::from(new),
            ),
        );
        *seq += 1;
        vec![hash_trace, put_trace]
    }

    fn transfer_log(seq: usize, from: &Acc, to: &Acc, amount: u64) -> Trace {
        let mut payload = [0u8; 32];
        payload[24..].copy_from_slice(&amount.to_be_bytes());
        trace(
            seq,
            Event::Log(
                vec![Int::from(TOPIC_TRANSFER.as_ref()), topic(from), topic(to)],
                Buf::from(payload.to_vec()),
            ),
        )
    }

    fn transfer4_log(seq: usize, from: &Acc, to: &Acc, token_id: u64) -> Trace {
        trace(
            seq,
            Event::Log(
                vec![
                    Int::from(TOPIC_TRANSFER.as_ref()),
                    topic(from),
                    topic(to),
                    Int::from(token_id),
                ],
                Buf::default(),
            ),
        )
    }

    fn approval_log(seq: usize, owner: &Acc, spender: &Acc, allowance: &Int) -> Trace {
        trace(
            seq,
            Event::Log(
                vec![
                    Int::from(TOPIC_APPROVAL.as_ref()),
                    topic(owner),
                    topic(spender),
                ],
                Buf::from(allowance.as_ref().to_vec()),
            ),
        )
    }

    #[test]
    fn detects_erc20_transfer_with_state() {
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let mut seq = 0;
        let mut traces = vec![trace(seq, call_ctx(from, token))];
        seq += 1;
        traces.extend(balance_traces(&mut seq, token, from, 2000, 1000)); // delta < 0
        traces.push(transfer_log(seq, &from, &to, 1000));
        seq += 1;
        traces.push(trace(seq, ret()));

        let alerts = analyse(&traces);
        assert_eq!(
            alerts.erc20_transfers.len(),
            1,
            "expected 1 ERC-20 transfer"
        );
        assert_eq!(alerts.forged_transfers.len(), 0);
        let t = &alerts.erc20_transfers[0];
        assert_eq!(t.token, token);
        assert_eq!(t.from, from);
        assert_eq!(t.to, to);
        assert_eq!(t.amount, Some(Int::from(1000u64)));
    }

    #[test]
    fn detects_erc20_transfer_both_sides() {
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let mut seq = 0;
        let mut traces = vec![trace(seq, call_ctx(from, token))];
        seq += 1;
        traces.extend(balance_traces(&mut seq, token, from, 5000, 4000)); // sender loses 1000
        traces.extend(balance_traces(&mut seq, token, to, 1000, 2000)); // receiver gains 1000
        traces.push(transfer_log(seq, &from, &to, 1000));
        seq += 1;
        traces.push(trace(seq, ret()));

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc20_transfers.len(), 1);
        assert_eq!(alerts.forged_transfers.len(), 0);
    }

    #[test]
    fn flags_forged_transfer() {
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let traces = vec![
            trace(0, call_ctx(from, token)),
            transfer_log(1, &from, &to, 1000), // no balance write → forged
            trace(2, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc20_transfers.len(), 0);
        assert_eq!(alerts.forged_transfers.len(), 1);
        assert_eq!(alerts.forged_transfers[0].token, token);
    }

    #[test]
    fn detects_erc20_approval() {
        let token = addr("0x1111111111111111111111111111111111111111");
        let owner = addr("0x2222222222222222222222222222222222222222");
        let spender = addr("0x3333333333333333333333333333333333333333");
        let allowance = Int::from(500u64);

        let traces = vec![
            trace(0, call_ctx(owner, token)),
            approval_log(1, &owner, &spender, &allowance),
            trace(2, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc20_approvals.len(), 1);
        let a = &alerts.erc20_approvals[0];
        assert_eq!(a.token, token);
        assert_eq!(a.owner, owner);
        assert_eq!(a.spender, spender);
        assert_eq!(a.allowance, Some(allowance));
    }

    #[test]
    fn detects_erc20_approval_unlimited() {
        let token = addr("0x1111111111111111111111111111111111111111");
        let owner = addr("0x2222222222222222222222222222222222222222");
        let spender = addr("0x3333333333333333333333333333333333333333");

        let traces = vec![
            trace(0, call_ctx(owner, token)),
            approval_log(1, &owner, &spender, &Int::MAX),
            trace(2, ret()),
        ];

        let a = &analyse(&traces).erc20_approvals[0];
        assert_eq!(a.allowance, Some(Int::MAX));
    }

    #[test]
    fn detects_erc721_mint() {
        let nft = addr("0x1111111111111111111111111111111111111111");
        let minter = addr("0x2222222222222222222222222222222222222222");
        let zero = Acc::default();

        let traces = vec![
            trace(0, call_ctx(minter, nft)),
            transfer4_log(1, &zero, &minter, 42),
            trace(2, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc721_transfers.len(), 1);
        let t = &alerts.erc721_transfers[0];
        assert_eq!(t.token, nft);
        assert_eq!(t.from, zero);
        assert_eq!(t.to, minter);
        assert_eq!(t.token_id, Some(Int::from(42u64)));
    }

    #[test]
    fn detects_erc721_transfer() {
        let nft = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let traces = vec![
            trace(0, call_ctx(from, nft)),
            transfer4_log(1, &from, &to, 9999),
            trace(2, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc721_transfers.len(), 1);
        assert_eq!(
            alerts.erc721_transfers[0].token_id,
            Some(Int::from(9999u64))
        );
        assert_eq!(alerts.erc721_transfers[0].from, from);
        assert_eq!(alerts.erc721_transfers[0].to, to);
    }

    #[test]
    fn detects_eth_change() {
        let acc = addr("0x2222222222222222222222222222222222222222");
        let traces = vec![trace(
            0,
            Event::Put(
                Target::Value {
                    acc,
                    val: Int::from(1_000_000_000_000_000_000u64),
                },
                Int::from(2_000_000_000_000_000_000u64),
            ),
        )];
        let alerts = analyse(&traces);
        assert_eq!(alerts.eth_changes.len(), 1);
        assert_eq!(alerts.eth_changes[0].acc, acc);
        assert_eq!(
            alerts.eth_changes[0].before,
            Int::from(1_000_000_000_000_000_000u64)
        );
        assert_eq!(
            alerts.eth_changes[0].after,
            Int::from(2_000_000_000_000_000_000u64)
        );
    }

    #[test]
    fn no_eth_change_when_unchanged() {
        let acc = addr("0x2222222222222222222222222222222222222222");
        let val = Int::from(1_000u64);
        let traces = vec![trace(0, Event::Put(Target::Value { acc, val }, val))];
        assert_eq!(analyse(&traces).eth_changes.len(), 0);
    }

    #[test]
    fn captures_fee_info() {
        let sender = addr("0x2222222222222222222222222222222222222222");
        let coinbase = addr("0x3333333333333333333333333333333333333333");
        let traces = vec![trace(
            0,
            Event::Fee(sender, coinbase, Int::ZERO, Int::ZERO, 21_000),
        )];
        let alerts = analyse(&traces);
        assert!(alerts.fee.is_some());
        let fee = alerts.fee.unwrap();
        assert_eq!(fee.sender, sender);
        assert_eq!(fee.coinbase, coinbase);
        assert_eq!(fee.gas_used, 21_000);
    }

    #[test]
    fn delegatecall_emitter_is_caller_not_implementation() {
        let proxy = addr("0x1111111111111111111111111111111111111111");
        let implementation = addr("0x2222222222222222222222222222222222222222");
        let user = addr("0x3333333333333333333333333333333333333333");
        let spender = addr("0x4444444444444444444444444444444444444444");

        let traces = vec![
            trace(0, call_ctx(user, proxy)),
            trace(
                1,
                Event::Call(
                    yevm_core::Call {
                        by: proxy,
                        to: Some(implementation),
                        gas: 80_000,
                        eth: Int::ZERO,
                        data: Buf::default(),
                    },
                    CallMode::Delegate(0, 0),
                ),
            ),
            // Approval emitted inside delegatecall — token must be proxy, not implementation
            approval_log(2, &user, &spender, &Int::from(100u64)),
            trace(3, ret()),
            trace(4, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc20_approvals.len(), 1);
        assert_eq!(alerts.erc20_approvals[0].token, proxy);
    }

    fn swap_v2_log(seq: usize, pool_ctx: &Acc) -> Trace {
        // topic0 only is inspected; the emitter comes from the call context.
        let _ = pool_ctx;
        trace(
            seq,
            Event::Log(vec![Int::from(TOPIC_SWAP_V2.as_ref())], Buf::default()),
        )
    }

    fn swap_v3_log(seq: usize) -> Trace {
        trace(
            seq,
            Event::Log(vec![Int::from(TOPIC_SWAP_V3.as_ref())], Buf::default()),
        )
    }

    fn fee(seq: usize, sender: &Acc) -> Trace {
        let coinbase = addr("0x9999999999999999999999999999999999999999");
        trace(
            seq,
            Event::Fee(*sender, coinbase, Int::ZERO, Int::ZERO, 100_000),
        )
    }

    #[test]
    fn swap_topic_hashes_match() {
        use yevm_misc::keccak256;
        assert_eq!(
            keccak256("Swap(address,uint256,uint256,uint256,uint256,address)".as_bytes()).as_ref(),
            &TOPIC_SWAP_V2[..],
            "V2 Swap topic",
        );
        assert_eq!(
            keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)".as_bytes())
                .as_ref(),
            &TOPIC_SWAP_V3[..],
            "V3 Swap topic",
        );
        assert_eq!(
            keccak256("Deposit(address,uint256)".as_bytes()).as_ref(),
            &TOPIC_DEPOSIT[..],
            "Deposit topic",
        );
        assert_eq!(
            keccak256("Withdrawal(address,uint256)".as_bytes()).as_ref(),
            &TOPIC_WITHDRAWAL[..],
            "Withdrawal topic",
        );
        assert_eq!(
            keccak256(
                "Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)".as_bytes()
            )
            .as_ref(),
            &TOPIC_SWAP_V4[..],
            "V4 Swap topic",
        );
        // The constants below are the topics these vaults/pools actually emit
        // on mainnet; these assertions independently confirm the signature.
        assert_eq!(
            keccak256("Swap(bytes32,address,address,uint256,uint256)".as_bytes()).as_ref(),
            &TOPIC_SWAP_BALANCER_V2[..],
            "Balancer V2 Swap topic",
        );
        assert_eq!(
            keccak256("Swap(address,address,address,uint256,uint256,uint256,uint256)".as_bytes())
                .as_ref(),
            &TOPIC_SWAP_BALANCER_V3[..],
            "Balancer V3 Swap topic",
        );
        assert_eq!(
            keccak256("TokenExchange(address,int128,uint256,int128,uint256)".as_bytes()).as_ref(),
            &TOPIC_EXCHANGE_CURVE[..],
            "Curve stable TokenExchange topic",
        );
        assert_eq!(
            keccak256("TokenExchange(address,uint256,uint256,uint256,uint256)".as_bytes()).as_ref(),
            &TOPIC_EXCHANGE_CURVE_CRYPTO[..],
            "Curve crypto TokenExchange topic",
        );
    }

    #[test]
    fn detects_v2_swap() {
        let user = addr("0x2222222222222222222222222222222222222222");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 100;
        let mut t = vec![trace(seq, call_ctx(user, pair))]; // ctx: pair
        seq += 1;

        // USDC user -> pair (input leg), emitted inside the USDC contract.
        t.push(trace(seq, call_ctx(pair, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000)); // user -1000
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret())); // pop usdc -> ctx: pair
        seq += 1;

        // WETH pair -> user (output leg), emitted inside the WETH contract.
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, user, 0, 900)); // user +900
        t.push(transfer_log(seq, &pair, &user, 900));
        seq += 1;
        t.push(trace(seq, ret())); // pop weth -> ctx: pair
        seq += 1;

        // The pair's Swap event (ctx top is the pair).
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret())); // pop pair
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 1, "expected one swap");
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(s.protocol, SwapProtocol::UniswapV2);
        assert_eq!(s.pools, vec![pair]);
        assert_eq!(s.legs, 1);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: weth,
                amount: Int::from(900u64)
            }]
        );
    }

    #[test]
    fn detects_multi_leg_mixed_swap() {
        // USDC --(V2 pair)--> WETH --(V3 pool)--> DAI, routed by a router.
        let user = addr("0x2222222222222222222222222222222222222222");
        let router = addr("0x6666666666666666666666666666666666666666");
        let pair = addr("0x5555555555555555555555555555555555555555"); // V2
        let pool = addr("0x7777777777777777777777777777777777777777"); // V3
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");
        let dai = addr("0x8888888888888888888888888888888888888888");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, router))]; // ctx: router
        seq += 1;

        // leg 1: USDC user -> pair
        t.push(trace(seq, call_ctx(router, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000)); // user -1000
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // intermediate: WETH pair -> pool (does not touch the swapper)
        t.push(trace(seq, call_ctx(router, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, pool, 0, 900)); // pool +900
        t.push(transfer_log(seq, &pair, &pool, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // pair Swap event (V2)
        t.push(trace(seq, call_ctx(router, pair)));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // leg 2: DAI pool -> user
        t.push(trace(seq, call_ctx(router, dai)));
        seq += 1;
        t.extend(balance_traces(&mut seq, dai, user, 0, 800)); // user +800
        t.push(transfer_log(seq, &pool, &user, 800));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // pool Swap event (V3)
        t.push(trace(seq, call_ctx(router, pool)));
        seq += 1;
        t.push(swap_v3_log(seq));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(s.protocol, SwapProtocol::Mixed);
        assert_eq!(s.pools, vec![pair, pool], "both pools, in order");
        assert_eq!(s.legs, 2);
        // End-to-end: only USDC in and DAI out; the WETH hop cancels.
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: dai,
                amount: Int::from(800u64)
            }]
        );
    }

    #[test]
    fn no_swap_without_pool_event() {
        // A plain ERC-20 transfer must not be reported as a swap.
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");
        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(from, token))];
        seq += 1;
        t.extend(balance_traces(&mut seq, token, from, 2000, 1000));
        t.push(transfer_log(seq, &from, &to, 1000));
        seq += 1;
        t.push(trace(seq, ret()));

        assert!(analyse(&t).swaps.is_empty());
    }

    #[test]
    fn ignores_spoofed_swap_log() {
        // A contract emits a Swap event with no token movement behind it. It must
        // NOT be reported as a swap, and must surface as unverified.
        let user = addr("0x2222222222222222222222222222222222222222");
        let evil = addr("0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddead");

        let traces = vec![
            trace(0, call_ctx(user, evil)), // ctx: evil
            swap_v2_log(1, &evil),          // emitter evil, but no real flow
            trace(2, ret()),
            fee(3, &user),
        ];

        let alerts = analyse(&traces);
        assert!(
            alerts.swaps.is_empty(),
            "spoofed log must not fabricate a swap"
        );
        assert_eq!(alerts.unverified_swaps, vec![evil]);
    }

    fn deposit_log(seq: usize, dst: &Acc, amount: u64) -> Trace {
        let mut payload = [0u8; 32];
        payload[24..].copy_from_slice(&amount.to_be_bytes());
        trace(
            seq,
            Event::Log(
                vec![Int::from(TOPIC_DEPOSIT.as_ref()), topic(dst)],
                Buf::from(payload.to_vec()),
            ),
        )
    }

    fn withdrawal_log(seq: usize, src: &Acc, amount: u64) -> Trace {
        let mut payload = [0u8; 32];
        payload[24..].copy_from_slice(&amount.to_be_bytes());
        trace(
            seq,
            Event::Log(
                vec![Int::from(TOPIC_WITHDRAWAL.as_ref()), topic(src)],
                Buf::from(payload.to_vec()),
            ),
        )
    }

    fn swap_v4_log(seq: usize) -> Trace {
        trace(
            seq,
            Event::Log(vec![Int::from(TOPIC_SWAP_V4.as_ref())], Buf::default()),
        )
    }

    #[test]
    fn weth_unwrap_is_pool_not_swapper() {
        // Token -> ETH swap with an unwrap at the end. The WETH contract nets
        // "WETH in, ETH out" -- the same shape as a swapper -- but it must be
        // classified as a conversion hop (pool), not reported as a swapper.
        let user = addr("0x2222222222222222222222222222222222222222");
        let router = addr("0x6666666666666666666666666666666666666666");
        let wethc = addr("0x4444444444444444444444444444444444444444");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let tok = addr("0x8888888888888888888888888888888888888888");
        let eth_out = Int::from(900u64);

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, router))]; // ctx: router
        seq += 1;

        // user's token goes into the pair
        t.push(trace(seq, call_ctx(router, tok)));
        seq += 1;
        t.extend(balance_traces(&mut seq, tok, user, 5000, 4000)); // user -1000
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // the pair pays WETH to the router
        t.push(trace(seq, call_ctx(router, pair)));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, wethc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, wethc, router, 0, 900)); // router +900
        t.push(transfer_log(seq, &pair, &router, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // router unwraps: WETH.withdraw(900) burns and sends ETH back
        t.push(trace(seq, call_ctx(router, wethc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, wethc, router, 900, 0)); // burn
        t.push(withdrawal_log(seq, &router, 900));
        seq += 1;
        t.push(trace(seq, call_ctx_eth(wethc, router, eth_out)));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, ret())); // pop withdraw frame
        seq += 1;

        // router pays the ETH out to the user
        t.push(trace(seq, call_ctx_eth(router, user, eth_out)));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        t.push(trace(seq, ret())); // pop router
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 1, "the WETH contract is not a swapper");
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: tok,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: crate::ETH,
                amount: eth_out
            }],
            "the bought side is native ETH"
        );
        assert!(s.pools.contains(&pair));
        assert!(s.pools.contains(&wethc), "the unwrap is a conversion hop");
        assert_eq!(s.protocol, SwapProtocol::UniswapV2);
    }

    #[test]
    fn mint_burn_zero_address_is_not_a_party() {
        // A burn-one-mint-another converter (e.g. DAI <-> USDS) routes flows
        // through the zero address. The zero address must never surface as a
        // swapper, and with no pool involved there is no swap to report.
        let user = addr("0x2222222222222222222222222222222222222222");
        let conv = addr("0x7777777777777777777777777777777777777777");
        let dai = addr("0x1111111111111111111111111111111111111111");
        let usds = addr("0x4444444444444444444444444444444444444444");
        let zero = Acc::default();

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, conv))];
        seq += 1;
        t.push(trace(seq, call_ctx(conv, dai)));
        seq += 1;
        t.extend(balance_traces(&mut seq, dai, user, 1000, 0)); // burn
        t.push(transfer_log(seq, &user, &zero, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(conv, usds)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usds, user, 0, 1000)); // mint
        t.push(transfer_log(seq, &zero, &user, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.erc20_transfers.len(), 2);
        assert!(
            alerts.swaps.iter().all(|s| s.swapper != zero),
            "the zero address is never a swapper"
        );
        assert!(alerts.swaps.is_empty(), "no pool, no swap");
    }

    #[test]
    fn erc4337_bundle_reports_relayed_taker_swap() {
        // An ERC-4337 bundle (modeled on mainnet 0x81e12c44..): the fee payer
        // is a bundler, the smart wallet nets zero on every token (a conduit),
        // the real payer is the EOA that funded the wallet and the output goes
        // to a separate payout address. The route: MEME -> V2 pair -> WETH ->
        // passive maker fill -> USDC. The entrypoint refunds the bundler in
        // ETH, which must NOT be picked up as a swap leg. Expect the maker's
        // fill AND the relayed taker pair (funder -> payout).
        let bundler = addr("0x4337000000000000000000000000000000000001");
        let ep = addr("0x4337000000000000000000000000000000000008");
        let wallet = addr("0xb92fb92fb92fb92fb92fb92fb92fb92fb92fb92f");
        let funder = addr("0x33f433f433f433f433f433f433f433f433f433f4");
        let payout = addr("0x4cd04cd04cd04cd04cd04cd04cd04cd04cd04cd0");
        let maker = addr("0x6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let meme = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");
        let usdc = addr("0x9999999999999999999999999999999999999999");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(bundler, ep))];
        seq += 1;
        t.push(trace(seq, call_ctx(ep, wallet)));
        seq += 1;

        // funder's MEME into the wallet, wallet feeds the pair
        t.push(trace(seq, call_ctx(wallet, meme)));
        seq += 1;
        t.extend(balance_traces(&mut seq, meme, funder, 5000, 4000)); // funder -1000
        t.push(transfer_log(seq, &funder, &wallet, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(wallet, meme)));
        seq += 1;
        t.extend(balance_traces(&mut seq, meme, wallet, 1000, 0)); // wallet -1000
        t.push(transfer_log(seq, &wallet, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // pair pays WETH to the wallet
        t.push(trace(seq, call_ctx(wallet, pair)));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, wallet, 0, 900)); // wallet +900
        t.push(transfer_log(seq, &pair, &wallet, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // maker fill: maker's USDC pulled in, WETH paid to the maker
        t.push(trace(seq, call_ctx(wallet, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, maker, 2000, 1150)); // maker -850
        t.push(transfer_log(seq, &maker, &wallet, 850));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(wallet, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, wallet, 900, 0)); // wallet -900
        t.push(transfer_log(seq, &wallet, &maker, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // payout to a separate address
        t.push(trace(seq, call_ctx(wallet, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, payout, 0, 850));
        t.push(transfer_log(seq, &wallet, &payout, 850));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        t.push(trace(seq, ret())); // pop wallet
        seq += 1;
        // entrypoint refunds the bundler in ETH (gas compensation)
        t.push(trace(seq, call_ctx_eth(ep, bundler, Int::from(5u64))));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, ret())); // pop ep
        seq += 1;
        t.push(fee(seq, &bundler));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 2, "maker fill + relayed taker pair");

        let m = alerts.swaps.iter().find(|s| s.swapper == maker).unwrap();
        assert_eq!(
            m.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(850u64)
            }]
        );

        let u = alerts.swaps.iter().find(|s| s.swapper == funder).unwrap();
        assert_eq!(u.recipient, payout);
        assert_eq!(
            u.sold,
            vec![TokenAmount {
                token: meme,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            u.bought,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(850u64)
            }]
        );
        assert_eq!(u.pools, vec![pair]);
        assert_eq!(u.protocol, SwapProtocol::UniswapV2);

        for s in &alerts.swaps {
            assert_ne!(s.swapper, bundler, "gas refund is not a swap leg");
            assert_ne!(s.recipient, bundler, "gas refund is not a swap leg");
            assert_ne!(s.swapper, ep);
        }
    }

    #[test]
    fn rfq_settlement_reports_user_and_maker() {
        // A settlement contract routes the sender's token through a V2 pair,
        // fills the middle leg against a passive RFQ maker, and pays the output
        // (minus a fee skim) to a separate payout wallet. Expect BOTH the
        // sender's end-to-end swap and the maker's fill -- and the maker must
        // not inherit the route's pools.
        let user = addr("0x2222222222222222222222222222222222222222");
        let entry = addr("0x8fea8fea8fea8fea8fea8fea8fea8fea8fea8fea");
        let maker = addr("0x6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a6f7a");
        let payout = addr("0xb8fcb8fcb8fcb8fcb8fcb8fcb8fcb8fcb8fcb8fc");
        let feesink = addr("0x7fc87fc87fc87fc87fc87fc87fc87fc87fc87fc8");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let t1 = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");
        let usdc = addr("0x9999999999999999999999999999999999999999");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, entry))]; // ctx: entry
        seq += 1;

        // leg 1: user's T1 into the pair, pair pays WETH to the entry
        t.push(trace(seq, call_ctx(entry, t1)));
        seq += 1;
        t.extend(balance_traces(&mut seq, t1, user, 5000, 4000)); // user -1000
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(entry, pair)));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, entry, 0, 900)); // entry +900
        t.push(transfer_log(seq, &pair, &entry, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // leg 2 (RFQ fill): maker's USDC pulled in, WETH paid to the maker
        t.push(trace(seq, call_ctx(entry, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, maker, 2000, 1000)); // maker -1000
        t.push(transfer_log(seq, &maker, &entry, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(entry, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, entry, 900, 0)); // entry -900
        t.push(transfer_log(seq, &entry, &maker, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // payout to a separate wallet, minus a small fee skim
        t.push(trace(seq, call_ctx(entry, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, payout, 0, 990));
        t.push(transfer_log(seq, &entry, &payout, 990));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(entry, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, feesink, 0, 10));
        t.push(transfer_log(seq, &entry, &feesink, 10));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        t.push(trace(seq, ret())); // pop entry
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 2, "maker fill + sender swap");

        let m = alerts.swaps.iter().find(|s| s.swapper == maker).unwrap();
        assert_eq!(
            m.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            m.bought,
            vec![TokenAmount {
                token: weth,
                amount: Int::from(900u64)
            }]
        );
        assert!(m.pools.is_empty(), "the maker did not touch the pools");
        assert_eq!(m.legs, 0);
        assert_eq!(m.protocol, SwapProtocol::Unknown);

        let u = alerts.swaps.iter().find(|s| s.swapper == user).unwrap();
        assert_eq!(u.recipient, payout, "fee skim must not steal the payout");
        assert_eq!(
            u.sold,
            vec![TokenAmount {
                token: t1,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            u.bought,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(990u64)
            }]
        );
        assert_eq!(u.pools, vec![pair]);
        assert_eq!(u.protocol, SwapProtocol::UniswapV2);
    }

    #[test]
    fn detects_v4_swap() {
        // Same flow shape as the V2 test, but the pool emits the Uniswap V4
        // PoolManager Swap event.
        let user = addr("0x2222222222222222222222222222222222222222");
        let pm = addr("0x5555555555555555555555555555555555555555");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, pm))];
        seq += 1;
        t.push(trace(seq, call_ctx(pm, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000));
        t.push(transfer_log(seq, &user, &pm, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(pm, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, user, 0, 900));
        t.push(transfer_log(seq, &pm, &user, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v4_log(seq));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert!(alerts.unverified_swaps.is_empty());
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(s.pools, vec![pm]);
        assert_eq!(s.protocol, SwapProtocol::UniswapV4);
    }

    fn log_topic(seq: usize, topic: &[u8; 32]) -> Trace {
        trace(
            seq,
            Event::Log(vec![Int::from(topic.as_ref())], Buf::default()),
        )
    }

    // Token-for-token flow through `pool`, which emits `topic`.
    fn one_hop_with_topic(pool: Acc, topic: &[u8; 32]) -> Vec<Trace> {
        let user = addr("0x2222222222222222222222222222222222222222");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, pool))];
        seq += 1;
        t.push(trace(seq, call_ctx(pool, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000));
        t.push(transfer_log(seq, &user, &pool, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(pool, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, user, 0, 900));
        t.push(transfer_log(seq, &pool, &user, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(log_topic(seq, topic));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(fee(seq, &user));
        t
    }

    #[test]
    fn detects_balancer_and_curve_swaps() {
        let user = addr("0x2222222222222222222222222222222222222222");
        let pool = addr("0x5555555555555555555555555555555555555555");
        for (topic, want) in [
            (&TOPIC_SWAP_BALANCER_V2, SwapProtocol::Balancer),
            (&TOPIC_SWAP_BALANCER_V3, SwapProtocol::Balancer),
            (&TOPIC_EXCHANGE_CURVE, SwapProtocol::Curve),
            (&TOPIC_EXCHANGE_CURVE_CRYPTO, SwapProtocol::Curve),
        ] {
            let alerts = analyse(&one_hop_with_topic(pool, topic));
            assert!(alerts.unverified_swaps.is_empty(), "{want:?}");
            assert_eq!(alerts.swaps.len(), 1, "{want:?}");
            let s = &alerts.swaps[0];
            assert_eq!(s.swapper, user, "{want:?}");
            assert_eq!(s.pools, vec![pool], "{want:?}");
            assert_eq!(s.protocol, want, "{want:?}");
        }
    }

    #[test]
    fn unbacked_balancer_log_is_unverified() {
        // A Balancer Swap topic with no confirmed token flow behind it is
        // still spoofing -- new topics must not become a trust shortcut.
        let evil = addr("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        let t = vec![
            trace(
                0,
                call_ctx(addr("0x2222222222222222222222222222222222222222"), evil),
            ),
            log_topic(1, &TOPIC_SWAP_BALANCER_V2),
            trace(2, ret()),
            fee(3, &addr("0x2222222222222222222222222222222222222222")),
        ];
        let alerts = analyse(&t);
        assert!(alerts.swaps.is_empty());
        assert_eq!(alerts.unverified_swaps, vec![evil]);
    }

    #[test]
    fn bot_contract_is_swapper_not_pool() {
        // An EOA triggers a bot contract that trades its OWN funds: the bot,
        // not the tx sender, is the swapper -- and it must not be mistaken for
        // a pool just because it converted one token into another.
        let user = addr("0x2222222222222222222222222222222222222222");
        let bot = addr("0xb07b07b07b07b07b07b07b07b07b07b07b07b07b");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, bot))]; // ctx: bot
        seq += 1;

        // USDC bot -> pair (the bot pays from its own balance)
        t.push(trace(seq, call_ctx(bot, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, bot, 5000, 4000)); // bot -1000
        t.push(transfer_log(seq, &bot, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // bot calls the pair, which pays WETH back to the bot
        t.push(trace(seq, call_ctx(bot, pair)));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, bot, 0, 900)); // bot +900
        t.push(transfer_log(seq, &pair, &bot, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret())); // pop pair
        seq += 1;

        t.push(trace(seq, ret())); // pop bot
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert!(alerts.unverified_swaps.is_empty());
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, bot, "the bot holds the funds, not the sender");
        assert_eq!(s.recipient, bot);
        assert_eq!(s.pools, vec![pair], "the bot is not a pool");
        assert_eq!(s.protocol, SwapProtocol::UniswapV2);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: weth,
                amount: Int::from(900u64)
            }]
        );
    }

    #[test]
    fn swap_output_to_other_recipient() {
        // The swapper pays in, but the pool's output goes to a different
        // recipient (router `to` param). Both sides must be captured.
        let user = addr("0x2222222222222222222222222222222222222222");
        let other = addr("0x3333333333333333333333333333333333333333");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, pair))];
        seq += 1;
        t.push(trace(seq, call_ctx(pair, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000)); // user -1000
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, other, 0, 900)); // other +900
        t.push(transfer_log(seq, &pair, &other, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(s.recipient, other, "output went to a different recipient");
        assert_eq!(s.pools, vec![pair]);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: weth,
                amount: Int::from(900u64)
            }]
        );
    }

    #[test]
    fn eth_to_token_swap_through_weth_wrap() {
        // ETH -> token via a router that wraps to WETH first. The sold side is
        // native ETH (call value + WETH Deposit), invisible to Transfer logs.
        let user = addr("0x2222222222222222222222222222222222222222");
        let router = addr("0x6666666666666666666666666666666666666666");
        let wethc = addr("0x4444444444444444444444444444444444444444");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let tok = addr("0x8888888888888888888888888888888888888888");
        let eth_in = Int::from(1000u64);

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx_eth(user, router, eth_in))]; // ctx: router
        seq += 1;

        // router wraps: WETH.deposit{value: 1000}()
        t.push(trace(seq, call_ctx_eth(router, wethc, eth_in)));
        seq += 1;
        t.extend(balance_traces(&mut seq, wethc, router, 0, 1000)); // mint to router
        t.push(deposit_log(seq, &router, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // router sends WETH into the pair
        t.push(trace(seq, call_ctx(router, wethc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, wethc, router, 1000, 0)); // router -1000
        t.push(transfer_log(seq, &router, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        // pair pays the token out to the user
        t.push(trace(seq, call_ctx(router, pair)));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, tok)));
        seq += 1;
        t.extend(balance_traces(&mut seq, tok, user, 0, 800)); // user +800
        t.push(transfer_log(seq, &pair, &user, 800));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(swap_v2_log(seq, &pair));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;

        t.push(trace(seq, ret())); // pop router
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.swapper, user);
        assert_eq!(s.recipient, user);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: crate::ETH,
                amount: eth_in
            }],
            "the sold side is native ETH"
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: tok,
                amount: Int::from(800u64)
            }]
        );
        assert_eq!(s.protocol, SwapProtocol::UniswapV2);
        assert!(s.pools.contains(&pair));
        assert!(s.pools.contains(&wethc), "the wrap is a conversion hop");
    }

    #[test]
    fn undone_range_discards_transfers() {
        // A transfer whose surrounding call was reverted (Undo range) must not
        // be counted, even though the traces stream out with reverted=false.
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(from, token))]; // seq 0
        seq += 1;
        let undo_from = seq;
        t.extend(balance_traces(&mut seq, token, from, 2000, 1000)); // seq 1, 2
        t.push(transfer_log(seq, &from, &to, 1000)); // seq 3
        seq += 1;
        t.push(trace(seq, Event::Undo(undo_from, seq))); // undoes [1, 4)
        seq += 1;
        t.push(trace(seq, ret()));

        let alerts = analyse(&t);
        assert_eq!(alerts.erc20_transfers.len(), 0, "transfer was undone");
        assert_eq!(alerts.forged_transfers.len(), 0);
        assert!(alerts.swaps.is_empty());
    }

    #[test]
    fn vyper_layout_balance_write_confirms_transfer() {
        // Vyper HashMap[address, uint256] hashes (slot, holder) -- the reverse
        // of Solidity. The balance write must still confirm the Transfer log.
        use yevm_misc::keccak256;
        let token = addr("0x1111111111111111111111111111111111111111");
        let from = addr("0x2222222222222222222222222222222222222222");
        let to = addr("0x3333333333333333333333333333333333333333");

        let mut preimage = [0u8; 64];
        preimage[31] = 3; // slot 3 in the first word
        preimage[44..64].copy_from_slice(from.as_ref()); // holder in the second
        let slot_hash = Int::from(keccak256(&preimage).as_ref());

        let traces = vec![
            trace(0, call_ctx(from, token)),
            trace(1, Event::Hash(Buf::from(preimage.to_vec()), slot_hash)),
            trace(
                2,
                Event::Put(
                    Target::Store {
                        acc: token,
                        key: slot_hash,
                        val: Int::from(2000u64),
                    },
                    Int::from(1000u64),
                ),
            ),
            transfer_log(3, &from, &to, 1000),
            trace(4, ret()),
        ];

        let alerts = analyse(&traces);
        assert_eq!(alerts.erc20_transfers.len(), 1);
        assert_eq!(alerts.forged_transfers.len(), 0);
    }

    #[test]
    fn detects_swap_without_swap_log() {
        // Real token-for-token flow through a pool, but no recognized Swap log:
        // still detected, labeled Unknown, and not flagged as unverified.
        let user = addr("0x2222222222222222222222222222222222222222");
        let pair = addr("0x5555555555555555555555555555555555555555");
        let usdc = addr("0x1111111111111111111111111111111111111111");
        let weth = addr("0x4444444444444444444444444444444444444444");

        let mut seq = 0;
        let mut t = vec![trace(seq, call_ctx(user, pair))];
        seq += 1;
        t.push(trace(seq, call_ctx(pair, usdc)));
        seq += 1;
        t.extend(balance_traces(&mut seq, usdc, user, 5000, 4000));
        t.push(transfer_log(seq, &user, &pair, 1000));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, call_ctx(pair, weth)));
        seq += 1;
        t.extend(balance_traces(&mut seq, weth, user, 0, 900));
        t.push(transfer_log(seq, &pair, &user, 900));
        seq += 1;
        t.push(trace(seq, ret()));
        seq += 1;
        t.push(trace(seq, ret())); // pop pair
        seq += 1;
        t.push(fee(seq, &user));

        let alerts = analyse(&t);
        assert!(alerts.unverified_swaps.is_empty());
        assert_eq!(alerts.swaps.len(), 1);
        let s = &alerts.swaps[0];
        assert_eq!(s.protocol, SwapProtocol::Unknown);
        assert_eq!(s.pools, vec![pair]);
        assert_eq!(
            s.sold,
            vec![TokenAmount {
                token: usdc,
                amount: Int::from(1000u64)
            }]
        );
        assert_eq!(
            s.bought,
            vec![TokenAmount {
                token: weth,
                amount: Int::from(900u64)
            }]
        );
    }
}
