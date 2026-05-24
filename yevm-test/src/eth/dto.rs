use serde::Deserialize;
use std::collections::HashMap;
use yevm_base::{Acc, Int};
use yevm_misc::buf::Buf;

// Top-level file: map from test name to test case
pub type TestFile = HashMap<String, TestCase>;

#[derive(Deserialize)]
pub struct TestCase {
    pub env: TestEnv,
    pub pre: HashMap<Acc, AccountState>,
    pub transaction: Transaction,
    pub post: HashMap<String, Vec<PostEntry>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestEnv {
    pub current_coinbase: Acc,
    pub current_gas_limit: Int,
    pub current_number: Int,
    pub current_timestamp: Int,
    pub current_base_fee: Option<Int>,
    pub current_random: Option<Int>,
    pub current_difficulty: Option<Int>,
}

#[derive(Deserialize)]
pub struct AccountState {
    pub balance: Int,
    pub code: Buf,
    pub nonce: Int,
    pub storage: HashMap<Int, Int>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub data: Vec<Buf>,
    pub gas_limit: Vec<Int>,
    pub gas_price: Option<Int>,
    pub max_fee_per_gas: Option<Int>,
    pub max_priority_fee_per_gas: Option<Int>,
    pub nonce: Int,
    pub sender: Acc,
    pub to: Option<Acc>,
    pub value: Vec<Int>,
    #[serde(default)]
    pub access_lists: Option<Vec<Option<Vec<AccessListEntry>>>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessListEntry {
    pub address: Acc,
    #[serde(default)]
    pub storage_keys: Vec<Int>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostEntry {
    pub indexes: Indexes,
    pub hash: Int,
    pub logs: Int,
    #[serde(default)]
    pub state: HashMap<Acc, AccountState>,
    /// When set, the test expects the transaction to fail (validation or execution).
    #[serde(default, rename = "expectException")]
    pub expect_exception: Option<String>,
}

#[derive(Deserialize)]
pub struct Indexes {
    pub data: usize,
    pub gas: usize,
    pub value: usize,
}
