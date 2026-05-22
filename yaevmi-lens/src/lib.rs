mod analyse;

pub use analyse::analyse;

use serde::{Deserialize, Serialize};
use yaevmi_base::{Acc, Int};

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
pub struct ProxySwap {
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alerts {
    pub proxy_swaps: Vec<ProxySwap>,
    pub eth_changes: Vec<EthChange>,
    pub erc20_transfers: Vec<Erc20Transfer>,
    pub erc20_approvals: Vec<Erc20Approval>,
    pub erc721_transfers: Vec<Erc721Transfer>,
    pub forged_transfers: Vec<ForgedTransfer>,
    pub fee: Option<FeeInfo>,
}
