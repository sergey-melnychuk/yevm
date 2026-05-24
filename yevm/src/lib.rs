//! YEVM — async, WASM-native EVM in Rust.
//!
//! Top-level integration crate. Re-exports the primitive types and the most
//! commonly used items from [`base`], [`core`], [`lens`], and [`misc`] so a
//! consumer can depend on a single crate instead of wiring the sub-crates by
//! hand.
//!
//! # Example
//!
//! Simulate a transaction against a JSON-RPC node, stream the execution
//! trace, and decode the side effects with [`analyse`]:
//!
//! ```ignore
//! use futures::StreamExt;
//! use yevm::{analyse, trace::filter, Cache, Call, Executor, Rpc, Trace, Tx};
//!
//! async fn simulate(call: Call, tx: Tx, rpc: &str) -> eyre::Result<()> {
//!     let mut rpc = Rpc::latest(rpc.into()).await?;
//!     let chain_id = rpc.chain_id().await?;
//!     let head = rpc.block(rpc.block_number).await?.head;
//!     rpc.reset(head.number.as_u64(), head.hash);
//!
//!     let (sender, receiver) = futures::channel::mpsc::channel(1 << 20);
//!     let mut cache = Cache::with_sender(
//!         sender,
//!         filter::MOVE | filter::PUT | filter::FEE | filter::LOG | filter::CREATE,
//!     );
//!     cache.set_chain_id(chain_id);
//!
//!     let executor = Executor::new(call);
//!     executor.run(tx, head, &mut cache, &rpc).await?;
//!
//!     let traces: Vec<Trace> = receiver.collect().await;
//!     let alerts = analyse(&traces);
//!     for swap in &alerts.proxy_swaps {
//!         eprintln!("proxy swap detected: {swap:?}");
//!     }
//!     Ok(())
//! }
//! ```

pub use yevm_base as base;
pub use yevm_core as core;
pub use yevm_lens as lens;
pub use yevm_misc as misc;

#[cfg(target_arch = "wasm32")]
pub use yevm_wasm as wasm;

pub use yevm_base::{Acc, Int};
pub use yevm_misc::buf::Buf;

pub use yevm_core::cache::Cache;
pub use yevm_core::chain::Chain;
pub use yevm_core::exe::{CallResult, Executor};
pub use yevm_core::rpc::Rpc;
pub use yevm_core::state::State;
pub use yevm_core::trace::{self, Event, Target, Trace};
pub use yevm_core::{Call, Head, Tx};

pub use yevm_lens::{Alerts, analyse};
