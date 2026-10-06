//! Standalone unit-tests for the execution layer.

use yevm_base::{Acc, Int, acc};
use yevm_core::{
    Call,
    cache::Cache,
    exe::{CallResult, Executor},
    state::{Account, State},
};
use yevm_misc::buf::Buf;

use crate::eth::EmptyChain;
use crate::sol::{head, tx};

const SENDER: Acc = acc("0x00000000000000000000000000000000000000AA");
const DEPLOYER: Acc = acc("0x00000000000000000000000000000000000000DD");

/// Deployer runtime code: CREATE2(value=0, mem[30..32], salt=0) with init
/// code `CALLER SELFDESTRUCT` (0x33ff), then return the created address.
/// The child is created and self-destructed inside the same transaction.
fn deployer_code() -> Vec<u8> {
    vec![
        0x61, 0x33, 0xff, // PUSH2 0x33ff   (child init code)
        0x60, 0x00, // PUSH1 0
        0x52, // MSTORE          mem[0..32] = ..33ff (right-aligned)
        0x60, 0x00, // PUSH1 0   salt
        0x60, 0x02, // PUSH1 2   size
        0x60, 0x1e, // PUSH1 30  offset
        0x60, 0x00, // PUSH1 0   value
        0xf5, // CREATE2         -> [addr]
        0x60, 0x00, // PUSH1 0
        0x52, // MSTORE          mem[0..32] = addr
        0x60, 0x20, // PUSH1 32
        0x60, 0x00, // PUSH1 0
        0xf3, // RETURN
    ]
}

fn call_deployer(gas: u64) -> Call {
    Call {
        by: SENDER,
        to: Some(DEPLOYER),
        gas,
        eth: Int::ZERO,
        data: Buf::default(),
    }
}

/// EIP-6780 end-to-end regression: a same-salt CREATE2 redeploy must succeed
/// in the next transaction after the child self-destructed in its creation
/// tx. Before the `Cache::apply()` fix the destroyed child kept its nonce,
/// so the second CREATE2 hit the collision check, drained the inner gas, and
/// returned the zero address (mainnet shape: replay mismatch at block
/// 26135267, a same-salt redeployer bot).
#[tokio::test]
async fn create2_selfdestruct_then_redeploy_same_address() -> eyre::Result<()> {
    let mut state = Cache::new();
    state.set_chain_id(1);
    let h = head();
    state.insert_account(h.coinbase, Account::default());
    state.insert_account(
        SENDER,
        Account {
            value: crate::sol::ethers(1),
            nonce: Int::ZERO,
            code: (Buf::default(), Int::ZERO),
        },
    );
    state.insert_account(
        DEPLOYER,
        Account {
            value: Int::ZERO,
            nonce: Int::ONE,
            code: (deployer_code().into(), Int::from(1u32)),
        },
    );

    // Tx 1: deploy + same-tx selfdestruct.
    let r1 = Executor::new(call_deployer(500_000))
        .run(&tx(0), &h, &mut state, &EmptyChain)
        .await?;
    let CallResult::Done { status, ret, .. } = r1 else {
        eyre::bail!("tx1: expected Done, got {r1:?}");
    };
    assert!(!status.is_zero(), "tx1 must succeed");
    let child1 = Acc::from(&ret.as_slice()[12..32]);
    assert!(!child1.is_zero(), "tx1 must return the created address");

    // The destroyed child must read as a fully empty account.
    let account = state.acc(&child1).expect("explicitly empty entry");
    assert!(
        account.nonce.is_zero() && account.value.is_zero() && account.code.0.is_empty(),
        "child must be deleted entirely after same-tx selfdestruct (EIP-6780)"
    );

    // Tx boundary, then Tx 2: identical CREATE2 (same deployer, same salt,
    // same init code) must deploy again at the same address.
    state.reset();
    let r2 = Executor::new(call_deployer(500_000))
        .run(&tx(1), &h, &mut state, &EmptyChain)
        .await?;
    let CallResult::Done { status, ret, .. } = r2 else {
        eyre::bail!("tx2: expected Done, got {r2:?}");
    };
    assert!(!status.is_zero(), "tx2 must succeed");
    let child2 = Acc::from(&ret.as_slice()[12..32]);
    assert!(
        !child2.is_zero(),
        "tx2 CREATE2 must not collide with the destroyed child"
    );
    assert_eq!(child1, child2, "same salt => same address");
    Ok(())
}
