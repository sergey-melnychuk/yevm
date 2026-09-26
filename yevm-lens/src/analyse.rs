use yevm_base::{Acc, Int};
use yevm_core::trace::{Event, Target, Trace};
use yevm_misc::buf::Buf;

use crate::{
    Alerts, Erc20Approval, Erc20Transfer, Erc721Transfer, EthChange, FeeInfo, ForgedTransfer,
    ProxySwap, Swap, SwapProtocol, TokenAmount,
};

// keccak256("Transfer(address,address,uint256)")
const TOPIC_TRANSFER: [u8; 32] =
    hex_lit!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
// keccak256("Approval(address,address,uint256)")
const TOPIC_APPROVAL: [u8; 32] =
    hex_lit!("8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");
// keccak256("Swap(address,uint256,uint256,uint256,uint256,address)") -- Uniswap V2 pair
const TOPIC_SWAP_V2: [u8; 32] =
    hex_lit!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
// keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)") -- Uniswap V3 pool
const TOPIC_SWAP_V3: [u8; 32] =
    hex_lit!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");

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
    holder: Acc,
    contract: Acc,
    delta: i128, // approximate; capped for large values
    log_matched: bool,
}

pub fn analyse(traces: &[Trace]) -> Alerts {
    // Pass 1: collect hash preimages (ERC-20 balance slot identification) and
    // the set of addresses that were actually called or had their code fetched.
    // The latter is used to confirm proxy implementation swaps: if the old impl
    // was never called/loaded, the storage write is likely a plain state update.
    //
    // NOTE: traces arrive from the stream with reverted=false (emitted before the revert
    // is applied). The receiver must mark ranges as reverted when Undo events arrive,
    // so by the time analyse() is called, t.reverted correctly reflects revert state.
    let mut preimages: std::collections::HashMap<Int, (Acc, Int)> = Default::default();
    let mut interacted: std::collections::HashSet<Acc> = Default::default();
    for t in traces {
        if t.reverted {
            continue;
        }
        match &t.event {
            Event::Hash(input, output) => {
                let b = input.as_slice();
                if b.len() == 64 && b[..12] == [0u8; 12] {
                    let holder = Acc::from(&b[12..32]);
                    let slot = Int::from(&b[32..64]);
                    preimages.insert(*output, (holder, slot));
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
    let mut ctx_stack: Vec<Acc> = Vec::new(); // call context addresses

    // Swap reconstruction. We do NOT trust Swap log events to mean a swap
    // happened -- any contract can emit one. We only record (emitter, is_v3) for
    // each, and later cross-check them against confirmed token flows; the swap
    // itself is derived from flows. `first_caller` is the swapper fallback when
    // no Fee event pins the sender.
    let mut swap_logs: Vec<(Acc, bool)> = Vec::new();
    let mut first_caller: Option<Acc> = None;

    for t in traces {
        if t.reverted {
            continue;
        }
        match &t.event {
            Event::Call(call, mode) => {
                use yevm_core::evm::CallMode;
                if first_caller.is_none() {
                    first_caller = Some(call.by);
                }
                let exec_addr = match mode {
                    CallMode::Delegate(..) | CallMode::CallCode(..) => {
                        ctx_stack.last().copied().unwrap_or(call.by)
                    }
                    CallMode::Create(addr) | CallMode::Create2(addr) => *addr,
                    _ => call.to.unwrap_or(call.by),
                };
                ctx_stack.push(exec_addr);
            }

            Event::Return(..) | Event::Revert(..) | Event::Halt(..) => {
                ctx_stack.pop();
            }

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
                                alerts.proxy_swaps.push(ProxySwap {
                                    proxy: *acc,
                                    slot: *key,
                                    old_impl,
                                    new_impl,
                                });
                            }
                        }

                        // ERC-20 balance write: storage key matches a known hash preimage
                        if let Some((holder, _)) = preimages.get(key) {
                            let old_u = val_to_u128(val);
                            let new_u = val_to_u128(next);
                            let delta = new_u as i128 - old_u as i128;
                            balance_writes.push(BalanceWrite {
                                holder: *holder,
                                contract: *acc,
                                delta,
                                log_matched: false,
                            });
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
                let emitter = ctx_stack.last().copied().unwrap_or_default();

                if topics.is_empty() {
                    continue;
                }
                let t0 = topics[0].as_ref();

                // Swap topic: record the emitter as a *candidate* pool. Whether
                // it was a real swap is decided later from token flows, not here.
                if t0 == TOPIC_SWAP_V2 || t0 == TOPIC_SWAP_V3 {
                    swap_logs.push((emitter, t0 == TOPIC_SWAP_V3));
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
                                && w.holder == from
                                && w.delta < 0
                        });
                        let has_from = from_w.is_some();
                        if let Some(w) = from_w {
                            w.log_matched = true;
                        }

                        let to_w = balance_writes.iter_mut().find(|w| {
                            !w.log_matched && w.contract == emitter && w.holder == to && w.delta > 0
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

    // Reconstruct swaps from CONFIRMED, balance-verified token flows -- never
    // from Swap log events alone, which any contract can emit. Swap logs only
    // *label* the protocol, and are cross-checked: a Swap log whose emitter shows
    // no real token-for-token flow is reported as `unverified_swaps`.
    {
        let add = yevm_base::math::lift(|[a, b]| a + b);
        let sub = yevm_base::math::lift(|[a, b]| a - b);
        // Keyed (holder, token). Built only from confirmed erc20_transfers, so
        // forged Transfer logs (no backing balance write) never enter the flow.
        let mut inflow: std::collections::HashMap<(Acc, Acc), Int> = Default::default();
        let mut outflow: std::collections::HashMap<(Acc, Acc), Int> = Default::default();
        let mut appearance: Vec<Acc> = Vec::new();
        for tr in &alerts.erc20_transfers {
            if let Some(amt) = tr.amount {
                for h in [tr.from, tr.to] {
                    if !appearance.contains(&h) {
                        appearance.push(h);
                    }
                }
                let o = outflow.entry((tr.from, tr.token)).or_insert(Int::ZERO);
                *o = add([*o, amt]);
                let i = inflow.entry((tr.to, tr.token)).or_insert(Int::ZERO);
                *i = add([*i, amt]);
            }
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

        let swapper = alerts
            .fee
            .as_ref()
            .map(|f| f.sender)
            .or(first_caller)
            .unwrap_or_default();

        // A pool is any *other* address that took in one token and paid out a
        // different one, both confirmed by balance writes. Routers net to zero
        // per token and drop out on their own; the swapper is excluded explicitly.
        let mut pools: Vec<Acc> = Vec::new();
        for addr in &appearance {
            if *addr == swapper {
                continue;
            }
            let (sold, bought) = net(*addr);
            if !sold.is_empty() && !bought.is_empty() {
                pools.push(*addr);
            }
        }

        // Cross-check Swap logs against the flow-derived pools: matches set the
        // protocol label; any that don't correspond to a real swap are flagged.
        let (mut saw_v2, mut saw_v3) = (false, false);
        for (emitter, is_v3) in &swap_logs {
            if pools.contains(emitter) {
                if *is_v3 {
                    saw_v3 = true;
                } else {
                    saw_v2 = true;
                }
            } else if !alerts.unverified_swaps.contains(emitter) {
                alerts.unverified_swaps.push(*emitter);
            }
        }

        if !pools.is_empty() {
            let (sold, bought) = net(swapper);
            let protocol = match (saw_v2, saw_v3) {
                (true, true) => SwapProtocol::Mixed,
                (true, false) => SwapProtocol::UniswapV2,
                (false, true) => SwapProtocol::UniswapV3,
                (false, false) => SwapProtocol::Unknown,
            };
            let legs = pools.len();
            alerts.swaps.push(Swap {
                swapper,
                sold,
                bought,
                pools,
                legs,
                protocol,
            });
        }
    }

    alerts
}

fn val_to_u128(v: &Int) -> u128 {
    v.as_u128()
}

fn buf_to_int(b: &Buf) -> Option<Int> {
    let s = b.as_slice();
    if s.is_empty() || s.len() > 32 {
        return None;
    }
    Some(Int::from(s))
}

// Compile-time hex literal → [u8; N]
macro_rules! hex_lit {
    ($s:literal) => {{
        const fn parse(s: &[u8]) -> [u8; 32] {
            let mut out = [0u8; 32];
            let mut i = 0;
            while i < 32 {
                let hi = hex_nibble(s[i * 2]);
                let lo = hex_nibble(s[i * 2 + 1]);
                out[i] = (hi << 4) | lo;
                i += 1;
            }
            out
        }
        const fn hex_nibble(b: u8) -> u8 {
            match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => panic!("invalid hex"),
            }
        }
        parse($s.as_bytes())
    }};
}
use hex_lit;

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

    fn ret() -> Event {
        Event::Return(Buf::default(), 21_000)
    }

    fn addr_as_storage(a: &Acc) -> Int {
        let mut v = [0u8; 32];
        v[12..].copy_from_slice(a.as_ref());
        Int::from(v.as_ref())
    }

    #[test]
    fn detects_proxy_swap() {
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
        assert_eq!(alerts.proxy_swaps.len(), 1);
        assert_eq!(alerts.proxy_swaps[0].old_impl, old_impl);
        assert_eq!(alerts.proxy_swaps[0].new_impl, new_impl);
        assert_eq!(alerts.proxy_swaps[0].proxy, proxy);
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

        assert_eq!(analyse(&traces).proxy_swaps.len(), 0);
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
        assert_eq!(analyse(&traces).proxy_swaps.len(), 0);
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
        assert_eq!(analyse(&traces).proxy_swaps.len(), 0);
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
