use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, aux::create_address, state::Account};
use yevm_misc::buf::Buf;

/// Deploy Vault, deposit ETH via give(), check balance via have(),
/// transfer via move(), and withdraw via take().
#[tokio::test]
async fn test_vault_give_move_take() -> eyre::Result<()> {
    let combined = super::load()?;
    let vault_src = &combined.contracts["sol/value.sol:Vault"];

    let alice = acc("0xAA");
    let bob = acc("0xBB");

    // Deploy Vault
    let deploy = Call {
        by: alice,
        to: None,
        gas: 500_000,
        eth: Int::ZERO,
        data: vault_src.bin.clone(),
    };
    let env = vec![
        (
            alice,
            Account {
                value: super::ethers(10),
                nonce: Int::ZERO,
                code: (Buf::default(), Int::ZERO),
            },
            vec![],
        ),
        (
            bob,
            Account {
                value: super::ethers(10),
                nonce: Int::ZERO,
                code: (Buf::default(), Int::ZERO),
            },
            vec![],
        ),
    ];

    let head = super::head();
    let tx0 = super::tx(0);
    let exp0 = crate::revm::run(deploy.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res0 = super::run(deploy, head.clone(), env, tx0).await?;
    super::assert_match(&res0, &exp0, "deploy Vault");

    let vault_addr: Acc = res0.0.to();
    assert_eq!(vault_addr, create_address(&alice, 0));
    let env1 = res0.4;

    // Alice calls give() with 1 ETH
    let give_call = Call {
        by: alice,
        to: Some(vault_addr),
        gas: 500_000,
        eth: super::ethers(1),
        data: Buf(super::selector("give()")),
    };
    let tx1 = super::tx(1);
    let exp1 = crate::revm::run(give_call.clone(), head.clone(), env1.clone(), tx1.clone()).await?;
    let res1 = super::run(give_call, head.clone(), env1, tx1).await?;
    super::assert_match(&res1, &exp1, "give()");

    let env2 = res1.4;

    // have(alice) should return 1 ETH
    let mut have_data = super::selector("have(address)");
    have_data.extend_from_slice(alice.to::<32>().as_ref());
    let have_call = Call {
        by: alice,
        to: Some(vault_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(have_data),
    };
    let tx2 = super::tx(2);
    let exp2 = crate::revm::run(have_call.clone(), head.clone(), env2.clone(), tx2.clone()).await?;
    let res2 = super::run(have_call, head.clone(), env2.clone(), tx2).await?;
    super::assert_match(&res2, &exp2, "have(alice)");

    // Alice moves 100 wei to Bob
    let mut move_data = super::selector("move(address,uint256)");
    move_data.extend_from_slice(bob.to::<32>().as_ref());
    move_data.extend_from_slice(Int::from(100u64).as_ref());
    let move_call = Call {
        by: alice,
        to: Some(vault_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(move_data),
    };
    let tx3 = super::tx(2);
    let exp3 = crate::revm::run(move_call.clone(), head.clone(), env2.clone(), tx3.clone()).await?;
    let res3 = super::run(move_call, head.clone(), env2, tx3).await?;
    super::assert_match(&res3, &exp3, "move()");

    let env3 = res3.4;

    // Bob calls take(50) to withdraw 50 wei
    let mut take_data = super::selector("take(uint256)");
    take_data.extend_from_slice(Int::from(50u64).as_ref());
    let take_call = Call {
        by: bob,
        to: Some(vault_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(take_data),
    };
    let tx4 = super::tx(0);
    let exp4 = crate::revm::run(take_call.clone(), head.clone(), env3.clone(), tx4.clone()).await?;
    let res4 = super::run(take_call, head, env3, tx4).await?;
    super::assert_match(&res4, &exp4, "take()");

    Ok(())
}
