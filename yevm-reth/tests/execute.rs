//! Block execution with yevm, checked against reth's evm2-backed Ethereum configuration.

use alloy_consensus::{Header, TxEip1559, constants::ETH_TO_WEI};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip4895::{Withdrawal, Withdrawals},
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7251::{CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, CONSOLIDATION_REQUEST_PREDEPLOY_CODE},
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, b256, bytes, keccak256};
use evm2::{
    EvmFeatures, NoopInspector, TxResult,
    bytecode::Bytecode,
    evm::{AccountInfo, InMemoryDB, PendingState},
    interpreter::InstrStop,
};
use reth_chainspec::{ChainSpec, ChainSpecBuilder, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody, Transaction, TransactionSigned};
use reth_evm::{
    BlockExecutionOutput, BlockExecutor, ConfigureEvm, Evm, EvmEnv, StateUpdate, execute::Executor,
};
use reth_evm_ethereum::{EthEvmConfig, EthEvmEnv};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{Block as _, RecoveredBlock, SignedTransaction};
use reth_testing_utils::generators::{self, sign_tx_with_key_pair};
use secp256k1::Keypair;
use std::sync::{Arc, Mutex};
use yevm_reth::exec::YevmEvmConfig;

/// Stores 42 in slot 0 and logs the first memory word with one topic.
const STORE_AND_LOG: Bytes = bytes!(
    "602a600055"
    "7fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    "60206000a1"
    "00"
);
/// Stores 1 in slot 0, then reverts with empty data.
const STORE_AND_REVERT: Bytes = bytes!("600160005560006000fd");
/// Init code that deploys `602a60005500` (store 42 in slot 0).
const DEPLOY: Bytes = bytes!("6006600c60003960066000f3602a60005500");

/// Runtime code of a minimal ERC-20 (`balanceOf` mapping at slot 0, checked arithmetic,
/// `Transfer` event, `require(balance >= value, "balance")`), compiled with solc 0.8.27
/// `--optimize`; see `benches/execute.rs`.
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

const STORE_AND_LOG_ADDRESS: Address = address!("0x1000000000000000000000000000000000000001");
const TOKEN_ADDRESS: Address = address!("0x1000000000000000000000000000000000000003");
const STORE_AND_REVERT_ADDRESS: Address = address!("0x1000000000000000000000000000000000000002");
const RECIPIENT: Address = address!("0x2000000000000000000000000000000000000002");
const WITHDRAWAL_RECIPIENT: Address = address!("0x3000000000000000000000000000000000000003");

fn chain_spec() -> Arc<ChainSpec> {
    Arc::new(ChainSpecBuilder::from(&*MAINNET).osaka_activated().build())
}

fn contract(code: Bytes) -> AccountInfo {
    AccountInfo {
        nonce: 1,
        code_hash: keccak256(&code),
        code: Some(Bytecode::new_raw(code)),
        ..AccountInfo::empty()
    }
}

/// Returns a database with the Osaka system contracts, two test contracts, and a funded sender.
fn database(sender: Address) -> InMemoryDB {
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
        (STORE_AND_LOG_ADDRESS, STORE_AND_LOG),
        (STORE_AND_REVERT_ADDRESS, STORE_AND_REVERT),
    ] {
        db.insert_account_info(&address, contract(code));
    }
    db.insert_account_info(
        &sender,
        AccountInfo {
            balance: U256::from(ETH_TO_WEI),
            ..AccountInfo::empty()
        },
    );
    db
}

fn sender() -> (Keypair, Address) {
    let key_pair = generators::generate_key(&mut generators::rng());
    let address =
        Address::from_raw_public_key(&key_pair.public_key().serialize_uncompressed()[1..]);
    (key_pair, address)
}

fn tx(key_pair: Keypair, nonce: u64, to: TxKind, value: U256, input: Bytes) -> TransactionSigned {
    sign_tx_with_key_pair(
        key_pair,
        Transaction::Eip1559(TxEip1559 {
            chain_id: MAINNET.chain.id(),
            nonce,
            gas_limit: 200_000,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 3,
            to,
            value,
            input,
            ..Default::default()
        }),
    )
}

fn header() -> Header {
    Header {
        number: 1,
        timestamp: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
        parent_hash: b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
        beneficiary: address!("0x4000000000000000000000000000000000000004"),
        ..Header::default()
    }
}

fn block(transactions: Vec<TransactionSigned>) -> RecoveredBlock<Block> {
    Block {
        header: header(),
        body: BlockBody {
            transactions,
            ommers: Vec::new(),
            withdrawals: Some(Withdrawals::new(vec![Withdrawal {
                index: 0,
                validator_index: 0,
                address: WITHDRAWAL_RECIPIENT,
                amount: 5,
            }])),
        },
    }
    .try_into_recovered()
    .unwrap()
}

/// Executes `block` with `evm_config` and returns its result and state changes.
fn execute(
    evm_config: impl ConfigureEvm<Primitives = reth_ethereum_primitives::EthPrimitives>,
    db: InMemoryDB,
    block: &RecoveredBlock<Block>,
) -> (
    BlockExecutionResult<reth_ethereum_primitives::Receipt>,
    reth_evm::BundleState,
) {
    let mut executor = evm_config.batch_executor(db);
    let result = executor.execute_one(block).unwrap();
    (result, executor.into_state())
}

/// Executes `block` with `evm_config`, recording the state hook's updates with accounts and slots
/// sorted, as their stream order is unspecified.
///
/// With `detached`, transactions are executed without committing and their detached state is
/// committed afterwards; otherwise the block executor runs them as a node does.
fn execute_with_updates(
    evm_config: impl ConfigureEvm<Primitives = reth_ethereum_primitives::EthPrimitives>,
    db: InMemoryDB,
    block: &RecoveredBlock<Block>,
    detached: bool,
) -> (
    BlockExecutionOutput<reth_ethereum_primitives::Receipt>,
    Vec<StateUpdate>,
) {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let sink = updates.clone();
    let hook = move |update| sink.lock().unwrap().push(update);
    let output = if detached {
        let mut executor = evm_config
            .executor_for_block(db, block.sealed_block())
            .unwrap();
        assert!(executor.set_state_hook(hook));
        executor.apply_pre_execution_changes().unwrap();
        for transaction in block.transactions_recovered() {
            let output = executor
                .execute_transaction_without_commit(transaction)
                .unwrap();
            executor.commit_transaction(output).unwrap();
        }
        executor.finish().unwrap()
    } else {
        evm_config
            .batch_executor(db)
            .execute_with_state_hook(block, hook)
            .unwrap()
    };
    let mut updates = core::mem::take(&mut *updates.lock().unwrap());
    for update in &mut updates {
        update.accounts.sort_by_key(|account| account.address);
        for account in &mut update.accounts {
            account.storage.sort_by_key(|(key, _)| *key);
        }
    }
    (output, updates)
}

#[test]
fn osaka_block_matches_evm2() {
    let (key_pair, sender) = sender();
    let db = database(sender);
    let block = block(vec![
        tx(
            key_pair,
            0,
            TxKind::Call(RECIPIENT),
            U256::from(1),
            Bytes::new(),
        ),
        tx(
            key_pair,
            1,
            TxKind::Call(STORE_AND_LOG_ADDRESS),
            U256::ZERO,
            Bytes::new(),
        ),
        tx(
            key_pair,
            2,
            TxKind::Call(STORE_AND_REVERT_ADDRESS),
            U256::ZERO,
            Bytes::new(),
        ),
        tx(key_pair, 3, TxKind::Create, U256::from(7), DEPLOY),
    ]);

    let (evm2_result, evm2_state) = execute(EthEvmConfig::new(chain_spec()), db.clone(), &block);
    let (yevm_result, yevm_state) = execute(YevmEvmConfig::new(chain_spec()), db, &block);

    let statuses: Vec<_> = yevm_result
        .receipts
        .iter()
        .map(|receipt| receipt.success)
        .collect();
    assert_eq!(statuses, [true, true, false, true]);
    assert_eq!(yevm_result.receipts[1].logs.len(), 1);
    assert_eq!(yevm_result, evm2_result);
    assert_eq!(yevm_state, evm2_state);
}

/// yevm commits a transaction's state straight from its journal when no caller sees the detached
/// state, and through a detached pending state otherwise; both must match evm2, including the
/// updates streamed to state hooks.
#[test]
fn committed_state_and_state_hook_updates_match_evm2() {
    let (key_pair, sender) = sender();
    let db = database(sender);
    let block = block(vec![
        tx(
            key_pair,
            0,
            TxKind::Call(RECIPIENT),
            U256::from(1),
            Bytes::new(),
        ),
        tx(
            key_pair,
            1,
            TxKind::Call(STORE_AND_LOG_ADDRESS),
            U256::ZERO,
            Bytes::new(),
        ),
        tx(
            key_pair,
            2,
            TxKind::Call(STORE_AND_REVERT_ADDRESS),
            U256::ZERO,
            Bytes::new(),
        ),
        tx(key_pair, 3, TxKind::Create, U256::from(7), DEPLOY),
    ]);

    let (evm2_output, evm2_updates) =
        execute_with_updates(EthEvmConfig::new(chain_spec()), db.clone(), &block, false);
    assert!(
        evm2_updates
            .iter()
            .any(|update| update.accounts.iter().any(|a| !a.storage.is_empty()))
    );
    assert!(
        evm2_updates
            .iter()
            .any(|update| update.accounts.iter().any(|a| a.created))
    );
    for detached in [false, true] {
        let (output, updates) = execute_with_updates(
            YevmEvmConfig::new(chain_spec()),
            db.clone(),
            &block,
            detached,
        );
        assert_eq!(output.result, evm2_output.result, "detached: {detached}");
        assert_eq!(output.state, evm2_output.state, "detached: {detached}");
        let mut expected = evm2_updates.clone();
        if detached {
            // evm2's public pending state API cannot mark accounts as created, so detached
            // state reports created accounts as changed ones.
            for account in expected.iter_mut().flat_map(|update| &mut update.accounts) {
                account.created = false;
            }
        }
        assert_eq!(updates, expected, "detached: {detached}");
    }
}

#[test]
fn invalid_nonce_matches_evm2() {
    let (key_pair, sender) = sender();
    let block = block(vec![tx(
        key_pair,
        1,
        TxKind::Call(RECIPIENT),
        U256::from(1),
        Bytes::new(),
    )]);

    let evm2_error = EthEvmConfig::new(chain_spec())
        .batch_executor(database(sender))
        .execute_one(&block)
        .unwrap_err();
    let yevm_error = YevmEvmConfig::new(chain_spec())
        .batch_executor(database(sender))
        .execute_one(&block)
        .unwrap_err();

    assert_eq!(yevm_error.to_string(), evm2_error.to_string());
}

#[test]
fn rejects_blocks_before_osaka() {
    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).prague_activated().build());
    let (_, sender) = sender();

    let error = YevmEvmConfig::new(chain_spec)
        .batch_executor(database(sender))
        .execute_one(&block(Vec::new()))
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "yevm only implements OSAKA, cannot execute PRAGUE"
    );
}

#[test]
fn rejects_evm2_inspectors() {
    let (key_pair, sender) = sender();
    let evm_config = YevmEvmConfig::new(chain_spec());
    let evm_env = evm_config.evm_env(&header()).unwrap();
    let mut evm = evm_config.evm_with_env(database(sender), evm_env);
    let tx = tx(
        key_pair,
        0,
        TxKind::Call(RECIPIENT),
        U256::from(1),
        Bytes::new(),
    )
    .try_into_recovered()
    .unwrap()
    .convert();

    evm.set_inspector(NoopInspector::default());
    let error = evm.transact(&tx).unwrap_err();

    assert_eq!(
        error.to_string(),
        "fatal error: yevm cannot run evm2 inspectors (tracing is not supported)"
    );
}

/// Engine prewarming executes transactions only to load their state into the shared cache, so
/// `transact_and_discard` must load it before failing, and must not keep the writes.
#[test]
fn transact_and_discard_loads_state_without_writes() {
    let (key_pair, sender) = sender();
    let evm_config = YevmEvmConfig::new(chain_spec());
    let evm_env = evm_config.evm_env(&header()).unwrap();
    let mut evm = evm_config.evm_with_env(database(sender), evm_env);
    let tx = tx(
        key_pair,
        0,
        TxKind::Call(STORE_AND_LOG_ADDRESS),
        U256::from(1),
        Bytes::new(),
    )
    .try_into_recovered()
    .unwrap()
    .convert();

    let error = evm.transact_and_discard(&tx, |_| ()).unwrap_err();
    let cache = evm.take_state_cache();

    assert_eq!(
        error.to_string(),
        "yevm does not produce evm2 executed-transaction views"
    );
    let sender_info = cache.accounts[&sender].as_ref().unwrap();
    assert_eq!(
        (sender_info.nonce, sender_info.balance),
        (0, U256::from(ETH_TO_WEI))
    );
    assert_eq!(
        cache.storage[&STORE_AND_LOG_ADDRESS].slots[&U256::ZERO],
        U256::ZERO
    );
}

/// `eth_call` disables the balance check; a value the sender cannot pay must then fail the call
/// with `OutOfFunds`, as with evm2, instead of executing it.
#[test]
fn underfunded_call_without_balance_check_matches_evm2() {
    let (key_pair, sender) = sender();
    let mut db = database(sender);
    db.insert_account_info(
        &sender,
        AccountInfo {
            balance: U256::from(1),
            ..AccountInfo::empty()
        },
    );
    let tx = tx(
        key_pair,
        0,
        TxKind::Call(STORE_AND_LOG_ADDRESS),
        U256::from(2),
        Bytes::new(),
    )
    .try_into_recovered()
    .unwrap()
    .convert();

    let result = |evm_config: &dyn Fn(InMemoryDB) -> TxResult| evm_config(db.clone());
    let eth_call_env = |mut evm_env: EthEvmEnv| {
        evm_env.version.features.remove(
            EvmFeatures::FEE_CHARGE | EvmFeatures::BALANCE_CHECK | EvmFeatures::BALANCE_TOP_UP,
        );
        evm_env
    };
    let evm2 = result(&|db| {
        let evm_config = EthEvmConfig::new(chain_spec());
        let evm_env = eth_call_env(evm_config.evm_env(&header()).unwrap());
        evm_config
            .evm_with_env(db, evm_env)
            .transact_result(&tx)
            .unwrap()
    });
    let yevm = result(&|db| {
        let evm_config = YevmEvmConfig::new(chain_spec());
        let evm_env = eth_call_env(evm_config.evm_env(&header()).unwrap());
        evm_config
            .evm_with_env(db, evm_env)
            .transact_result(&tx)
            .unwrap()
    });

    assert_eq!(yevm.stop, InstrStop::OutOfFunds);
    assert_eq!(yevm, evm2);
}

/// With the balance check disabled but the top-up enabled, as in engine prewarming, a sender that
/// cannot pay is credited its upfront cost, and the credit is part of the state changes.
#[test]
fn balance_top_up_matches_evm2() {
    let (key_pair, sender) = sender();
    let mut db = database(sender);
    db.insert_account_info(
        &sender,
        AccountInfo {
            balance: U256::from(1),
            ..AccountInfo::empty()
        },
    );
    let tx = tx(
        key_pair,
        0,
        TxKind::Call(STORE_AND_LOG_ADDRESS),
        U256::from(2),
        Bytes::new(),
    )
    .try_into_recovered()
    .unwrap()
    .convert();

    let evm2_config = EthEvmConfig::new(chain_spec());
    let evm2_env = evm2_config
        .evm_env(&header())
        .unwrap()
        .with_balance_check_disabled();
    let evm2 = Evm::transact(&mut evm2_config.evm_with_env(db.clone(), evm2_env), &tx).unwrap();
    let yevm_config = YevmEvmConfig::new(chain_spec());
    let yevm_env = yevm_config
        .evm_env(&header())
        .unwrap()
        .with_balance_check_disabled();
    let yevm = Evm::transact(&mut yevm_config.evm_with_env(db, yevm_env), &tx).unwrap();

    assert!(yevm.result.is_success());
    assert_eq!(yevm.result, evm2.result);
    let balance = |state: &PendingState| state.account_info(&sender).unwrap().balance;
    assert_eq!(balance(&yevm.pending_state), balance(&evm2.pending_state));
}

/// Token transfers exercise Solidity dispatch, ABI decoding, mappings, checked arithmetic, logs
/// and a `require` revert with a reason.
#[test]
fn erc20_transfers_match_evm2() {
    let (key_pair, sender) = sender();
    let mut db = database(sender);
    db.insert_account_info(&TOKEN_ADDRESS, contract(TOKEN));
    let balance_slot = keccak256([sender.into_word().0, [0; 32]].concat());
    db.insert_account_storage(&TOKEN_ADDRESS, &balance_slot.into(), &U256::from(100));
    let transfer = |nonce: u64, amount: u8| {
        let recipient = Address::with_last_byte(1).create(nonce);
        let input = [
            &TRANSFER[..],
            &recipient.into_word()[..],
            &B256::with_last_byte(amount)[..],
        ];
        tx(
            key_pair,
            nonce,
            TxKind::Call(TOKEN_ADDRESS),
            U256::ZERO,
            input.concat().into(),
        )
    };
    // Twenty transfers of 4 and one of 50, which exceeds the remaining balance of 20.
    let block = block(
        (0..20)
            .map(|nonce| transfer(nonce, 4))
            .chain([transfer(20, 50)])
            .collect(),
    );

    let (evm2_result, evm2_state) = execute(EthEvmConfig::new(chain_spec()), db.clone(), &block);
    let (yevm_result, yevm_state) = execute(YevmEvmConfig::new(chain_spec()), db, &block);

    let statuses: Vec<_> = yevm_result
        .receipts
        .iter()
        .map(|receipt| receipt.success)
        .collect();
    assert_eq!(statuses, [[true; 20].as_slice(), &[false]].concat());
    assert_eq!(yevm_result, evm2_result);
    assert_eq!(yevm_state, evm2_state);
}
