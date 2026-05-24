use yevm_base::{Acc, Int, acc};
use yevm_core::{Call, state::Account};
use yevm_misc::buf::Buf;

/// Deploy Setup, then call simulate() which internally deploys MockERC20 tokens
/// and a Maker pool, runs a full liquidity + swap + withdraw cycle, and returns
/// the results via a revert-decode trick.
///
/// Ignored by default: simulate() executes many internal CREATE + CALL operations
/// and the trace collection makes it too memory-intensive for CI.
/// Run with: cargo test sol::maker -- --ignored
#[tokio::test]
// #[ignore]
async fn test_maker_simulate() -> eyre::Result<()> {
    let combined = super::load()?;
    let setup_src = &combined.contracts["sol/maker.sol:Setup"];

    let deployer = acc("0xAA");

    // Deploy Setup (large contract — needs more gas)
    let deploy = Call {
        by: deployer,
        to: None,
        gas: 3_000_000,
        eth: Int::ZERO,
        data: setup_src.bin.clone(),
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
    let exp0 = crate::revm::run(deploy.clone(), head.clone(), env.clone(), tx0.clone()).await?;
    let res0 = super::run(deploy, head.clone(), env, tx0).await?;
    super::assert_match(&res0, &exp0, "deploy Setup");

    let setup_addr: Acc = res0.0.to();
    let env1 = res0.4;

    // simulate() calls run() via try/catch, which deploys tokens, adds liquidity,
    // swaps, removes liquidity, then reverts with the results.
    let simulate_call = Call {
        by: deployer,
        to: Some(setup_addr),
        gas: 5_000_000,
        eth: Int::ZERO,
        data: Buf(super::selector("simulate()")),
    };
    let tx1 = super::tx(1);
    let exp1 = crate::revm::run(
        simulate_call.clone(),
        head.clone(),
        env1.clone(),
        tx1.clone(),
    )
    .await?;
    let res1 = super::run(simulate_call, head, env1, tx1).await?;
    super::assert_match(&res1, &exp1, "simulate()");

    Ok(())
}
