use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use yaevmi_base::{Acc, Int};
use yaevmi_misc::{buf::Buf, http::Http};

use crate::{
    call::{Block, Head, Receipt, TxFull},
    chain::Chain,
    state::Account,
};

pub struct Rpc {
    url: String,
    http: Http,
    pub block_number: u64,
    pub block_hash: Int,
}

impl Rpc {
    pub async fn latest(url: String) -> eyre::Result<Self> {
        let http = Http::new();
        let head = head(&http, &url, "latest".into()).await?;
        Ok(Self {
            url,
            http,
            block_number: head.number.as_u64(),
            block_hash: head.hash,
        })
    }

    pub async fn number(url: String, number: u64) -> eyre::Result<Self> {
        let http = Http::new();
        let head = head(&http, &url, format!("0x{:x}", number)).await?;
        Ok(Self {
            url,
            http,
            block_number: head.number.as_u64(),
            block_hash: head.hash,
        })
    }

    pub fn offline() -> Self {
        Self {
            url: "".to_string(),
            http: Http::new(),
            block_number: 0,
            block_hash: Int::zero(),
        }
    }

    pub fn reset(&mut self, number: u64, hash: Int) {
        self.block_number = number;
        self.block_hash = hash;
    }

    pub async fn chain_id(&self) -> eyre::Result<u64> {
        let chain_id: Int = call(&self.http, &self.url, "eth_chainId", &[]).await?;
        Ok(chain_id.as_u64())
    }

    pub async fn lookup(&self, block: u64, index: u64) -> eyre::Result<TxFull> {
        let tx = call::<TxFull>(
            &self.http,
            &self.url,
            "eth_getTransactionByBlockNumberAndIndex",
            &[
                Value::String(format!("0x{block:x}")),
                Value::String(format!("0x{index:x}")),
            ],
        )
        .await?;
        Ok(tx)
    }

    pub async fn receipt(&self, hash: Int) -> eyre::Result<Receipt> {
        receipt(&self.http, &self.url, hash).await
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Chain for Rpc {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int> {
        let value = call(
            &self.http,
            &self.url,
            "eth_getStorageAt",
            &[
                Value::String(acc.to_string()),
                Value::String(key.to_string()),
                Value::String(self.block_hash.to_string()),
            ],
        )
        .await?;
        Ok(value)
    }

    async fn acc(&self, acc: &Acc) -> eyre::Result<Account> {
        // TODO: consider firing sub-calls concurrently to speed this up
        Ok(Account {
            value: self.balance(acc).await?,
            nonce: self.nonce(acc).await?.into(),
            code: self.code(acc).await?,
        })
    }

    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)> {
        let code: Buf = call(
            &self.http,
            &self.url,
            "eth_getCode",
            &[
                Value::String(acc.to_string()),
                Value::String(self.block_hash.to_string()),
            ],
        )
        .await?;
        let hash = yaevmi_misc::keccak256(code.as_slice());
        Ok((code, hash))
    }

    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64> {
        let nonce: Int = call(
            &self.http,
            &self.url,
            "eth_getTransactionCount",
            &[
                Value::String(acc.to_string()),
                Value::String(self.block_hash.to_string()),
            ],
        )
        .await?;
        Ok(nonce.as_u64())
    }

    async fn balance(&self, acc: &Acc) -> eyre::Result<Int> {
        let balance = call(
            &self.http,
            &self.url,
            "eth_getBalance",
            &[
                Value::String(acc.to_string()),
                Value::String(self.block_hash.to_string()),
            ],
        )
        .await?;
        Ok(balance)
    }

    async fn head(&self, number: u64) -> eyre::Result<Head> {
        let head = call(
            &self.http,
            &self.url,
            "eth_getBlockByNumber",
            &[Value::String(format!("0x{:x}", number)), Value::Bool(false)],
        )
        .await?;
        Ok(head)
    }

    async fn block(&self, number: u64) -> eyre::Result<Block> {
        let block = call(
            &self.http,
            &self.url,
            "eth_getBlockByNumber",
            &[Value::String(format!("0x{:x}", number)), Value::Bool(true)],
        )
        .await?;
        Ok(block)
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        let chain_id: Int = call(&self.http, &self.url, "eth_chainId", &[]).await?;
        Ok(chain_id.as_u64())
    }
}

async fn head(http: &Http, url: &str, arg: String) -> eyre::Result<Head> {
    let head = call(
        http,
        url,
        "eth_getBlockByNumber",
        &[Value::String(arg), Value::Bool(false)],
    )
    .await?;
    Ok(head)
}

async fn receipt(http: &Http, url: &str, hash: Int) -> eyre::Result<Receipt> {
    let head = call(
        http,
        url,
        "eth_getTransactionReceipt",
        &[Value::String(hash.to_string())],
    )
    .await?;
    Ok(head)
}

async fn call<R: DeserializeOwned>(
    http: &Http,
    url: &str,
    method: &str,
    params: &[Value],
) -> eyre::Result<R> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    });
    let json: Value = http.post(url, &body).await?;
    if std::env::var("DEBUG").is_ok() {
        // Just for debugging until proper logging is implemented
        let body = serde_json::to_string_pretty(&body).unwrap();
        let json = serde_json::to_string_pretty(&json).unwrap();
        println!("{} -> {}", body, json);
    }
    if let Some(error) = json.get("error") {
        if let Some(message) = error.as_str() {
            eyre::bail!(message.to_owned());
        }
        if let Some(message) = error.get("message") {
            eyre::bail!(message.to_string());
        }
        let message = serde_json::to_string(error)?;
        eyre::bail!(message);
    }
    if let Some(result) = json.get("result") {
        if result.is_null() {
            eyre::bail!("result is null");
        } else {
            let ret = serde_json::from_value::<R>(result.to_owned())?;
            return Ok(ret);
        }
    }
    eprint!("JSON: {:#?}", json);
    eyre::bail!("missing: result & error")
}

#[cfg(test)]
mod tests {
    use yaevmi_base::int;

    use super::*;

    #[tokio::test]
    #[ignore = "makes RPC call to a public node"]
    async fn test_latest() -> eyre::Result<()> {
        let http = Http::new();
        let head = head(
            &http,
            "https://ethereum-rpc.publicnode.com",
            "latest".to_string(),
        )
        .await?;
        let (number, hash) = (head.number.as_u64(), head.hash);
        assert_ne!(hash, Int::ZERO);
        assert!(number > 24697386);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "makes RPC call to a public node"]
    async fn test_receipt() -> eyre::Result<()> {
        let hash = int("0xd3aafbde18d85a863399c94ffec80af928bb3ebecc3685f1c784245deff04c04");
        let http = Http::new();
        let _ = receipt(&http, "https://ethereum-rpc.publicnode.com", hash).await?;
        Ok(())
    }
}
