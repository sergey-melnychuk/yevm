use yevm_base::{Int, acc};
use yevm_core::{Call, state::Account};
use yevm_misc::buf::Buf;

use crate::sol::{assert_match, ethers, head, load, run, tx};

#[tokio::test]
async fn test_deploy_counter() -> eyre::Result<()> {
    let combined = load()?;
    let contract = &combined.contracts["sol/count.sol:Count"];

    let sender = acc("0xAA");
    let env = vec![(
        sender,
        Account {
            value: ethers(1),
            nonce: Int::ZERO,
            code: (Buf::default(), Int::ZERO),
        },
        vec![],
    )];

    let mut head = head();
    head.gas_limit = 1_000_000.into();
    let call = Call {
        by: sender,
        to: None,
        gas: 1_000_000,
        eth: Int::ZERO,
        data: contract.bin.clone(),
    };
    let tx0 = tx(0);

    let exp = crate::revm::run(call.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res = run(call, head, env, tx0).await?;
    assert_match(&res, &exp, "deploy Count");
    Ok(())
}
