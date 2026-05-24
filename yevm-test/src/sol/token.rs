use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, state::Account};
use yevm_misc::buf::Buf;

/// Deploy MockERC20 (from maker.sol) and Token, then test the buy/use/give/check flow.
/// Token requires an ERC20 for payment; MockERC20 provides a minimal implementation.
#[tokio::test]
async fn test_token_buy_use_give() -> eyre::Result<()> {
    let combined = super::load()?;
    let mock_src = &combined.contracts["sol/maker.sol:MockERC20"];
    let token_src = &combined.contracts["sol/token.sol:Token"];

    let deployer = acc("0xAA");
    let buyer = acc("0xBB");
    let recipient = acc("0xCC");

    // --- step 1: deploy MockERC20 ---
    let deploy_mock = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: mock_src.bin.clone(),
    };
    let env = vec![
        (
            deployer,
            Account {
                value: super::ethers(1),
                nonce: Int::ZERO,
                code: (Buf::default(), Int::ZERO),
            },
            vec![],
        ),
        (
            buyer,
            Account {
                value: super::ethers(1),
                nonce: Int::ZERO,
                code: (Buf::default(), Int::ZERO),
            },
            vec![],
        ),
        (
            recipient,
            Account {
                value: super::ethers(1),
                nonce: Int::ZERO,
                code: (Buf::default(), Int::ZERO),
            },
            vec![],
        ),
    ];

    let head = super::head();
    let tx0 = super::tx(0);
    let exp0 =
        crate::revm::run(deploy_mock.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res0 = super::run(deploy_mock, head.clone(), env, tx0).await?;
    super::assert_match(&res0, &exp0, "deploy MockERC20");

    let mock_addr: Acc = res0.0.to();
    let env1 = res0.4;

    // --- step 2: deploy Token(mock_addr, price=100, total=5) ---
    let mut token_deploy = token_src.bin.0.clone();
    token_deploy.extend_from_slice(mock_addr.to::<32>().as_ref());
    token_deploy.extend_from_slice(Int::from(100u64).as_ref()); // price
    token_deploy.extend_from_slice(Int::from(5u64).as_ref()); // total

    let deploy_token = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(token_deploy),
    };
    let tx1 = super::tx(1);
    let exp1 = crate::revm::run(
        deploy_token.clone(),
        head.clone(),
        env1.clone(),
        tx1.clone(),
    )
    .await?;
    let res1 = super::run(deploy_token, head.clone(), env1, tx1).await?;
    super::assert_match(&res1, &exp1, "deploy Token");

    let token_addr: Acc = res1.0.to();
    let env2 = res1.4;

    // --- step 3: mint 1000 tokens to buyer ---
    let mut mint_data = super::selector("mint(address,uint256)");
    mint_data.extend_from_slice(buyer.to::<32>().as_ref());
    mint_data.extend_from_slice(Int::from(1000u64).as_ref());

    let mint_call = Call {
        by: deployer,
        to: Some(mock_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(mint_data),
    };
    let tx2 = super::tx(2);
    let exp2 = crate::revm::run(mint_call.clone(), head.clone(), env2.clone(), tx2.clone()).await?;
    let res2 = super::run(mint_call, head.clone(), env2, tx2).await?;
    super::assert_match(&res2, &exp2, "mint");

    let env3 = res2.4;

    // --- step 4: buyer approves Token contract to spend 100 tokens ---
    let mut approve_data = super::selector("approve(address,uint256)");
    approve_data.extend_from_slice(token_addr.to::<32>().as_ref());
    approve_data.extend_from_slice(Int::from(100u64).as_ref());

    let approve_call = Call {
        by: buyer,
        to: Some(mock_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(approve_data),
    };
    let tx3 = super::tx(0);
    let exp3 = crate::revm::run(
        approve_call.clone(),
        head.clone(),
        env3.clone(),
        tx3.clone(),
    )
    .await?;
    let res3 = super::run(approve_call, head.clone(), env3, tx3).await?;
    super::assert_match(&res3, &exp3, "approve");

    let env4 = res3.4;

    // --- step 5: buyer calls buy() on Token ---
    let buy_call = Call {
        by: buyer,
        to: Some(token_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(super::selector("buy()")),
    };
    let tx4 = super::tx(1);
    let exp4 = crate::revm::run(buy_call.clone(), head.clone(), env4.clone(), tx4.clone()).await?;
    let res4 = super::run(buy_call, head.clone(), env4, tx4).await?;
    super::assert_match(&res4, &exp4, "buy()");

    let env5 = res4.4;

    // --- step 6: buyer calls use() on Token ---
    let use_call = Call {
        by: buyer,
        to: Some(token_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(super::selector("use()")),
    };
    let tx5 = super::tx(2);
    let exp5 = crate::revm::run(use_call.clone(), head.clone(), env5.clone(), tx5.clone()).await?;
    let res5 = super::run(use_call, head.clone(), env5, tx5).await?;
    super::assert_match(&res5, &exp5, "use()");

    let env6 = res5.4;

    // --- step 7: check(buyer) should return receipt seq ---
    let mut check_data = super::selector("check(address)");
    check_data.extend_from_slice(buyer.to::<32>().as_ref());
    let check_call = Call {
        by: deployer,
        to: Some(token_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(check_data),
    };
    let tx6 = super::tx(3);
    let exp6 =
        crate::revm::run(check_call.clone(), head.clone(), env6.clone(), tx6.clone()).await?;
    let res6 = super::run(check_call, head, env6, tx6).await?;
    super::assert_match(&res6, &exp6, "check()");

    Ok(())
}
