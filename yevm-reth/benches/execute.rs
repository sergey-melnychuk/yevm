#![allow(missing_docs)]

//! Compares block execution with evm2 ([`EthEvmConfig`]) and yevm ([`YevmEvmConfig`]).

use alloy_consensus::{Header, TxEip1559, constants::ETH_TO_WEI};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7251::{CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, CONSOLIDATION_REQUEST_PREDEPLOY_CODE},
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, bytes, keccak256};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, InMemoryDB},
};
use reth_chainspec::{ChainSpec, ChainSpecBuilder, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives, Transaction};
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{Block as _, RecoveredBlock};
use reth_testing_utils::generators::{self, sign_tx_with_key_pair};
use std::sync::Arc;
use yevm_reth::exec::YevmEvmConfig;

/// Counts down from 30,000 in a `JUMPDEST` loop (26 gas per iteration).
const LOOP: Bytes = bytes!("6175305b600190038060035700");
/// Stores each counter value from 50 down to 1 in its own fresh slot.
const STORE: Bytes = bytes!("60325b80805560019003806002570000");

/// Runtime code of a minimal ERC-20 (`balanceOf` mapping at slot 0, checked arithmetic,
/// `Transfer` event), compiled with solc 0.8.27 `--optimize`:
///
/// ```solidity
/// function transfer(address to, uint256 value) external returns (bool) {
///     uint256 balance = balanceOf[msg.sender];
///     require(balance >= value, "balance");
///     unchecked { balanceOf[msg.sender] = balance - value; }
///     balanceOf[to] += value;
///     emit Transfer(msg.sender, to, value);
///     return true;
/// }
/// ```
const TOKEN: Bytes = bytes!(
    "608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb14"
    "61006a575b5f5ffd5b610057610046366004610178565b5f6020819052908152604090205481565b6040519081526020"
    "015b60405180910390f35b61007d610078366004610198565b61008d565b6040519015158152602001610061565b335f"
    "90815260208190526040812054828110156100da5760405162461bcd60e51b8152602060048201526007602482015266"
    "62616c616e636560c81b604482015260640160405180910390fd5b335f90815260208190526040808220858403905560"
    "01600160a01b03861682528120805485929061010c9084906101c0565b90915550506040518381526001600160a01b03"
    "85169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef90602001604051809103"
    "90a360019150505b92915050565b80356001600160a01b0381168114610173575f5ffd5b919050565b5f602082840312"
    "15610188575f5ffd5b6101918261015d565b9392505050565b5f5f604083850312156101a9575f5ffd5b6101b2836101"
    "5d565b946020939093013593505050565b8082018082111561015757634e487b7160e01b5f52601160045260245ffdfe"
    "a264697066735822122016da151f1f42ee0b19a35abc29e4b8b7e6cbdc501c2c61f2ffaa8a0d9d446dd264736f6c6343"
    "00081b0033"
);
/// Selector of `transfer(address,uint256)`.
const TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

const LOOP_ADDRESS: Address = address!("0x1000000000000000000000000000000000000001");
const STORE_ADDRESS: Address = address!("0x1000000000000000000000000000000000000002");
const TOKEN_ADDRESS: Address = address!("0x1000000000000000000000000000000000000003");

/// A workload's transactions: plain transfers, or calls to a contract.
#[derive(Clone, Copy)]
enum Calls {
    Transfers,
    Contract(Address),
    TokenTransfers,
}

fn contract(code: Bytes) -> AccountInfo {
    AccountInfo {
        nonce: 1,
        code_hash: keccak256(&code),
        code: Some(Bytecode::new_raw(code)),
        ..AccountInfo::empty()
    }
}

/// Builds a block of `count` transactions from one sender, with the database it executes on.
fn workload(calls: Calls, count: u64) -> (InMemoryDB, RecoveredBlock<Block>) {
    let key_pair = generators::generate_key(&mut generators::rng());
    let sender = Address::from_raw_public_key(&key_pair.public_key().serialize_uncompressed()[1..]);
    let mut db = InMemoryDB::default();
    for (address, code) in [
        (BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone()),
        (HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone()),
        (
            WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone(),
        ),
        (
            CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
            CONSOLIDATION_REQUEST_PREDEPLOY_CODE.clone(),
        ),
        (LOOP_ADDRESS, LOOP),
        (STORE_ADDRESS, STORE),
        (TOKEN_ADDRESS, TOKEN),
    ] {
        db.insert_account_info(&address, contract(code));
    }
    // The sender's token balance, `balanceOf[sender]` at `keccak256(sender . 0)`.
    let balance_slot = keccak256([sender.into_word().0, [0; 32]].concat());
    db.insert_account_storage(&TOKEN_ADDRESS, &balance_slot.into(), &U256::from(1_000_000));
    db.insert_account_info(
        &sender,
        AccountInfo {
            balance: U256::from(ETH_TO_WEI),
            ..AccountInfo::empty()
        },
    );

    let transactions = (0..count)
        .map(|nonce| {
            let recipient = Address::with_last_byte(1).create(nonce);
            let (to, value, input) = match calls {
                Calls::Transfers => (recipient, U256::from(1), Bytes::new()),
                Calls::Contract(to) => (to, U256::ZERO, Bytes::new()),
                Calls::TokenTransfers => {
                    let input = [
                        &TRANSFER[..],
                        &recipient.into_word()[..],
                        &B256::with_last_byte(1)[..],
                    ];
                    (TOKEN_ADDRESS, U256::ZERO, Bytes::from(input.concat()))
                }
            };
            sign_tx_with_key_pair(
                key_pair,
                Transaction::Eip1559(TxEip1559 {
                    chain_id: MAINNET.chain.id(),
                    nonce,
                    gas_limit: 2_000_000,
                    max_fee_per_gas: 20,
                    max_priority_fee_per_gas: 3,
                    to: TxKind::Call(to),
                    value,
                    input,
                    ..Default::default()
                }),
            )
        })
        .collect();
    let header = Header {
        number: 1,
        timestamp: 1,
        gas_limit: 60_000_000,
        base_fee_per_gas: Some(7),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::with_last_byte(1)),
        ..Header::default()
    };
    let block = Block {
        header,
        body: BlockBody {
            transactions,
            ommers: Vec::new(),
            withdrawals: Some(Default::default()),
        },
    }
    .try_into_recovered()
    .unwrap();
    (db, block)
}

fn execute(
    evm_config: &impl ConfigureEvm<Primitives = EthPrimitives>,
    db: &InMemoryDB,
    block: &RecoveredBlock<Block>,
) {
    evm_config
        .batch_executor(db.clone())
        .execute_one(block)
        .unwrap();
}

fn execute_blocks(c: &mut Criterion) {
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpecBuilder::from(&*MAINNET).osaka_activated().build());
    let evm2 = EthEvmConfig::new(chain_spec.clone());
    let yevm = YevmEvmConfig::new(chain_spec);

    let mut group = c.benchmark_group("execute_block");
    for (name, calls, count) in [
        ("transfers_200", Calls::Transfers, 200),
        ("loop_10x30k", Calls::Contract(LOOP_ADDRESS), 10),
        ("sstore_10x50", Calls::Contract(STORE_ADDRESS), 10),
        ("erc20_transfers_100", Calls::TokenTransfers, 100),
    ] {
        let (db, block) = workload(calls, count);
        group.bench_with_input(BenchmarkId::new("evm2", name), &(), |b, ()| {
            b.iter(|| execute(&evm2, &db, &block))
        });
        group.bench_with_input(BenchmarkId::new("yevm", name), &(), |b, ()| {
            b.iter(|| execute(&yevm, &db, &block))
        });
    }
    group.finish();
}

criterion_group!(benches, execute_blocks);
criterion_main!(benches);
