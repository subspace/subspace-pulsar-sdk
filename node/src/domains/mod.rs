//! Module for executor and its domains

use std::path::Path;
use std::sync::{Arc, Weak};

use anyhow::Context;
use cross_domain_message_gossip::GossipWorkerBuilder;
use derivative::Derivative;
use derive_builder::Builder;
use domain_runtime_primitives::opaque::Block;
use domain_runtime_primitives::AccountId;
use domain_service::DomainConfiguration;
use futures::prelude::*;
use sc_client_api::BlockchainEvents;
use sdk_substrate::{Base, BaseBuilder};
use serde::{Deserialize, Serialize};
use sp_domains::DomainId;
use system_domain_runtime::{Header, Runtime, RuntimeApi};
use tracing_futures::Instrument;

use super::{BlockNumber, Hash};

pub(crate) mod chain_spec;

/// System domain executor instance.
pub struct ExecutorDispatch;

impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
    // #[cfg(feature = "runtime-benchmarks")]
    // type ExtendHostFunctions = frame_benchmarking::benchmarking::HostFunctions;
    // #[cfg(not(feature = "runtime-benchmarks"))]
    type ExtendHostFunctions = ();

    fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
        system_domain_runtime::api::dispatch(method, data)
    }

    fn native_version() -> sc_executor::NativeVersion {
        system_domain_runtime::native_version()
    }
}

/// New block notification
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BlockHeader {
    /// Block hash
    pub hash: Hash,
    /// Block number
    pub number: BlockNumber,
    /// Parent block hash
    pub parent_hash: Hash,
    /// Block state root
    pub state_root: Hash,
    /// Extrinsics root
    pub extrinsics_root: Hash,
}

impl From<Header> for BlockHeader {
    fn from(header: Header) -> Self {
        let hash = header.hash();
        let Header { number, parent_hash, state_root, extrinsics_root, digest: _ } = header;
        Self { hash, number, parent_hash, state_root, extrinsics_root }
    }
}

/// Node builder
#[derive(Debug, Clone, Derivative, Builder, Deserialize, Serialize, PartialEq)]
#[derivative(Default)]
#[builder(pattern = "owned", build_fn(private, name = "_build"), name = "ConfigBuilder")]
#[non_exhaustive]
pub struct Config {
    /// Id of the relayer
    #[builder(setter(strip_option), default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub relayer_id: Option<AccountId>,
    #[doc(hidden)]
    #[builder(
        setter(into, strip_option),
        field(type = "BaseBuilder", build = "self.base.build()")
    )]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub base: Base,
}

sdk_utils::generate_builder!(Config);
sdk_substrate::derive_base!(@ sdk_substrate::Base => ConfigBuilder);

pub(crate) type FullClient = domain_service::FullClient<Block, RuntimeApi, ExecutorDispatch>;
pub(crate) type NewFull = domain_service::NewFullSystem<
    Arc<FullClient>,
    sc_executor::NativeElseWasmExecutor<ExecutorDispatch>,
    subspace_runtime_primitives::opaque::Block,
    crate::FullClient,
    RuntimeApi,
    ExecutorDispatch,
>;
/// Chain spec of the system domain
pub type ChainSpec = chain_spec::ChainSpec;

/// System domain node
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct SystemDomainNode {
    #[derivative(Debug = "ignore")]
    _client: Weak<FullClient>,
    rpc_handlers: sdk_utils::Rpc,
}

static_assertions::assert_impl_all!(SystemDomainNode: Send, Sync);

impl SystemDomainNode {
    pub(crate) async fn new(
        cfg: Config,
        directory: impl AsRef<Path>,
        chain_spec: ChainSpec,
        primary_new_full: &mut crate::NewFull,
    ) -> anyhow::Result<Self> {
        let Config { base, relayer_id: maybe_relayer_id } = cfg;
        let service_config =
            base.configuration(directory.as_ref().join("system"), chain_spec).await;

        let system_domain_config = DomainConfiguration { service_config, maybe_relayer_id };
        let block_importing_notification_stream = primary_new_full
            .block_importing_notification_stream
            .subscribe()
            .then(|block_importing_notification| async move {
                (
                    block_importing_notification.block_number,
                    block_importing_notification.acknowledgement_sender,
                )
            });

        let new_slot_notification_stream = primary_new_full
            .new_slot_notification_stream
            .subscribe()
            .then(|slot_notification| async move {
                (
                    slot_notification.new_slot_info.slot,
                    slot_notification.new_slot_info.global_challenge,
                    None,
                )
            });

        let mut xdm_gossip_worker_builder = GossipWorkerBuilder::new();

        // TODO: proper value
        let block_import_throttling_buffer_size = 10;

        let executor_streams = domain_client_executor::ExecutorStreams {
            primary_block_import_throttling_buffer_size: block_import_throttling_buffer_size,
            block_importing_notification_stream,
            imported_block_notification_stream: primary_new_full
                .client
                .every_import_notification_stream(),
            new_slot_notification_stream,
            _phantom: Default::default(),
        };

        let system_domain_node = domain_service::new_full_system(
            system_domain_config,
            primary_new_full.client.clone(),
            primary_new_full.sync_service.clone(),
            &primary_new_full.select_chain,
            executor_streams,
            xdm_gossip_worker_builder.gossip_msg_sink(),
        )
        .await?;

        xdm_gossip_worker_builder
            .push_domain_tx_pool_sink(DomainId::SYSTEM, system_domain_node.tx_pool_sink);
        primary_new_full.task_manager.add_child(system_domain_node.task_manager);

        let cross_domain_message_gossip_worker = xdm_gossip_worker_builder.build::<Block, _, _>(
            primary_new_full.network_service.clone(),
            primary_new_full.sync_service.clone(),
        );

        let NewFull { client, network_starter, rpc_handlers, .. } = system_domain_node;

        tokio::spawn(cross_domain_message_gossip_worker.run().in_current_span());
        network_starter.start_network();

        Ok(Self {
            _client: Arc::downgrade(&client),
            rpc_handlers: sdk_utils::Rpc::new(&rpc_handlers),
        })
    }

    pub(crate) fn _client(&self) -> anyhow::Result<Arc<FullClient>> {
        self._client.upgrade().ok_or_else(|| anyhow::anyhow!("The node was already closed"))
    }

    /// Subscribe to new blocks imported
    pub async fn subscribe_new_heads(
        &self,
    ) -> anyhow::Result<impl Stream<Item = BlockHeader> + Send + Sync + Unpin + 'static> {
        Ok(self
            .rpc_handlers
            .subscribe_new_heads::<Runtime>()
            .await
            .context("Failed to subscribe to new blocks")?
            .map(Into::into))
    }

    /// Subscribe to finalized blocks
    pub async fn subscribe_finalized_heads(
        &self,
    ) -> anyhow::Result<impl Stream<Item = BlockHeader> + Send + Sync + Unpin + 'static> {
        Ok(self
            .rpc_handlers
            .subscribe_finalized_heads::<Runtime>()
            .await
            .context("Failed to subscribe to finalized blocks")?
            .map(Into::into))
    }
}
