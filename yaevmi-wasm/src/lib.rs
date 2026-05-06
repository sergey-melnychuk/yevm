#[cfg(target_arch = "wasm32")] // comment-out this line for development
mod wasm {
    use eyre::eyre;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use js_sys::JsString;
    use wasm_bindgen::prelude::*;
    use yaevmi_base::int;
    use yaevmi_core::Int;
    use yaevmi_core::cache::Cache;
    use yaevmi_core::chain::Chain;
    use yaevmi_core::exe::{CallResult, Executor};
    use yaevmi_core::state::State;
    use yaevmi_core::trace::Trace;
    use yaevmi_core::{Call, Head, Tx, call::TxCall, rpc::Rpc};

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct Error(eyre::Report);

    impl From<eyre::Report> for Error {
        fn from(e: eyre::Report) -> Self {
            Self(e)
        }
    }

    impl From<yaevmi_core::Error> for Error {
        fn from(e: yaevmi_core::Error) -> Self {
            Self(e.into())
        }
    }

    impl From<Error> for JsValue {
        fn from(e: Error) -> Self {
            JsValue::from(JsError::new(&e.0.to_string()))
        }
    }

    #[wasm_bindgen]
    pub fn hello(name: JsString) -> JsString {
        JsString::from(format!("Hello, {name}").as_str())
    }

    #[wasm_bindgen]
    pub fn call(json: JsValue) -> Result<JsValue, JsError> {
        let call: Call = serde_wasm_bindgen::from_value(json)?;
        Ok(serde_wasm_bindgen::to_value(&call)?)
    }

    #[wasm_bindgen]
    pub struct Stream {
        receiver: mpsc::Receiver<Trace>,
        tx: Int,
        rcpt_gas: Int,
        yevm_gas: Int,
        rcpt_status: Int,
        yevm_status: Int,
    }

    #[wasm_bindgen]
    impl Stream {
        pub async fn next(&mut self) -> JsValue {
            release().await;
            let result = match self.receiver.next().await {
                Some(step) => {
                    let value = serde_wasm_bindgen::to_value(&step).unwrap_or(JsValue::NULL);
                    release().await;
                    value
                }
                None => JsValue::NULL,
            };
            release().await;
            result
        }

        pub fn check(&self) -> JsString {
            let gas_ok = self.yevm_gas == self.rcpt_gas;
            let status_ok = self.yevm_status == self.rcpt_status;
            [
                format!("TX: {}", self.tx),
                format!(
                    "gas: yevm={} rcpt={}: OK={}",
                    self.yevm_gas.as_u64(),
                    self.rcpt_gas.as_u64(),
                    gas_ok
                ),
                if self.rcpt_status > 2.into() {
                    format!(
                        "created: yevm={} rcpt={}: OK={}",
                        self.yevm_status.to::<20>(),
                        self.rcpt_status.to::<20>(),
                        status_ok
                    )
                } else {
                    format!(
                        "status: yevm={} rcpt={}: OK={}",
                        self.yevm_status.as_u8(),
                        self.rcpt_status.as_u8(),
                        status_ok
                    )
                },
            ]
            .join("\n")
            .into()
        }
    }

    #[wasm_bindgen]
    pub async fn pick(url: JsString) -> Result<JsString, Error> {
        let rpc = Rpc::latest(url.into()).await?;
        let txs = &rpc.block(rpc.block_number).await?.txs;
        let len = txs.len();
        let idx = (js_sys::Math::random() * len as f64) as usize;
        let hash = txs[idx].tx.hash.to_string();
        Ok(hash.into())
    }

    #[wasm_bindgen]
    pub async fn run(url: JsString, txn: JsString, event_filter: u32) -> Result<Stream, Error> {
        let mut rpc = Rpc::latest(url.into()).await?;
        let chain_id = rpc.chain_id().await?;
        let txn: String = txn.into();
        let (block, index) = if let Some((left, right)) = txn.split_once(':') {
            let block = left.trim().parse().map_err(|_| eyre!("invalid block"))?;
            let index = right.trim().parse().map_err(|_| eyre!("invalid index"))?;
            (block, index)
        } else if txn.starts_with("0x") {
            let hash = int(&txn);
            let receipt = rpc.receipt(hash).await?;
            let block = receipt.block_number.as_u64();
            let index = receipt.transaction_index.as_usize();
            (block, index)
        } else {
            return Err(eyre!("invalid tx selector").into());
        };

        let (call, tx, head): (Call, Tx, Head) = {
            let block = rpc.block(block).await?;
            let tx = &block.txs[index];
            let call = tx.call.clone().into();
            (call, tx.tx.clone(), block.head)
        };
        let hash = tx.hash;
        rpc.reset(head.number.as_u64() - 1, head.parent_hash);

        let (ytx, yrx) = mpsc::channel(1024 * 1024);
        let mut cache = Cache::with_sender(ytx, event_filter);
        cache.set_chain_id(chain_id);

        let mut exe = Executor::new(call);
        let result = exe.run(tx, head, &mut cache, &rpc).await?;
        let _ = cache.sender.take();

        let (yevm_status, yevm_gas) = match result {
            CallResult::Done {
                status,
                ret: _,
                gas,
            } => (status, gas.finalized.into()),
            CallResult::Created { acc, code: _, gas } => (acc.to(), gas.finalized.into()),
        };
        let receipt = rpc.receipt(hash).await?;
        let (rcpt_status, rcpt_gas) = (
            if let Some(acc) = receipt.contract_address {
                acc.to()
            } else {
                receipt.status
            },
            receipt.gas_used,
        );
        Ok(Stream {
            receiver: yrx,
            tx: hash,
            rcpt_gas,
            yevm_gas,
            rcpt_status,
            yevm_status,
        })
    }

    #[wasm_bindgen]
    pub async fn simulate(url: JsString, call_json: JsValue, event_filter: u32) -> Result<Stream, Error> {
        let mut rpc = Rpc::latest(url.into()).await?;
        let chain_id = rpc.chain_id().await?;
        let head = rpc.block(rpc.block_number).await?.head;
        rpc.reset(head.number.as_u64(), head.hash);

        let call: Call = serde_wasm_bindgen::from_value::<TxCall>(call_json)
            .map_err(|e| eyre!("{e}"))?.into();

        // Synthetic legacy tx: gas_price = base_fee satisfies the >= base_fee check.
        // chain_id left at zero so exe.rs fills it from cache.
        let tx = Tx {
            chain_id: Default::default(),
            nonce: Int::ZERO,
            gas_price: head.base_fee,
            max_fee_per_gas: Int::ZERO,
            max_priority_fee_per_gas: Int::ZERO,
            access_list: vec![],
            authorization_list: vec![],
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: None,
            hash: Int::ZERO,
            index: Int::ZERO,
        };

        let (ytx, yrx) = mpsc::channel(1024 * 1024);
        let mut cache = Cache::with_sender(ytx, event_filter);
        cache.set_chain_id(chain_id);

        let mut exe = Executor::new(call);
        let result = exe.run(tx, head, &mut cache, &rpc).await?;
        let _ = cache.sender.take();

        let (yevm_status, yevm_gas) = match result {
            CallResult::Done { status, ret: _, gas } => (status, gas.finalized.into()),
            CallResult::Created { acc, code: _, gas } => (acc.to(), gas.finalized.into()),
        };

        Ok(Stream {
            receiver: yrx,
            tx: Int::ZERO,
            rcpt_gas: yevm_gas,
            yevm_gas,
            rcpt_status: yevm_status,
            yevm_status,
        })
    }

    async fn release() {
        let promise = js_sys::Promise::resolve(&JsValue::NULL);
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;

/*
TODO:

Call target: execute call against network block at given height.
Call target: execute network tx (given block + index OR tx hash).
Call target: execute network block (given block number/hash).

Call target: bring your own env (bytecode, call, storage, etc).
(Fully reproducible hermetic execution environment, demo/PoC/etc)

In-memory (or remote server) cache of account state and storage.

Traces:
- trace filter (only collect certain event types)
- trace streaming (ease memory pressure, backpressure/pause)

Result:
- show affected accounts (balance, nonce) and storage slots
- show created accounts (if any)
- show emitted logs (if any)
- show gas usage per step/frame

Intelligence:
- resolve function selectors (4byte.directory)
- resolve function parameters (source code + ABI)
- for Solidity source code (matching bytecode + srcmap): per-line debugging
- resolve storage slots based on hash preimage (e.g. ERC-20 token balance for specific address)

---

Use Case: Anomaly Detection and Transaction Verification

Re-execute all previous transactions for the given address, collect results (state, value).
Re-execute target transaction, collect results (state, value) and compare to previous results.
If significant deviation is detected [TODO: define "significant"], flag and report it.

[This could have prevented "ByBit hack" of Feb'25 $1.5B worth of ETH being stolen].
https://www.chainalysis.com/blog/bybit-exchange-hack-february-2025-crypto-security-dprk/
https://www.cremit.io/blog/bybit-hacking-incident-analysis-how-to-strengthen-cryptocurrency-exchange-security

*/
