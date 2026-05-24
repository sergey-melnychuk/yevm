use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, aux::create_address, state::Account};
use yevm_misc::buf::Buf;

/// Deploy Flash and MockBorrower, fund both, then execute a flash loan.
/// The MockBorrower receives the loan and repays it in the same transaction.
#[tokio::test]
async fn test_flash_loan() -> eyre::Result<()> {
    let combined = super::load()?;
    let flash_src = &combined.contracts["sol/flash.sol:Flash"];
    let borrower_src = &combined.contracts["sol/flash.sol:MockBorrower"];

    let deployer = acc("0xAA");
    let flash_addr = create_address(&deployer, 0);

    // --- step 1: deploy Flash ---
    let deploy_flash = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: flash_src.bin.clone(),
    };
    let env = vec![(
        deployer,
        Account {
            value: super::ethers(10),
            nonce: Int::ZERO,
            code: (Buf::default(), Int::ZERO),
        },
        vec![],
    )];

    let head = super::head();
    let tx0 = super::tx(0);
    let exp0 =
        crate::revm::run(deploy_flash.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res0 = super::run(deploy_flash, head.clone(), env, tx0).await?;
    super::assert_match(&res0, &exp0, "deploy Flash");

    let deployed_flash: Acc = res0.0.to();
    assert_eq!(
        deployed_flash, flash_addr,
        "Flash deployed at expected address"
    );
    let env1 = res0.4;

    // --- step 2: deploy MockBorrower(flash_addr) ---
    let mut borrower_deploy = borrower_src.bin.0.clone();
    borrower_deploy.extend_from_slice(flash_addr.to::<32>().as_ref());

    let deploy_borrower = Call {
        by: deployer,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(borrower_deploy),
    };
    let tx1 = super::tx(1);
    let exp1 = crate::revm::run(
        deploy_borrower.clone(),
        head.clone(),
        env1.clone(),
        tx1.clone(),
    )
    .await?;
    let res1 = super::run(deploy_borrower, head.clone(), env1, tx1).await?;
    super::assert_match(&res1, &exp1, "deploy MockBorrower");

    let borrower_addr: Acc = res1.0.to();
    let env2 = res1.4;

    // --- step 3: fund Flash with 5 ETH (send ETH to its receive()) ---
    let fund_flash = Call {
        by: deployer,
        to: Some(flash_addr),
        gas: 500_000,
        eth: super::ethers(5),
        data: Buf::default(),
    };
    let tx2 = super::tx(2);
    let exp2 =
        crate::revm::run(fund_flash.clone(), head.clone(), env2.clone(), tx2.clone()).await?;
    let res2 = super::run(fund_flash, head.clone(), env2, tx2).await?;
    super::assert_match(&res2, &exp2, "fund Flash");

    let env3 = res2.4;

    // --- step 4: fund MockBorrower with 2 ETH (so it can repay) ---
    let fund_borrower = Call {
        by: deployer,
        to: Some(borrower_addr),
        gas: 500_000,
        eth: super::ethers(2),
        data: Buf::default(),
    };
    let tx3 = super::tx(3);
    let exp3 = crate::revm::run(
        fund_borrower.clone(),
        head.clone(),
        env3.clone(),
        tx3.clone(),
    )
    .await?;
    let res3 = super::run(fund_borrower, head.clone(), env3, tx3).await?;
    super::assert_match(&res3, &exp3, "fund MockBorrower");

    let env4 = res3.4;

    // --- step 5: call Flash.loan(borrower, 1 ETH) ---
    let mut loan_data = super::selector("loan(address,uint256)");
    loan_data.extend_from_slice(borrower_addr.to::<32>().as_ref());
    loan_data.extend_from_slice(super::ethers(1).as_ref());

    let loan_call = Call {
        by: deployer,
        to: Some(flash_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(loan_data),
    };
    let tx4 = super::tx(4);
    let exp4 = crate::revm::run(loan_call.clone(), head.clone(), env4.clone(), tx4.clone()).await?;
    let res4 = super::run(loan_call, head, env4, tx4).await?;
    super::assert_match(&res4, &exp4, "flash loan");

    Ok(())
}
