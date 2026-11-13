//! Ethereum node that executes blocks with yevm.
//!
//! This is the standard Ethereum node composed with [`YevmEvmConfig`] as its EVM configuration,
//! so block validation, payload building and `eth_call` style RPCs execute on yevm. See
//! [`yevm_reth::exec`] for the limitations, most importantly that only Osaka blocks can be executed:
//! on an existing chain, sync with `--snap.v2` so that only blocks after the snap target are
//! executed.
//!
//! silt is installed alongside (two builder calls): the "silt" ExEx publishes a state diff per
//! committed block on the feed (`silt.toml` `bind`, default 127.0.0.1:8585) and serves binary
//! state lookups and trie proofs (`lookup_bind`, default 127.0.0.1:8586); `silt_multiproof` is
//! merged into the RPC transports `--http`/`--ws`/`--ipc` enable. Config: `./silt.toml`,
//! defaults if absent.

use reth_ethereum::{
    EthPrimitives,
    chainspec::ChainSpec,
    cli::interface::Cli,
    node::{
        EthereumNode,
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, components::ExecutorBuilder},
        node::EthereumAddOns,
        snap::EthereumBackfill,
    },
};
use tracing::info;
use yevm_reth::exec::YevmEvmConfig;

/// Builds the yevm EVM configuration for an Ethereum node.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
struct YevmExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for YevmExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = YevmEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(YevmEvmConfig::new(ctx.chain_spec()))
    }
}

fn main() {
    if let Err(err) = Cli::parse_args().run(async move |builder, _| {
        info!(target: "reth::cli", "Launching node with the yevm executor");
        let backfill = EthereumBackfill::new(builder.config());
        let silt = silt_node::SiltConfig::load()?;
        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(YevmExecutorBuilder))
            .with_add_ons(EthereumAddOns::default())
            .install_exex("silt", async move |ctx| Ok(silt_node::run_exex(ctx, silt)))
            .extend_rpc_modules(|ctx| {
                ctx.modules
                    .merge_configured(silt_node::rpc_module(ctx.provider().clone()))?;
                Ok(())
            })
            .launch_with_debug_capabilities_and_backfill(backfill)
            .await?;

        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
