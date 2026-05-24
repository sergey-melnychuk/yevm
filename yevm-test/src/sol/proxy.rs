use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, misc::create_address, state::Account};
use yevm_misc::buf::Buf;

/// Deploy Logic and Proxy, then exercise delegatecall forwarding:
/// init(value) through Proxy stores in Proxy's storage, upgrade() changes impl,
/// and sending ETH to Proxy reverts via receive().
#[tokio::test]
async fn test_proxy_delegatecall() -> eyre::Result<()> {
    let combined = super::load()?;
    let logic_src = &combined.contracts["sol/proxy.sol:Logic"];
    let proxy_src = &combined.contracts["sol/proxy.sol:Proxy"];

    let deployer = acc("0xAA");
    let logic_addr = create_address(&deployer, 0);
    let proxy_addr = create_address(&deployer, 1);

    // --- step 1: deploy Logic ---
    let deploy_logic = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: logic_src.bin.clone(),
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
    let exp0 =
        crate::revm::run(deploy_logic.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res0 = super::run(deploy_logic, head.clone(), env, tx0).await?;
    super::assert_match(&res0, &exp0, "deploy Logic");

    let deployed_logic: Acc = res0.0.to();
    assert_eq!(
        deployed_logic, logic_addr,
        "Logic deployed at expected address"
    );
    let env1 = res0.4;

    // --- step 2: deploy Proxy(logic_addr) ---
    let mut proxy_deploy_data = proxy_src.bin.0.clone();
    proxy_deploy_data.extend_from_slice(logic_addr.to::<32>().as_ref());

    let deploy_proxy = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(proxy_deploy_data),
    };
    let tx1 = super::tx(1);
    let exp1 = crate::revm::run(
        deploy_proxy.clone(),
        head.clone(),
        env1.clone(),
        tx1.clone(),
    )
    .await?;
    let res1 = super::run(deploy_proxy, head.clone(), env1, tx1).await?;
    super::assert_match(&res1, &exp1, "deploy Proxy");

    let deployed_proxy: Acc = res1.0.to();
    assert_eq!(
        deployed_proxy, proxy_addr,
        "Proxy deployed at expected address"
    );

    let env2 = res1.4;
    let env_pre_init = env2.clone();

    // --- step 3: init(42) through Proxy (delegatecall to Logic) ---
    let mut init_data = super::selector("init(uint256)");
    init_data.extend_from_slice(Int::from(42u64).as_ref());

    let init_call = Call {
        by: deployer,
        to: Some(proxy_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(init_data),
    };
    let tx2 = super::tx(2);
    let exp2 = crate::revm::run(init_call.clone(), head.clone(), env2.clone(), tx2.clone()).await?;
    let res2 = super::run(init_call, head.clone(), env2, tx2).await?;
    super::assert_match(&res2, &exp2, "init(42) through proxy");

    let env3 = res2.4;

    // --- step 4: get() through Proxy ---
    // Note: after init(42), due to storage collision (Logic slot 0 = value overwrites
    // Proxy slot 0 = impl), the proxy may be broken. We verify yevm matches revm regardless.
    let get_call = Call {
        by: deployer,
        to: Some(proxy_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(super::selector("get()")),
    };
    let tx3 = super::tx(3);
    let exp3 = crate::revm::run(get_call.clone(), head.clone(), env3.clone(), tx3.clone()).await?;
    let res3 = super::run(get_call, head.clone(), env3, tx3).await?;
    super::assert_match(&res3, &exp3, "get() through proxy");

    // --- step 5: upgrade() on Proxy directly (not delegatecall) ---
    // Use the state from step 2 (before init) so the proxy is still functional.
    let new_impl = acc("0xDEAD");
    let mut upgrade_data = super::selector("upgrade(address)");
    upgrade_data.extend_from_slice(new_impl.to::<32>().as_ref());

    let upgrade_call = Call {
        by: deployer,
        to: Some(proxy_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(upgrade_data),
    };
    let tx4 = super::tx(2);
    let exp4 = crate::revm::run(
        upgrade_call.clone(),
        head.clone(),
        env_pre_init.clone(),
        tx4.clone(),
    )
    .await?;
    let res4 = super::run(upgrade_call, head.clone(), env_pre_init.clone(), tx4).await?;
    super::assert_match(&res4, &exp4, "upgrade()");

    // --- step 6: sending ETH to Proxy triggers receive() which reverts ---
    let eth_call = Call {
        by: deployer,
        to: Some(proxy_addr),
        gas: 500_000,
        eth: Int::from(1u64),
        data: Buf::default(),
    };
    let tx5 = super::tx(2);
    let exp5 = crate::revm::run(
        eth_call.clone(),
        head.clone(),
        env_pre_init.clone(),
        tx5.clone(),
    )
    .await?;
    let res5 = super::run(eth_call, head, env_pre_init, tx5).await?;
    super::assert_match(&res5, &exp5, "sending ETH to proxy");

    Ok(())
}
