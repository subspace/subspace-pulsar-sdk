use std::num::NonZeroUsize;
use std::path::Path;

use derivative::Derivative;
use derive_builder::Builder;
use derive_more::{Deref, DerefMut, Display, From};
use sc_service::BlocksPruning;
use sdk_dsn::{Dsn, DsnBuilder};
use sdk_substrate::{
    Base, BaseBuilder, ExecutionStrategy, NetworkBuilder, OffchainWorkerBuilder, PruningMode, Role,
    RpcBuilder, StorageMonitor,
};
use sdk_utils::ByteSize;
use serde::{Deserialize, Serialize};

use super::{ChainSpec, Farmer, Node};
use crate::domains::builder::DomainConfig;
use crate::PotConfiguration;

/// Wrapper with default value for piece cache size
#[derive(
    Debug, Clone, Derivative, Deserialize, Serialize, PartialEq, Eq, From, Deref, DerefMut, Display,
)]
#[derivative(Default)]
#[serde(transparent)]
/// Size of cache of pieces that node produces
/// TODO: Set it to 1 GB once DSN is fixed
pub struct PieceCacheSize(#[derivative(Default(value = "ByteSize::gib(3)"))] pub(crate) ByteSize);

/// Wrapper with default value for segment publish concurrent jobs
#[derive(
    Debug, Clone, Derivative, Deserialize, Serialize, PartialEq, Eq, From, Deref, DerefMut, Display,
)]
#[derivative(Default)]
#[serde(transparent)]
pub struct SegmentPublishConcurrency(
    #[derivative(Default(value = "NonZeroUsize::new(10).expect(\"10 > 0\")"))]
    pub(crate)  NonZeroUsize,
);

/// Node builder
#[derive(Debug, Clone, Derivative, Builder, Deserialize, Serialize, PartialEq)]
#[derivative(Default(bound = ""))]
#[builder(pattern = "owned", build_fn(private, name = "_build"), name = "Builder")]
#[non_exhaustive]
pub struct Config<F: Farmer> {
    /// Max number of segments that can be published concurrently, impacts
    /// RAM usage and network bandwidth.
    #[builder(setter(into), default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub segment_publish_concurrency: SegmentPublishConcurrency,
    /// Should we sync blocks from the DSN?
    #[builder(default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub sync_from_dsn: bool,
    #[doc(hidden)]
    #[builder(
        setter(into, strip_option),
        field(type = "BaseBuilder", build = "self.base.build()")
    )]
    #[serde(flatten, skip_serializing_if = "sdk_utils::is_default")]
    pub base: Base,
    /// DSN settings
    #[builder(setter(into), default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub dsn: Dsn,
    /// Storage monitor settings
    #[builder(setter(into), default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub storage_monitor: Option<StorageMonitor>,
    /// Enables subspace block relayer
    #[builder(default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub enable_subspace_block_relay: bool,
    #[builder(setter(skip), default)]
    #[serde(skip, default)]
    _farmer: std::marker::PhantomData<F>,
    /// Optional domain configuration
    #[builder(setter(into), default)]
    #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
    pub domain: Option<DomainConfig>,
}

impl<F: Farmer + 'static> Config<F> {
    /// Dev configuraiton
    pub fn dev() -> Builder<F> {
        Builder::dev()
    }

    /// Gemini 3f configuraiton
    pub fn gemini_3f() -> Builder<F> {
        Builder::gemini_3f()
    }

    /// Devnet configuraiton
    pub fn devnet() -> Builder<F> {
        Builder::devnet()
    }
}

impl<F: Farmer + 'static> Builder<F> {
    /// Dev chain configuration
    pub fn dev() -> Self {
        Self::new()
            .force_authoring(true)
            .network(NetworkBuilder::dev())
            .dsn(DsnBuilder::dev())
            .rpc(RpcBuilder::dev())
            .offchain_worker(OffchainWorkerBuilder::dev())
    }

    /// Gemini 3f configuration
    pub fn gemini_3f() -> Self {
        Self::new()
            .execution_strategy(ExecutionStrategy::AlwaysWasm)
            .network(NetworkBuilder::gemini_3f())
            .dsn(DsnBuilder::gemini_3f())
            .rpc(RpcBuilder::gemini_3f())
            .offchain_worker(OffchainWorkerBuilder::gemini_3f())
            .role(Role::Authority)
            .state_pruning(PruningMode::ArchiveAll)
            .blocks_pruning(BlocksPruning::Some(256))
    }

    /// Devnet chain configuration
    pub fn devnet() -> Self {
        Self::new()
            .execution_strategy(ExecutionStrategy::AlwaysWasm)
            .network(NetworkBuilder::devnet())
            .dsn(DsnBuilder::devnet())
            .rpc(RpcBuilder::devnet())
            .offchain_worker(OffchainWorkerBuilder::devnet())
            .role(Role::Authority)
            .state_pruning(PruningMode::ArchiveAll)
            .blocks_pruning(BlocksPruning::Some(256))
    }

    /// Get configuration for saving on disk
    pub fn configuration(self) -> Config<F> {
        self._build().expect("Build is infallible")
    }

    /// New builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a node with supplied parameters
    pub async fn build(
        self,
        directory: impl AsRef<Path>,
        chain_spec: ChainSpec,
        pot_configuration: PotConfiguration,
    ) -> anyhow::Result<Node<F>> {
        self.configuration().build(directory, chain_spec, pot_configuration).await
    }
}

sdk_substrate::derive_base!(<F: Farmer + 'static> @ Base => Builder);
