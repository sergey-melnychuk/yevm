use serde::{Deserialize, Serialize};
use yevm_base::{Acc, Int};
use yevm_misc::{buf::Buf, hex::Hex, keccak256};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Call {
    pub by: Acc,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Acc>,
    pub gas: u64,
    pub eth: Int,
    pub data: Buf,
}

impl Call {
    pub fn is_create(&self) -> bool {
        self.to.is_none()
    }

    pub fn builder() -> Builder {
        Builder::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Builder {
    by: Option<Acc>,
    to: Option<Acc>,
    gas: Option<u64>,
    eth: Option<Int>,
    data: Buf,
}

impl Builder {
    pub fn build(self) -> Call {
        Call {
            by: self.by.unwrap_or_default(),
            to: self.to,
            gas: self.gas.unwrap_or_default(),
            eth: self.eth.unwrap_or_default(),
            data: self.data,
        }
    }

    pub fn call(self, abi: &str, args: &[&[u8]]) -> Self {
        let len = 4 + args.iter().map(|arg| arg.len()).sum::<usize>();
        let selector = keccak256(abi.as_bytes());
        let selector = &selector.as_ref()[..4];
        let mut vec = Vec::with_capacity(len);
        vec.extend_from_slice(selector);
        for arg in args {
            vec.extend_from_slice(arg);
        }
        Self {
            data: vec.into(),
            ..self
        }
    }

    pub fn create(self, mut code: Vec<u8>, args: &[&[u8]]) -> Self {
        let len = args.iter().map(|arg| arg.len()).sum::<usize>();
        code.reserve(len);
        for arg in args {
            code.extend_from_slice(arg);
        }
        Self {
            data: code.into(),
            ..self
        }
    }

    pub fn by(self, by: Acc) -> Self {
        Self {
            by: Some(by),
            ..self
        }
    }

    pub fn to(self, to: Acc) -> Self {
        Self {
            to: Some(to),
            ..self
        }
    }

    pub fn gas(self, gas: u64) -> Self {
        Self {
            gas: Some(gas),
            ..self
        }
    }

    pub fn eth(self, eth: Int) -> Self {
        Self {
            eth: Some(eth),
            ..self
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    pub number: Hex<8>,
    pub hash: Int,
    pub gas_limit: Int,
    #[serde(alias = "miner")]
    pub coinbase: Acc,
    pub timestamp: Int,
    #[serde(alias = "baseFeePerGas")]
    pub base_fee: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excess_blob_gas: Option<Int>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobhash: Option<Int>,
    #[serde(alias = "mixHash")]
    #[serde(alias = "prevRandao")]
    pub prevrandao: Int,
    pub parent_hash: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_beacon_block_root: Option<Int>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tx {
    #[serde(default)]
    #[serde(skip_serializing_if = "Hex::is_zero")]
    pub chain_id: Hex<8>,
    pub nonce: Int,
    pub gas_price: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Int::is_zero")]
    pub max_fee_per_gas: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Int::is_zero")]
    pub max_priority_fee_per_gas: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub access_list: Vec<AccessListItem>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authorization_list: Vec<AuthorizationListItem>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blob_versioned_hashes: Vec<Int>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_blob_gas: Option<Int>,
    pub hash: Int,
    #[serde(rename = "transactionIndex")]
    pub index: Int,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    pub address: Acc,
    pub storage_keys: Vec<Int>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationListItem {
    pub address: Acc,
    pub chain_id: Int,
    pub nonce: Int,
    pub r: Int,
    pub s: Int,
    pub y_parity: Int,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TxCall {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Acc>,
    pub from: Acc,
    pub input: Buf,
    pub value: Int,
    pub gas: Int,
}

impl From<TxCall> for Call {
    fn from(call: TxCall) -> Self {
        Call {
            by: call.from,
            to: call.to,
            gas: call.gas.as_u64(),
            eth: call.value,
            data: call.input,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TxFull {
    #[serde(flatten)]
    pub call: TxCall,
    #[serde(flatten)]
    pub tx: Tx,
}

pub type TxHash = Int;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Block {
    #[serde(flatten)]
    pub head: Head,
    #[serde(rename = "transactions")]
    pub txs: Vec<TxFull>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub block_hash: Int,
    pub block_number: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<Acc>,
    pub cumulative_gas_used: Int,
    pub effective_gas_price: Int,
    pub gas_used: Int,
    pub status: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Acc>,
    pub r#type: Int,
    pub transaction_hash: Int,
    pub transaction_index: Int,
    pub logs: Vec<Logged>,
    pub logs_bloom: Buf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Logged {
    pub address: Acc,
    pub block_hash: Int,
    pub block_number: Int,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_timestamp: Option<Int>,
    pub data: Buf,
    pub log_index: Int,
    pub removed: bool,
    pub topics: Vec<Int>,
    pub transaction_hash: Int,
    pub transaction_index: Int,
}

// TODO: proper blob handling
/*
/// EIP-4844 BLOBBASEFEE calculation (fake_exponential)
/// (see: https://eips.ethereum.org/EIPS/eip-4844)
pub fn blob_base_fee(excess_blob_gas: u64) -> u128 {
    let d = 5_007_716u128;
    let mut i = 1u64;
    let mut out = 0;
    let mut acc = d;
    while acc > 0 {
        out += acc;
        acc = (acc * excess_blob_gas) / (d * i);
        i += 1;
    }
    out / d
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_block() {
        const JSON: &str = r#"{
                "baseFeePerGas": "0x246edf3",
                "blobGasUsed": "0x40000",
                "difficulty": "0x0",
                "excessBlobGas": "0xa2a720e",
                "extraData": "0x4275696c6465724e6574202842656176657229",
                "gasLimit": "0x3938700",
                "gasUsed": "0x1a4ed69",
                "hash": "0x6c4c6ea047f0ca4bcb0b0fbbcc2e50061d8dd24ba1ab8a22a6d3583a497c3c82",
                "logsBloom": "0xfffffef97ffdf7dfb75fbbfeffffdfffffbeefffffbbffff6ffffdffbffffaf9df7fff7f7fff6ffbffffdfdffbffffff76fffffcbf7ffbfffffeefffffff7df7bffffffdefffff8fdfdf7f7ceffdffff7ff77affe7ef5ff5fbfff57ff7eeadfffffffabfff7bdffbbfbffcfffdefffeffefffffffff5fffefdffdffbffffdfdffbffe7fffffffffdffebf7e37fdfe977f7fbdfefdffddffffffffff5f97ffffdfbffdf7fffffbff9feff77afffefffeef77ffff7fff6fffeffd77ffffeefefdb7bffffff7f7ffdfbff7ffffffffefdffdffddffffffefffffffefdfbf7fdffef7fffff3bdf7fff6cefffdfffdf7ffffeffbfbffffdafffdfbf7efdbeffff7fff",
                "miner": "0xdadb0d80178819f2319190d340ce9a924f783711",
                "mixHash": "0xf824cbc0bffaa4862fffafa96c2cce90898dda8b97fc3eaa6a85ac38075991e5",
                "nonce": "0x0000000000000000",
                "number": "0x177d354",
                "parentBeaconBlockRoot": "0xd9b957562f8477151f61eddc77bcad7451ef62eab8f4fe470402073a794e8bba",
                "parentHash": "0x63ce16602b2afe4089fa8e0a2c16be1a2dea1c4d30b65bd717d9398ddd5fba23",
                "receiptsRoot": "0x52fc280ab6b1db61781dae4cc5f81b7ddd484eae78320bdd8161d5b2fe8e7719",
                "requestsHash": "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
                "size": "0x2a8a9",
                "stateRoot": "0x41ebb402f36d0ce15799e98d1aca83bbb5d4b08d1b6572e73b388e4ece192916",
                "timestamp": "0x69b09d6b",
                "transactions": [
                    {
                        "accessList": [
                            {
                                "address": "0xf6e72db5454dd049d0788e411b06cfaf16853042",
                                "storageKeys": [
                                    "0x0000000000000000000000000000000000000000000000000000000000000004"
                                ]
                            }
                        ],
                        "authorizationList": [
                            {
                                "address": "0x66e9d4bc80c1d04d045573c2f4631342fe6fc87d",
                                "chainId": "0x1",
                                "nonce": "0xa28",
                                "r": "0xedba2225e5a2a8958d7530ef3fdd941ba5996c8a92ddb60d649f1ccabb9d686d",
                                "s": "0x2a058bc60a7674c234739fefcbcf40bf846a7c4cf89ed850b86cb871033e7502",
                                "yParity": "0x0"
                            }
                        ],
                        "blockHash": "0x1f5b98ca47015bbfa9bff1b6282fd7753a0b79053876677b22ac178df53d76c7",
                        "blockNumber": "0x177e232",
                        "blockTimestamp": "0x69b1509f",
                        "chainId": "0x1",
                        "from": "0xdadb0d80178819f2319190d340ce9a924f783711",
                        "gas": "0x5208",
                        "gasPrice": "0x4b2f976",
                        "hash": "0xb478179e8e7f2fadfd42103007e9dfb5ea882667b1dd6e65142ef6b2a54b5668",
                        "input": "0x",
                        "maxFeePerGas": "0x4b2f976",
                        "maxPriorityFeePerGas": "0x0",
                        "nonce": "0x22792c",
                        "r": "0x51a8855fd9176012c328891ee0027959fd6480ef7ca09a2aac2e287c1c311566",
                        "s": "0x3e4b0c0cf854f42a0999c2ce6a9dc88b16f1b5f4e201dd61a36ed3a769719e6c",
                        "to": "0x08ec37e2eb451ab6fb29fc14d215b0aeef170040",
                        "transactionIndex": "0x201",
                        "type": "0x2",
                        "v": "0x1",
                        "value": "0x1f92fa85e3ea2a",
                        "yParity": "0x1"
                    }
                ],
                "transactionsRoot": "0x3b889eb9fa0713d2e1d9e90725d45fd04ec4a550a7a390639c243c5f3b2c6215",
                "uncles": [],
                "withdrawals": [
                    {
                    "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
                    "amount": "0x344741",
                    "index": "0x73e482b",
                    "validatorIndex": "0x21e64c"
                    }
                ],
                "withdrawalsRoot": "0x76abcbef0846a5bd065d0207581dacc3cf8df4ec2ba0a8e4d4fbbe97864b82ee"
            }"#;
        assert!(serde_json::from_str::<Block>(JSON).is_ok());
    }

    #[test]
    fn test_receipt() {
        const JSON: &str = r#"  {
            "blockHash": "0xe80afa4e302440b3f13974819d24027d1c5c7663a9688c5f03caa685660a28c7",
            "blockNumber": "0x178f9f8",
            "contractAddress": null,
            "cumulativeGasUsed": "0x216cc",
            "effectiveGasPrice": "0x1ef66a0",
            "from": "0xe4613af9ecdf8db83707546af9d4bd9da76ce504",
            "gasUsed": "0x216cc",
            "logs": [
            {
                "address": "0xe0f63a424a4439cbe457d80e4f4b51ad25b2c56c",
                "blockHash": "0xe80afa4e302440b3f13974819d24027d1c5c7663a9688c5f03caa685660a28c7",
                "blockNumber": "0x178f9f8",
                "blockTimestamp": "0x69be7e43",
                "data": "0x000000000000000000000000000000000000000000000000000000261a04825d",
                "logIndex": "0x0",
                "removed": false,
                "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                "0x00000000000000000000000051c72848c68a965f66fa7a88855f9f7784502a7f",
                "0x00000000000000000000000052c77b0cb827afbad022e6d6caf2c44452edbc39"
                ],
                "transactionHash": "0xd3aafbde18d85a863399c94ffec80af928bb3ebecc3685f1c784245deff04c04",
                "transactionIndex": "0x0"
            }
            ],
            "logsBloom": "0x002000000000000002000000800000010000000000000000000000000000000000000000000000000000000000000000020000000800000000000400000000000000000000000000000000080000002000000000000400000000000000000000000000000000000000000000000000000000000000000000000000100000000000000040000000000000000000000000000000000000000c0000004000000000000000000000000000000000000000004000000000000000000000000000000000000002000000000000000000000000000000000400001000000000000000000000200000000000000000000000000000400000000000000000000040006000",
            "status": "0x1",
            "to": "0x51c72848c68a965f66fa7a88855f9f7784502a7f",
            "transactionHash": "0xd3aafbde18d85a863399c94ffec80af928bb3ebecc3685f1c784245deff04c04",
            "transactionIndex": "0x0",
            "type": "0x2"
        }"#;
        assert!(serde_json::from_str::<Receipt>(JSON).is_ok());
    }
}
