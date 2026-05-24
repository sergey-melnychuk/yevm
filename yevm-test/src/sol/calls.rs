use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, misc::create_address, state::Account};
use yevm_misc::buf::Buf;

/// Deploy Caller (constructor stores x), then call create() to deploy Callee,
/// then call call(a, b) which triggers Callee → Caller.callback cross-contract calls.
/// Each step is compared against revm for correctness.
#[tokio::test]
async fn test_caller_callee_chain() -> eyre::Result<()> {
    let combined = super::load()?;
    let caller_src = &combined.contracts["sol/calls.sol:Caller"];

    let deployer = acc("0xAA");
    let caller_addr = create_address(&deployer, 0);

    // --- step 1: deploy Caller with constructor arg x = 10 ---
    let mut deploy_data = caller_src.bin.0.clone();
    deploy_data.extend_from_slice(Int::from(10u64).as_ref());

    let deploy_call = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(deploy_data),
    };
    let env = vec![(
        deployer,
        Account {
            value: super::ethers(1),
            nonce: Int::ZERO,
            code: (Buf::default(), Int::ZERO),
        },
        vec![],
    )];

    let head = super::head();
    let tx0 = super::tx(0);
    let exp = crate::revm::run(deploy_call.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res = super::run(deploy_call, head.clone(), env, tx0).await?;
    super::assert_match(&res, &exp, "deploy Caller");

    let deployed: Acc = res.0.to();
    assert_eq!(deployed, caller_addr, "Caller deployed at expected address");

    let env1 = res.4;

    // --- step 2: call Caller.create() to deploy Callee ---
    let create_call = Call {
        by: deployer,
        to: Some(caller_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(super::selector("create()")),
    };
    let tx1 = super::tx(1);
    let exp1 =
        crate::revm::run(create_call.clone(), head.clone(), env1.clone(), tx1.clone()).await?;
    let res1 = super::run(create_call, head.clone(), env1, tx1).await?;
    super::assert_match(&res1, &exp1, "create()");
    assert_eq!(res1.0, Int::ONE, "create() must succeed");

    // Extract Callee address from return data (ABI-encoded address).
    let callee_addr: Acc = Int::from(&res1.1.0[..32]).to();
    let expected_callee_addr = create_address(&caller_addr, 1);
    assert_eq!(
        callee_addr, expected_callee_addr,
        "Callee deployed at expected address"
    );

    let env2 = res1.4;

    // --- step 3: call Caller.call(a, b) → Callee.call → Caller.callback → a + b - x ---
    let mut call_data = super::selector("call(uint256,uint256)");
    call_data.extend_from_slice(Int::from(30u64).as_ref());
    call_data.extend_from_slice(Int::from(25u64).as_ref());

    let chain_call = Call {
        by: deployer,
        to: Some(caller_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(call_data),
    };
    let tx2 = super::tx(2);
    let exp2 =
        crate::revm::run(chain_call.clone(), head.clone(), env2.clone(), tx2.clone()).await?;
    let res2 = super::run(chain_call, head, env2, tx2).await?;
    super::assert_match(&res2, &exp2, "call(a, b)");

    Ok(())
}
