mod analyse;
pub mod quotes;

pub use analyse::analyse;

use serde::{Deserialize, Serialize};
use yevm_base::{Acc, Int, acc};
use yevm_core::trace::filter;

/// Sentinel "token" address representing native ETH in swap flows (EIP-7528).
/// Used in [`TokenAmount::token`] when a swap leg is native ETH rather than an
/// ERC-20 (e.g. an ETH -> token swap routed through a WETH wrap).
pub const ETH: Acc = acc("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

pub const FILTER: u32 = filter::HASH
    | filter::CALL
    | filter::GET
    | filter::PUT
    | filter::RETURN
    | filter::REVERT
    | filter::HALT
    | filter::FEE
    | filter::LOG;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthChange {
    pub acc: Acc,
    pub before: Int,
    pub after: Int,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Erc20Transfer {
    pub token: Acc,
    pub from: Acc,
    pub to: Acc,
    pub amount: Option<Int>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Erc20Approval {
    pub token: Acc,
    pub owner: Acc,
    pub spender: Acc,
    pub allowance: Option<Int>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Erc721Transfer {
    pub token: Acc,
    pub from: Acc,
    pub to: Acc,
    pub token_id: Option<Int>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyUpgrade {
    pub proxy: Acc,
    pub slot: Int,
    pub old_impl: Acc,
    pub new_impl: Acc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgedTransfer {
    pub token: Acc,
    pub from: Acc,
    pub to: Acc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeInfo {
    pub sender: Acc,
    pub coinbase: Acc,
    pub gas_used: u64,
}

/// A token together with an amount (magnitude). Used for the input/output legs
/// of a [`Swap`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenAmount {
    pub token: Acc,
    pub amount: Int,
}

/// Which AMM the swap's pools were, as corroborated by a cross-checked `Swap`
/// log. `Unknown` means the swap was reconstructed from token flows alone (no
/// recognized, verified pool event) -- it is still a real swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwapProtocol {
    UniswapV2,
    UniswapV3,
    UniswapV4,
    /// Balancer vault (V2 or V3); the vault itself holds the pool tokens, so
    /// it is the account the flows identify as the pool.
    Balancer,
    /// Curve stable or crypto pool (`TokenExchange`).
    Curve,
    /// Pools of more than one protocol appeared (a multi-leg route across versions).
    Mixed,
    /// Detected purely from confirmed token flows; no verified pool event.
    Unknown,
}

/// A token swap, reconstructed end-to-end from the trace.
///
/// The result is stated from the swapper's point of view: `sold` is what left
/// the swapper's account (net), `bought` (a.k.a. tokens taken) is what arrived
/// (net, at `recipient`). Native ETH legs appear under the [`ETH`] sentinel
/// token. Intermediate hops of a multi-leg route cancel out and do not appear
/// here -- they show up as the `pools` that were touched, in execution order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Swap {
    /// The account that paid the input side. Derived from the token flows and
    /// the call graph -- a bot/settlement contract holding the funds counts,
    /// not merely the tx sender.
    pub swapper: Acc,
    /// The account that received the output side. Usually == `swapper`, but
    /// routers can pay out to a different recipient.
    pub recipient: Acc,
    /// Tokens the swapper net sent (the input side).
    pub sold: Vec<TokenAmount>,
    /// Tokens the recipient net received (the output side; "tokens taken").
    pub bought: Vec<TokenAmount>,
    /// Pools touched, de-duplicated in first-seen order. Derived from token
    /// flows (an address that took in one token and paid out another), NOT from
    /// `Swap` log events. A single-hop swap has one; more means a multi-leg route.
    pub pools: Vec<Acc>,
    /// Number of pool hops (== `pools.len()`).
    pub legs: usize,
    pub protocol: SwapProtocol,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alerts {
    pub proxy_upgrades: Vec<ProxyUpgrade>,
    pub eth_changes: Vec<EthChange>,
    pub erc20_transfers: Vec<Erc20Transfer>,
    pub erc20_approvals: Vec<Erc20Approval>,
    pub erc721_transfers: Vec<Erc721Transfer>,
    pub forged_transfers: Vec<ForgedTransfer>,
    pub swaps: Vec<Swap>,
    /// Emitters of a `Swap` topic that no confirmed token flow backs -- a spoofed
    /// or otherwise unverifiable pool event. The swap-log analog of
    /// [`ForgedTransfer`].
    pub unverified_swaps: Vec<Acc>,
    pub fee: Option<FeeInfo>,
}

/*

Use Case: Anomaly Detection and Transaction Verification

Re-execute all previous transactions for the given address, collect results (state, value).
Re-execute target transaction, collect results (state, value) and compare to previous results.
If significant deviation is detected, flag and report it.

[This could have prevented "ByBit hack" of Feb'25 $1.5B worth of ETH being stolen].
https://www.chainalysis.com/blog/bybit-exchange-hack-february-2025-crypto-security-dprk/
https://www.cremit.io/blog/bybit-hacking-incident-analysis-how-to-strengthen-cryptocurrency-exchange-security

*/
