use serde::{Deserialize, Serialize};
use yaevmi_base::{Acc, Int};
use yaevmi_misc::buf::Buf;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Fetched {
    Account(Acc, Account),
    State(Acc, Int, Int),
    Hash(u64, Int),
    Block(Block),
}

pub async fn fetch(f: Fetch, state: &mut impl State, chain: &impl Chain) -> Result<()> {
    match f {
        Fetch::Account(acc) | Fetch::Balance(acc) | Fetch::Nonce(acc) | Fetch::Code(acc) => {
            if state.is_offline() {
                let Some(Fetched::Account(_, account)) = state.next_fetched() else {
                    return Err(eyre::eyre!("!").into());
                };
                state.merge(&acc, account.clone());
            } else {
                let account = chain.acc(&acc).await?;
                state.merge(&acc, account.clone());
                state.save_fetched(Fetched::Account(acc, account));
            }
            Ok(())
        }
        Fetch::BlockHash(number) => {
            if state.is_offline() {
                let Some(Fetched::Hash(number, hash)) = state.next_fetched() else {
                    return Err(eyre::eyre!("!").into());
                };
                state.hash(number, hash);
            } else {
                let hash = chain
                    .head(number)
                    .await
                    .map(|head| head.hash)
                    .unwrap_or(Int::ZERO);
                state.hash(number, hash);
                state.save_fetched(Fetched::Hash(number, hash));
            }
            Ok(())
        }
        Fetch::StateCell(acc, key) => {
            if state.is_offline() {
                let Some(Fetched::State(_, _, val)) = state.next_fetched() else {
                    return Err(eyre::eyre!("!").into());
                };
                state.init(&acc, &key, val);
            } else {
                let val = chain.get(&acc, &key).await?;
                state.init(&acc, &key, val);
                state.save_fetched(Fetched::State(acc, key, val));
            }
            Ok(())
        }
    }
}
