use std::sync::Arc;

use cross_domain_message_gossip::GossipWorkerBuilder;
use domain_client_operator::OperatorStreams;
use domain_eth_service::provider::EthProvider;
use domain_eth_service::DefaultEthConfig;
use domain_runtime_primitives::opaque::Block as DomainBlock;
use domain_service::{DomainConfiguration, FullBackend, FullClient};
use futures::StreamExt;
use sc_client_api::ImportNotifications;
use sc_consensus_subspace::notification::SubspaceNotificationStream;
use sc_consensus_subspace::{BlockImportingNotification, NewSlotNotification};
use sc_service::{BasePath, Configuration, RpcHandlers};
use sp_core::crypto::AccountId32;
use sp_core::traits::SpawnEssentialNamed;
use sp_domains::{DomainId, RuntimeType};
use sp_runtime::traits::{Block as BlockT, Convert, NumberFor};
use subspace_runtime::RuntimeApi as CRuntimeApi;
use subspace_runtime_primitives::opaque::Block as CBlock;
use subspace_service::{FullClient as CFullClient, FullSelectChain};
use tokio::task::JoinHandle;

use crate::domains::evm_domain_executor_dispatch::EVMDomainExecutorDispatch;
use crate::domains::utils::{AccountId20, AccountId32ToAccountId20Converter};
use crate::ExecutorDispatch as CExecutorDispatch;

/// `DomainInstanceStarter` used to start a domain instance node based on the
/// given bootstrap result
pub struct DomainInstanceStarter {
    pub service_config: Configuration,
    pub domain_id: DomainId,
    pub relayer_id: AccountId32,
    pub runtime_type: RuntimeType,
    pub additional_arguments: Vec<String>,
    pub consensus_client: Arc<CFullClient<CRuntimeApi, CExecutorDispatch>>,
    pub block_importing_notification_stream:
        SubspaceNotificationStream<BlockImportingNotification<CBlock>>,
    pub new_slot_notification_stream: SubspaceNotificationStream<NewSlotNotification>,
    pub consensus_network_service:
        Arc<sc_network::NetworkService<CBlock, <CBlock as BlockT>::Hash>>,
    pub consensus_sync_service: Arc<sc_network_sync::SyncingService<CBlock>>,
    pub select_chain: FullSelectChain,
}

impl DomainInstanceStarter {
    pub async fn prepare_for_start(
        self,
        domain_created_at: NumberFor<CBlock>,
        imported_block_notification_stream: ImportNotifications<CBlock>,
    ) -> anyhow::Result<(RpcHandlers, JoinHandle<anyhow::Result<()>>)> {
        let DomainInstanceStarter {
            domain_id,
            runtime_type,
            relayer_id,
            mut additional_arguments,
            service_config,
            consensus_client,
            block_importing_notification_stream,
            new_slot_notification_stream,
            consensus_network_service,
            consensus_sync_service,
            select_chain,
        } = self;

        let domain_config = DomainConfiguration {
            service_config,
            maybe_relayer_id: Some(AccountId32ToAccountId20Converter::convert(relayer_id)),
        };

        let block_importing_notification_stream = || {
            block_importing_notification_stream.subscribe().then(
                |block_importing_notification| async move {
                    (
                        block_importing_notification.block_number,
                        block_importing_notification.acknowledgement_sender,
                    )
                },
            )
        };

        let new_slot_notification_stream = || {
            new_slot_notification_stream.subscribe().then(|slot_notification| async move {
                (
                    slot_notification.new_slot_info.slot,
                    slot_notification.new_slot_info.global_randomness,
                    None::<futures::channel::mpsc::Sender<()>>,
                )
            })
        };

        let operator_streams = OperatorStreams {
            // TODO: proper value
            consensus_block_import_throttling_buffer_size: 10,
            block_importing_notification_stream: block_importing_notification_stream(),
            imported_block_notification_stream,
            new_slot_notification_stream: new_slot_notification_stream(),
            _phantom: Default::default(),
        };

        match runtime_type {
            RuntimeType::Evm => {
                let mut xdm_gossip_worker_builder = GossipWorkerBuilder::new();

                let eth_provider = EthProvider::<
                    evm_domain_runtime::TransactionConverter,
                    DefaultEthConfig<
                        FullClient<
                            DomainBlock,
                            evm_domain_runtime::RuntimeApi,
                            EVMDomainExecutorDispatch,
                        >,
                        FullBackend<DomainBlock>,
                    >,
                >::new(
                    Some(BasePath::new(domain_config.service_config.base_path.path())),
                    additional_arguments.drain(..),
                );

                let domain_params = domain_service::DomainParams {
                    domain_id,
                    domain_config,
                    domain_created_at,
                    consensus_client,
                    consensus_network_sync_oracle: consensus_sync_service.clone(),
                    select_chain,
                    operator_streams,
                    gossip_message_sink: xdm_gossip_worker_builder.gossip_msg_sink(),
                    provider: eth_provider,
                };

                let mut domain_node = domain_service::new_full::<
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    evm_domain_runtime::RuntimeApi,
                    EVMDomainExecutorDispatch,
                    AccountId20,
                    _,
                >(domain_params)
                .await
                .map_err(anyhow::Error::new)?;

                xdm_gossip_worker_builder
                    .push_domain_tx_pool_sink(domain_id, domain_node.tx_pool_sink);

                let cross_domain_message_gossip_worker = xdm_gossip_worker_builder
                    .build::<CBlock, _, _>(consensus_network_service, consensus_sync_service);

                domain_node.task_manager.spawn_essential_handle().spawn_essential_blocking(
                    "cross-domain-gossip-message-worker",
                    None,
                    Box::pin(cross_domain_message_gossip_worker.run()),
                );

                let domain_start_join_handle = sdk_utils::task_spawn(
                    format!("domain-{}/start-domain", <DomainId as Into<u32>>::into(domain_id)),
                    async move {
                        domain_node.network_starter.start_network();
                        domain_node.task_manager.future().await.map_err(anyhow::Error::new)
                    },
                );

                Ok((domain_node.rpc_handlers.clone(), domain_start_join_handle))
            }
        }
    }
}
