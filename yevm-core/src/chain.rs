use serde::{Deserialize, Serialize};
use yevm_base::{Acc, Int};
use yevm_misc::buf::Buf;

use crate::{
    Result,
    call::{Block, Head},
    evm::Fetch,
    state::{Account, State},
};

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Chain {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int>;
    async fn acc(&self, acc: &Acc) -> eyre::Result<Account>;
    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)>;
    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64>;
    async fn balance(&self, acc: &Acc) -> eyre::Result<Int>;
    async fn head(&self, number: u64) -> eyre::Result<Head>;
    async fn block(&self, number: u64) -> eyre::Result<Block>;
    async fn chain_id(&self) -> eyre::Result<u64>;
}

#[allow(clippy::large_enum_variant)] // TODO: wrap Block with Box?
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Fetched {
    Account(Acc, Account),
    State(Acc, Int, Int),
    Hash(u64, Int),
    Block(Block),
    ChainId(u64),
}

pub async fn fetch(f: Fetch, state: &mut impl State, chain: &impl Chain) -> Result<()> {
    match f {
        Fetch::Account(acc) | Fetch::Balance(acc) | Fetch::Nonce(acc) | Fetch::Code(acc) => {
            if state.is_offline() {
                let next = state.next_fetched();
                let Some(Fetched::Account(_, account)) = next else {
                    return Err(
                        eyre::eyre!("Offline fetch: expected account but got {next:?}").into(),
                    );
                };
                state.merge(&acc, account.clone());
            } else {
                #[cfg(not(target_arch = "wasm32"))]
                let now = std::time::Instant::now();

                let account = chain.acc(&acc).await?;

                #[cfg(not(target_arch = "wasm32"))]
                let millis = now.elapsed().as_micros() as f64 / 1000.0;
                #[cfg(target_arch = "wasm32")]
                let millis = 0;

                state.merge(&acc, account.clone());
                state.save_fetched(Fetched::Account(acc, account), millis);
            }
            Ok(())
        }
        Fetch::BlockHash(number) => {
            if state.is_offline() {
                let next = state.next_fetched();
                let Some(Fetched::Hash(number, hash)) = next else {
                    return Err(
                        eyre::eyre!("Offline fetch: expected block hash but got {next:?}").into(),
                    );
                };
                state.hash(number, hash);
            } else {
                #[cfg(not(target_arch = "wasm32"))]
                let now = std::time::Instant::now();

                let hash = chain
                    .head(number)
                    .await
                    .map(|head| head.hash)
                    .unwrap_or(Int::ZERO);

                #[cfg(not(target_arch = "wasm32"))]
                let millis = now.elapsed().as_micros() as f64 / 1000.0;
                #[cfg(target_arch = "wasm32")]
                let millis = 0;

                state.hash(number, hash);
                state.save_fetched(Fetched::Hash(number, hash), millis);
            }
            Ok(())
        }
        Fetch::StateCell(acc, key) => {
            if state.is_offline() {
                let next = state.next_fetched();
                let Some(Fetched::State(_, _, val)) = next else {
                    return Err(
                        eyre::eyre!("Offline fetch: expected state cell but got {next:?}").into(),
                    );
                };
                state.init(&acc, &key, val);
            } else {
                #[cfg(not(target_arch = "wasm32"))]
                let now = std::time::Instant::now();

                let val = chain.get(&acc, &key).await?;

                #[cfg(not(target_arch = "wasm32"))]
                let millis = now.elapsed().as_micros() as f64 / 1000.;
                #[cfg(target_arch = "wasm32")]
                let millis = 0;

                state.init(&acc, &key, val);
                state.save_fetched(Fetched::State(acc, key, val), millis);
            }
            Ok(())
        }
    }
}
