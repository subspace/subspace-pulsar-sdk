//! This crate is related to abstract farmer implementation

#![warn(
    missing_docs,
    clippy::dbg_macro,
    clippy::unwrap_used,
    clippy::disallowed_types,
    unused_features
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![feature(const_option)]

use std::collections::HashMap;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
pub use builder::{Builder, Config};
use derivative::Derivative;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use sdk_dsn::{FarmerPieceCache, FarmerProviderStorage};
use sdk_traits::Node;
use sdk_utils::{AsyncDropFutures, ByteSize, DropCollection, PublicKey};
use serde::{Deserialize, Serialize};
use subspace_core_primitives::crypto::kzg;
use subspace_core_primitives::{
    ArchivedHistorySegment, PieceIndex, PieceIndexHash, Record, SectorIndex, SegmentIndex,
};
use subspace_erasure_coding::ErasureCoding;
use subspace_farmer::single_disk_plot::{
    SingleDiskPlot, SingleDiskPlotError, SingleDiskPlotId, SingleDiskPlotInfo,
    SingleDiskPlotOptions, SingleDiskPlotSummary,
};
use subspace_farmer::utils::archival_storage_pieces::ArchivalStoragePieces;
use subspace_farmer::utils::farmer_piece_getter::FarmerPieceGetter;
use subspace_farmer::utils::parity_db_store::ParityDbStore;
use subspace_farmer::utils::piece_cache::PieceCache;
use subspace_farmer::utils::piece_validator::SegmentCommitmentPieceValidator;
use subspace_farmer::utils::readers_and_pieces::ReadersAndPieces;
use subspace_farmer_components::plotting::{PieceGetter, PieceGetterRetryPolicy, PlottedSector};
use subspace_networking::utils::multihash::ToMultihash;
use subspace_networking::utils::piece_provider::PieceValidator;
use subspace_rpc_primitives::{FarmerAppInfo, SolutionResponse};
use tokio::sync::{oneshot, watch, Mutex};
use tracing::{debug, error, trace, warn};
use tracing_futures::Instrument;

use self::builder::PieceCacheSize;

/// Description of the cache
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct CacheDescription {
    /// Path to the cache description
    pub directory: PathBuf,
    /// Space which you want to dedicate
    pub space_dedicated: ByteSize,
}

/// Error type for cache description constructor
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("Cache should be larger than {}", CacheDescription::MIN_SIZE)]
pub struct CacheTooSmall;

impl CacheDescription {
    /// Minimal cache size
    pub const MIN_SIZE: ByteSize = ByteSize::mib(1);

    /// Construct cache description
    pub fn new(
        directory: impl Into<PathBuf>,
        space_dedicated: ByteSize,
    ) -> Result<Self, CacheTooSmall> {
        if space_dedicated < Self::MIN_SIZE {
            return Err(CacheTooSmall);
        }
        Ok(Self { directory: directory.into(), space_dedicated })
    }

    /// Creates minimal cache description
    pub fn minimal(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into(), space_dedicated: Self::MIN_SIZE }
    }

    /// Wipe all the data from the plot
    pub async fn wipe(self) -> io::Result<()> {
        tokio::fs::remove_dir_all(self.directory).await
    }
}

/// Description of the plot
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct PlotDescription {
    /// Path of the plot
    pub directory: PathBuf,
    /// Space which you want to pledge
    pub space_pledged: ByteSize,
}

impl PlotDescription {
    /// Construct Plot description
    pub fn new(directory: impl Into<PathBuf>, space_pledged: ByteSize) -> Self {
        Self { directory: directory.into(), space_pledged }
    }

    /// Wipe all the data from the plot
    pub async fn wipe(self) -> io::Result<()> {
        tokio::fs::remove_dir_all(self.directory).await
    }
}

mod builder {
    use std::num::NonZeroUsize;

    use derivative::Derivative;
    use derive_builder::Builder;
    use derive_more::{Deref, DerefMut, Display, From};
    use sdk_traits::Node;
    use sdk_utils::{ByteSize, PublicKey};
    use serde::{Deserialize, Serialize};

    use super::{BuildError, CacheDescription};
    use crate::{Farmer, PlotDescription};

    #[derive(
        Debug,
        Clone,
        Derivative,
        Deserialize,
        Serialize,
        PartialEq,
        Eq,
        From,
        Deref,
        DerefMut,
        Display,
    )]
    #[derivative(Default)]
    #[serde(transparent)]
    pub struct MaxConcurrentPlots(
        #[derivative(Default(value = "NonZeroUsize::new(10).expect(\"10 > 0\")"))]
        pub(crate)  NonZeroUsize,
    );

    #[derive(
        Debug,
        Clone,
        Derivative,
        Deserialize,
        Serialize,
        PartialEq,
        Eq,
        From,
        Deref,
        DerefMut,
        Display,
    )]
    #[derivative(Default)]
    #[serde(transparent)]
    pub struct PieceCacheSize(
        #[derivative(Default(value = "ByteSize::mib(10)"))] pub(crate) ByteSize,
    );

    #[derive(
        Debug,
        Clone,
        Derivative,
        Deserialize,
        Serialize,
        PartialEq,
        Eq,
        From,
        Deref,
        DerefMut,
        Display,
    )]
    #[derivative(Default)]
    #[serde(transparent)]
    pub struct ProvidedKeysLimit(
        #[derivative(Default(value = "NonZeroUsize::new(655360).expect(\"655360 > 0\")"))]
        pub(crate) NonZeroUsize,
    );

    /// Technical type which stores all
    #[derive(Debug, Clone, Derivative, Builder, Serialize, Deserialize)]
    #[derivative(Default)]
    #[builder(pattern = "immutable", build_fn(private, name = "_build"), name = "Builder")]
    #[non_exhaustive]
    pub struct Config {
        /// Number of plots that can be plotted concurrently, impacts RAM usage.
        #[builder(default, setter(into))]
        #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
        pub max_concurrent_plots: MaxConcurrentPlots,
        /// Number of plots that can be plotted concurrently, impacts RAM usage.
        #[builder(default, setter(into))]
        #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
        pub piece_cache_size: PieceCacheSize,
        /// Number of plots that can be plotted concurrently, impacts RAM usage.
        #[builder(default, setter(into))]
        #[serde(default, skip_serializing_if = "sdk_utils::is_default")]
        pub provided_keys_limit: ProvidedKeysLimit,
        /// Maximum number of pieces in single sector
        #[builder(default)]
        pub max_pieces_in_sector: Option<u16>,
    }

    impl Builder {
        /// Get configuration for saving on disk
        pub fn configuration(&self) -> Config {
            self._build().expect("Build is infallible")
        }

        /// Open and start farmer
        pub async fn build<N: Node>(
            self,
            reward_address: PublicKey,
            node: &N,
            plots: &[PlotDescription],
            cache: CacheDescription,
        ) -> Result<Farmer<N::Table>, BuildError> {
            self.configuration().build(reward_address, node, plots, cache).await
        }
    }
}

/// Build Error
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// Failed to create single disk plot
    #[error("Single disk plot creation error: {0}")]
    SingleDiskPlotCreate(#[from] SingleDiskPlotError),
    /// No plots were supplied during building
    #[error("Supply at least one plot")]
    NoPlotsSupplied,
    /// Failed to fetch data from the node
    #[error("Failed to fetch data from node: {0}")]
    RPCError(#[source] subspace_farmer::RpcClientError),
    /// Other error
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
impl<T: subspace_proof_of_space::Table> sdk_traits::Farmer for Farmer<T> {
    type Table = T;

    async fn get_piece_by_hash(
        piece_index_hash: PieceIndexHash,
        piece_store: &sdk_dsn::PieceStore,
        weak_readers_and_pieces: &std::sync::Weak<parking_lot::Mutex<Option<ReadersAndPieces>>>,
    ) -> Option<subspace_core_primitives::Piece> {
        use tracing::debug;

        if let Some(piece) = piece_store.get(&piece_index_hash.to_multihash().into()) {
            return Some(piece);
        }

        let weak_readers_and_pieces = weak_readers_and_pieces.clone();

        debug!(?piece_index_hash, "No piece in the cache. Trying archival storage...");

        let readers_and_pieces = match weak_readers_and_pieces.upgrade() {
            Some(readers_and_pieces) => readers_and_pieces,
            None => {
                debug!("A readers and pieces are already dropped");
                return None;
            }
        };
        let read_piece = match readers_and_pieces.lock().as_ref() {
            Some(readers_and_pieces) => readers_and_pieces.read_piece(&piece_index_hash),
            None => {
                debug!(?piece_index_hash, "Readers and pieces are not initialized yet");
                return None;
            }
        };

        match read_piece {
            Some(fut) => fut.in_current_span().await,
            None => None,
        }
    }
}

const SEGMENT_COMMITMENTS_CACHE_SIZE: NonZeroUsize =
    NonZeroUsize::new(1_000_000).expect("Not zero; qed");

fn create_readers_and_pieces(
    single_disk_plots: &[SingleDiskPlot],
    archival_storage_pieces: ArchivalStoragePieces,
) -> Result<ReadersAndPieces, BuildError> {
    // Store piece readers so we can reference them later
    let readers = single_disk_plots.iter().map(SingleDiskPlot::piece_reader).collect();
    let mut readers_and_pieces = ReadersAndPieces::new(readers, archival_storage_pieces);

    tracing::debug!("Collecting already plotted pieces");

    // Collect already plotted pieces
    single_disk_plots.iter().enumerate().try_for_each(|(disk_farm_index, single_disk_plot)| {
        let disk_farm_index = disk_farm_index.try_into().map_err(|_error| {
            anyhow!(
                "More than 256 plots are not supported, consider running multiple farmer instances"
            )
        })?;

        (0 as SectorIndex..).zip(single_disk_plot.plotted_sectors()).for_each(
            |(sector_index, plotted_sector_result)| match plotted_sector_result {
                Ok(plotted_sector) => {
                    readers_and_pieces.add_sector(disk_farm_index, &plotted_sector);
                }
                Err(error) => {
                    error!(
                        %error,
                        %disk_farm_index,
                        %sector_index,
                        "Failed reading plotted sector on startup, skipping"
                    );
                }
            },
        );

        Ok::<_, anyhow::Error>(())
    })?;
    tracing::debug!("Finished collecting already plotted pieces");

    Ok(readers_and_pieces)
}

#[allow(clippy::too_many_arguments)]
fn handler_on_sector_plotted(
    plotted_sector: &PlottedSector,
    maybe_old_plotted_sector: &Option<PlottedSector>,
    disk_farm_index: usize,
    readers_and_pieces: Arc<parking_lot::Mutex<Option<ReadersAndPieces>>>,
) {
    let disk_farm_index = disk_farm_index
        .try_into()
        .expect("More than 256 plots are not supported, this is checked above already; qed");

    {
        let mut readers_and_pieces = readers_and_pieces.lock();
        let readers_and_pieces =
            readers_and_pieces.as_mut().expect("Initial value was populated before; qed");

        if let Some(old_plotted_sector) = maybe_old_plotted_sector {
            readers_and_pieces.delete_sector(disk_farm_index, old_plotted_sector);
        }
        readers_and_pieces.add_sector(disk_farm_index, plotted_sector);
    }
}

impl Config {
    /// Open and start farmer
    pub async fn build<N: Node, T: subspace_proof_of_space::Table>(
        self,
        reward_address: PublicKey,
        node: &N,
        plots: &[PlotDescription],
        cache: CacheDescription,
    ) -> Result<Farmer<T>, BuildError> {
        if plots.is_empty() {
            return Err(BuildError::NoPlotsSupplied);
        }

        let Self {
            max_concurrent_plots: _,
            piece_cache_size: PieceCacheSize(piece_cache_size),
            provided_keys_limit: _,
            max_pieces_in_sector,
        } = self;

        let piece_cache_size = NonZeroUsize::new(
            piece_cache_size.as_u64() as usize / subspace_core_primitives::Piece::SIZE,
        )
        .ok_or_else(|| anyhow::anyhow!("Piece cache size shouldn't be zero"))?;

        let mut single_disk_plots = Vec::with_capacity(plots.len());
        let mut plot_info = HashMap::with_capacity(plots.len());

        let base_path = cache.directory;
        let readers_and_pieces = Arc::clone(&node.dsn().farmer_readers_and_pieces);

        let node_name = node.name().to_owned();

        let piece_cache_db_path = base_path.join("piece_cache_db");

        let (piece_store, piece_cache, farmer_provider_storage) = {
            let peer_id = node.dsn().node.id();

            tracing::info!(
                db_path = ?piece_cache_db_path,
                size = ?piece_cache_size,
                "Initializing piece cache..."
            );

            let piece_store = ParityDbStore::new(&piece_cache_db_path)
                .context("Failed to create parity db piece store")?;

            let piece_cache = FarmerPieceCache::new(piece_store.clone(), piece_cache_size, peer_id);

            tracing::info!(
                current_size = ?piece_cache.size(),
                "Piece cache initialized successfully"
            );
            let farmer_provider_storage = FarmerProviderStorage::new(peer_id, piece_cache.clone());

            (piece_store, Arc::new(Mutex::new(piece_cache)), farmer_provider_storage)
        };

        let mut drop_at_exit = DropCollection::new();
        let kzg = kzg::Kzg::new(kzg::embedded_kzg_settings());
        let erasure_coding = ErasureCoding::new(
            NonZeroUsize::new(Record::NUM_S_BUCKETS.next_power_of_two().ilog2() as usize).expect(
                "Number of buckets >= 1, therefore next power of 2 >= 2, therefore ilog2 >= 1",
            ),
        )
        .map_err(|error| anyhow::anyhow!("Failed to create erasure coding for plot: {error}"))?;

        let piece_provider = subspace_networking::utils::piece_provider::PieceProvider::new(
            node.dsn().node.clone(),
            Some(SegmentCommitmentPieceValidator::new(
                node.dsn().node.clone(),
                node.rpc().clone(),
                kzg.clone(),
                // TODO: Consider introducing and using global in-memory segment commitments cache
                parking_lot::Mutex::new(lru::LruCache::new(SEGMENT_COMMITMENTS_CACHE_SIZE)),
            )),
        );
        let piece_getter = Arc::new(FarmerPieceGetter::new(
            node.dsn().node.clone(),
            piece_provider,
            piece_cache.clone(),
            node.dsn().farmer_archival_storage_info.clone(),
        ));

        let farmer_app_info = subspace_farmer::NodeClient::farmer_app_info(node.rpc())
            .await
            .expect("Node is always reachable");
        let last_segment_index = farmer_app_info.protocol_info.history_size.segment_index();

        let _piece_cache_population = subspace_farmer::utils::run_future_in_dedicated_thread(
            Box::pin({
                let piece_cache = piece_cache.clone();
                let piece_getter = piece_getter.clone();
                let dsn_node = node.dsn().node.clone();

                populate_pieces_cache(dsn_node, last_segment_index, piece_getter, piece_cache)
            }),
            format!("subspace-sdk-farmer-{node_name}-pieces-cache-population"),
        )
        .context("Failed to start piece cache population thread")?;

        let _piece_cache_maintainer = subspace_farmer::utils::run_future_in_dedicated_thread(
            Box::pin({
                let piece_cache = piece_cache.clone();
                let node = node.rpc().clone();

                fill_piece_cache_from_archived_segments(node, piece_cache)
            }),
            format!("subspace-sdk-farmer-{node_name}-pieces-cache-maintainer"),
        )
        .context("Failed to start piece cache maintainer thread")?;

        let max_pieces_in_sector = match max_pieces_in_sector {
            Some(m) => m,
            None => farmer_app_info.protocol_info.max_pieces_in_sector,
        };

        for (disk_farm_idx, description) in plots.iter().enumerate() {
            let (plot, single_disk_plot) = Plot::new(PlotOptions {
                disk_farm_idx,
                reward_address,
                node,
                max_pieces_in_sector,
                piece_getter: Arc::clone(&piece_getter),
                description,
                kzg: kzg.clone(),
                erasure_coding: erasure_coding.clone(),
            })
            .await?;
            plot_info.insert(plot.directory.clone(), plot);
            single_disk_plots.push(single_disk_plot);
        }

        readers_and_pieces.lock().replace(create_readers_and_pieces(
            &single_disk_plots,
            node.dsn().farmer_archival_storage_pieces.clone(),
        )?);

        for (disk_farm_index, single_disk_plot) in single_disk_plots.iter().enumerate() {
            let readers_and_pieces = Arc::clone(&readers_and_pieces);
            let span = tracing::info_span!("farm", %disk_farm_index);

            // Collect newly plotted pieces
            // TODO: Once we have replotting, this will have to be updated
            let handler_id = single_disk_plot.on_sector_plotted(Arc::new(
                move |(plotted_sector, maybe_old_plotted_sector)| {
                    let _span_guard = span.enter();
                    handler_on_sector_plotted(
                        plotted_sector,
                        maybe_old_plotted_sector,
                        disk_farm_index,
                        readers_and_pieces.clone(),
                    )
                },
            ));

            drop_at_exit.push(handler_id);
        }

        let mut single_disk_plots_stream =
            single_disk_plots.into_iter().map(SingleDiskPlot::run).collect::<FuturesUnordered<_>>();

        let (drop_sender, drop_receiver) = oneshot::channel::<()>();
        let (result_sender, result_receiver) = oneshot::channel::<_>();

        sdk_utils::task_spawn_blocking(format!("subspace-sdk-farmer-{node_name}-plots-driver"), {
            let handle = tokio::runtime::Handle::current();

            move || {
                use future::Either::*;

                let result = match handle
                    .block_on(future::select(single_disk_plots_stream.next(), drop_receiver))
                {
                    Left((maybe_result, _)) => maybe_result
                        .expect("We have at least one plot")
                        .context("Farmer exited with error"),
                    Right((_, _)) => Ok(()),
                };
                let _ = result_sender.send(result);
            }
        });

        node.dsn().farmer_piece_store.lock().await.replace(piece_store);
        node.dsn().farmer_provider_storage.swap(Some(farmer_provider_storage));

        drop_at_exit.defer({
            let provider_storage = node.dsn().farmer_provider_storage.clone();
            move || {
                let _ = drop_sender.send(());
                provider_storage.swap(None);
            }
        });

        let mut async_drop = AsyncDropFutures::new();

        async_drop.push({
            let piece_store = Arc::clone(&node.dsn().farmer_piece_store);
            async move {
                piece_store.lock().await.take();
            }
        });

        // TODO: check for piece cache to exit
        // async_drop.push(async move {
        //     const PIECE_STORE_POLL: Duration = Duration::from_millis(100);

        //     // HACK: Poll on piece store creation just to be sure
        //     loop {
        //         let result = ParityDbStore::<
        //             subspace_networking::libp2p::kad::record::Key,
        //             subspace_core_primitives::Piece,
        //         >::new(&piece_cache_db_path);

        //         match result.map(drop) {
        //             // If parity db is still locked wait on it
        //             Err(parity_db::Error::Locked(_)) =>
        // tokio::time::sleep(PIECE_STORE_POLL).await,             _ => break,
        //         }
        //     }
        // });

        tracing::debug!("Started farmer");

        Ok(Farmer {
            reward_address,
            plot_info,
            result_receiver: Some(result_receiver),
            node_name,
            base_path,
            app_info: subspace_farmer::NodeClient::farmer_app_info(node.rpc())
                .await
                .expect("Node is always reachable"),
            _drop_at_exit: drop_at_exit,
            _async_drop: Some(async_drop),
        })
    }
}

/// Farmer structure
#[derive(Derivative)]
#[derivative(Debug)]
#[must_use = "Farmer should be closed"]
pub struct Farmer<T: subspace_proof_of_space::Table> {
    reward_address: PublicKey,
    plot_info: HashMap<PathBuf, Plot<T>>,
    result_receiver: Option<oneshot::Receiver<anyhow::Result<()>>>,
    base_path: PathBuf,
    node_name: String,
    app_info: FarmerAppInfo,
    _drop_at_exit: DropCollection,
    _async_drop: Option<AsyncDropFutures>,
}

/// Info about some plot
#[derive(Debug)]
#[non_exhaustive]
// TODO: Should it be versioned?
pub struct PlotInfo {
    /// ID of the plot
    pub id: SingleDiskPlotId,
    /// Genesis hash of the chain used for plot creation
    pub genesis_hash: [u8; 32],
    /// Public key of identity used for plot creation
    pub public_key: PublicKey,
    /// How much space in bytes is allocated for this plot
    pub allocated_space: ByteSize,
    /// How many pieces are in sector
    pub pieces_in_sector: u16,
}

impl From<SingleDiskPlotInfo> for PlotInfo {
    fn from(info: SingleDiskPlotInfo) -> Self {
        let SingleDiskPlotInfo::V0 {
            id,
            genesis_hash,
            public_key,
            allocated_space,
            pieces_in_sector,
        } = info;
        Self {
            id,
            genesis_hash,
            public_key: PublicKey(public_key),
            allocated_space: ByteSize::b(allocated_space),
            pieces_in_sector,
        }
    }
}

/// Farmer info
#[derive(Debug)]
#[non_exhaustive]
pub struct Info {
    /// Version of the farmer
    pub version: String,
    /// Reward address of our farmer
    pub reward_address: PublicKey,
    // TODO: add dsn peers info
    // pub dsn_peers: u64,
    /// Info about each plot
    pub plots_info: HashMap<PathBuf, PlotInfo>,
    /// Sector size in bits
    pub sector_size: u64,
}

/// Initial plotting progress
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialPlottingProgress {
    /// Number of sectors from which we started plotting
    pub starting_sector: u64,
    /// Current number of sectors
    pub current_sector: u64,
    /// Total number of sectors on disk
    pub total_sectors: u64,
}

/// Progress data received from sender, used to monitor plotting progress
pub type ProgressData = Option<(PlottedSector, Option<PlottedSector>)>;

/// Plot structure
#[derive(Debug)]
pub struct Plot<T: subspace_proof_of_space::Table> {
    directory: PathBuf,
    progress: watch::Receiver<ProgressData>,
    solutions: watch::Receiver<Option<SolutionResponse>>,
    initial_plotting_progress: Arc<Mutex<InitialPlottingProgress>>,
    allocated_space: u64,
    _drop_at_exit: DropCollection,
    _table: std::marker::PhantomData<T>,
}

#[pin_project::pin_project]
struct InitialPlottingProgressStreamInner<S> {
    last_initial_plotting_progress: InitialPlottingProgress,
    #[pin]
    stream: S,
}

impl<S: Stream> Stream for InitialPlottingProgressStreamInner<S>
where
    S: Stream<Item = InitialPlottingProgress>,
{
    type Item = InitialPlottingProgress;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            result @ std::task::Poll::Ready(Some(progress)) => {
                *this.last_initial_plotting_progress = progress;
                result
            }
            result => result,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.last_initial_plotting_progress.total_sectors
            - self.last_initial_plotting_progress.current_sector;
        (left as usize, Some(left as usize))
    }
}

/// Initial plotting progress stream
#[pin_project::pin_project]
pub struct InitialPlottingProgressStream {
    #[pin]
    boxed_stream:
        std::pin::Pin<Box<dyn Stream<Item = InitialPlottingProgress> + Send + Sync + Unpin>>,
}

impl Stream for InitialPlottingProgressStream {
    type Item = InitialPlottingProgress;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.project().boxed_stream.poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.boxed_stream.size_hint()
    }
}

struct PlotOptions<'a, PG, N: sdk_traits::Node> {
    pub disk_farm_idx: usize,
    pub reward_address: PublicKey,
    pub node: &'a N,
    pub piece_getter: PG,
    pub description: &'a PlotDescription,
    pub kzg: kzg::Kzg,
    pub erasure_coding: ErasureCoding,
    pub max_pieces_in_sector: u16,
}

impl<T: subspace_proof_of_space::Table> Plot<T> {
    async fn new(
        PlotOptions {
            disk_farm_idx,
            reward_address,
            node,
            piece_getter,
            description,
            kzg,
            erasure_coding,
            max_pieces_in_sector,
        }: PlotOptions<
            '_,
            impl subspace_farmer_components::plotting::PieceGetter + Send + 'static,
            impl sdk_traits::Node,
        >,
    ) -> Result<(Self, SingleDiskPlot), BuildError> {
        let directory = description.directory.clone();
        let allocated_space = description.space_pledged.as_u64();
        let farmer_app_info = subspace_farmer::NodeClient::farmer_app_info(node.rpc())
            .await
            .expect("Node is always reachable");
        let description = SingleDiskPlotOptions {
            allocated_space,
            directory: directory.clone(),
            farmer_app_info,
            max_pieces_in_sector,
            reward_address: *reward_address,
            node_client: node.rpc().clone(),
            kzg,
            erasure_coding,
            piece_getter,
        };
        let single_disk_plot = SingleDiskPlot::new::<_, _, T>(description, disk_farm_idx).await?;
        let mut drop_at_exit = DropCollection::new();

        let progress = {
            let (sender, receiver) = watch::channel::<Option<_>>(None);
            drop_at_exit.push(single_disk_plot.on_sector_plotted(Arc::new(move |sector| {
                let _ = sender.send(Some(sector.clone()));
            })));
            receiver
        };
        let solutions = {
            let (sender, receiver) = watch::channel::<Option<_>>(None);
            drop_at_exit.push(single_disk_plot.on_solution(Arc::new(move |solution| {
                let _ = sender.send(Some(solution.clone()));
            })));
            receiver
        };

        Ok((
            Self {
                directory: directory.clone(),
                allocated_space,
                progress,
                solutions,
                initial_plotting_progress: Arc::new(Mutex::new(InitialPlottingProgress {
                    starting_sector: u64::try_from(single_disk_plot.plotted_sectors_count())
                        .expect("Sector count is less than u64::MAX"),
                    current_sector: u64::try_from(single_disk_plot.plotted_sectors_count())
                        .expect("Sector count is less than u64::MAX"),
                    total_sectors: allocated_space
                        / subspace_farmer_components::sector::sector_size(max_pieces_in_sector)
                            as u64,
                })),
                _drop_at_exit: drop_at_exit,
                _table: Default::default(),
            },
            single_disk_plot,
        ))
    }

    /// Plot location
    pub fn directory(&self) -> &PathBuf {
        &self.directory
    }

    /// Plot size
    pub fn allocated_space(&self) -> ByteSize {
        ByteSize::b(self.allocated_space)
    }

    /// Will return a stream of initial plotting progress which will end once we
    /// finish plotting
    pub async fn subscribe_initial_plotting_progress(&self) -> InitialPlottingProgressStream {
        let initial = *self.initial_plotting_progress.lock().await;
        if initial.current_sector == initial.total_sectors {
            return InitialPlottingProgressStream {
                boxed_stream: Box::pin(futures::stream::iter(None)),
            };
        }

        let stream = tokio_stream::wrappers::WatchStream::new(self.progress.clone())
            .filter_map({
                let initial_plotting_progress = Arc::clone(&self.initial_plotting_progress);
                move |_| {
                    let initial_plotting_progress = Arc::clone(&initial_plotting_progress);
                    async move {
                        let mut guard = initial_plotting_progress.lock().await;
                        let plotting_progress = *guard;
                        guard.current_sector += 1;
                        Some(plotting_progress)
                    }
                }
            })
            .take_while(|InitialPlottingProgress { current_sector, total_sectors, .. }| {
                futures::future::ready(current_sector < total_sectors)
            });
        let last_initial_plotting_progress = *self.initial_plotting_progress.lock().await;

        InitialPlottingProgressStream {
            boxed_stream: Box::pin(Box::pin(InitialPlottingProgressStreamInner {
                stream,
                last_initial_plotting_progress,
            })),
        }
    }

    /// New solution subscription
    pub async fn subscribe_new_solutions(
        &self,
    ) -> impl Stream<Item = SolutionResponse> + Send + Sync + Unpin {
        tokio_stream::wrappers::WatchStream::new(self.solutions.clone())
            .filter_map(futures::future::ready)
    }
}

impl<T: subspace_proof_of_space::Table> Farmer<T> {
    /// Farmer builder
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Gets plot info
    pub async fn get_info(&self) -> anyhow::Result<Info> {
        let plots_info = tokio::task::spawn_blocking({
            let dirs = self.plot_info.keys().cloned().collect::<Vec<_>>();
            || dirs.into_iter().map(SingleDiskPlot::collect_summary).collect::<Vec<_>>()
        })
        .await?
        .into_iter()
        .map(|summary| match summary {
            SingleDiskPlotSummary::Found { info, directory } => Ok((directory, info.into())),
            SingleDiskPlotSummary::NotFound { directory } =>
                Err(anyhow::anyhow!("Didn't found plot at `{directory:?}'")),
            SingleDiskPlotSummary::Error { directory, error } =>
                Err(error).context(format!("Failed to get plot summary at `{directory:?}'")),
        })
        .collect::<anyhow::Result<_>>()?;

        Ok(Info {
            plots_info,
            version: format!("{}-{}", env!("CARGO_PKG_VERSION"), env!("GIT_HASH")),
            reward_address: self.reward_address,
            sector_size: subspace_farmer_components::sector::sector_size(
                self.app_info.protocol_info.max_pieces_in_sector,
            ) as _,
        })
    }

    /// Iterate over plots
    pub async fn iter_plots(&'_ self) -> impl Iterator<Item = &'_ Plot<T>> + '_ {
        self.plot_info.values()
    }

    /// Stops farming, closes plots, and sends signal to the node
    pub async fn close(mut self) -> anyhow::Result<()> {
        let result_receiver = self.result_receiver.take().expect("Handle is always there");
        let async_drop = self._async_drop.take().expect("Always set").async_drop();

        drop(self);
        async_drop.await;

        result_receiver.await?
    }
}

const GET_PIECE_MAX_RETRIES_COUNT: u16 = 3;
const GET_PIECE_DELAY_IN_SECS: u64 = 3;
const POPULATE_PIECE_DELAY: Duration = Duration::from_secs(10);

/// Populates piece cache on startup. It waits for the new segment index and
/// check all pieces from previous segments to see if they are already in the
/// cache. If they are not, they are added from DSN.
async fn populate_pieces_cache<PV, PC>(
    dsn_node: subspace_networking::Node,
    segment_index: SegmentIndex,
    piece_getter: Arc<FarmerPieceGetter<PV, PC>>,
    piece_cache: Arc<tokio::sync::Mutex<FarmerPieceCache>>,
) where
    PV: PieceValidator + Send + Sync + 'static,
    PC: PieceCache + Send + 'static,
{
    let _ = dsn_node.wait_for_connected_peers(POPULATE_PIECE_DELAY).await;

    debug!(%segment_index, "Started syncing piece cache...");
    let final_piece_index =
        u64::from(segment_index.first_piece_index()) + ArchivedHistorySegment::NUM_PIECES as u64;

    // TODO: consider optimizing starting point of this loop
    let mut piece_index = 0;
    'outer: while piece_index < final_piece_index {
        // Scroll to the next piece index to cache.
        {
            let piece_cache = piece_cache.lock().await;
            while !piece_cache
                .should_cache(&PieceIndex::from(piece_index).hash().to_multihash().into())
            {
                piece_index += 1;

                if piece_index >= final_piece_index {
                    break 'outer;
                }
            }
        }

        let key = PieceIndex::from(piece_index).hash().to_multihash().into();

        let result =
            piece_getter.get_piece(piece_index.into(), PieceGetterRetryPolicy::Limited(1)).await;

        match result {
            Ok(Some(piece)) => {
                piece_cache.lock().await.add_piece(key, piece);
                debug!(%piece_index, "Added piece to cache.");
            }
            Ok(None) => {
                debug!(%piece_index, "Couldn't find piece.");
            }
            Err(err) => {
                debug!(error=%err, %piece_index, "Failed to get piece for piece cache.");
            }
        }

        piece_index += 1;
    }

    debug!("Finished syncing piece cache.");
}

/// Subscribes to a new segment index and adds pieces from the segment to the
/// cache if required.
async fn fill_piece_cache_from_archived_segments(
    node_client: impl subspace_farmer::node_client::NodeClient,
    piece_cache: Arc<tokio::sync::Mutex<FarmerPieceCache>>,
) {
    let segment_headers_notifications = node_client
        .subscribe_archived_segment_headers()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
        .context("Failed to subscribe to archived segments");

    match segment_headers_notifications {
        Ok(mut segment_headers_notifications) => {
            while let Some(segment_header) = segment_headers_notifications.next().await {
                let segment_index = segment_header.segment_index();

                debug!(%segment_index, "Starting to process archived segment....");

                for piece_index in segment_index.segment_piece_indexes() {
                    let key = piece_index.hash().to_multihash().into();
                    {
                        if !piece_cache.lock().await.should_cache(&key) {
                            trace!(%piece_index, ?key, "Piece key will not be included in the cache.");

                            continue;
                        }
                    }

                    trace!(%piece_index, ?key, "Piece key will be included in the cache.");

                    // Segment notification will come earlier than node's local cache finishes its
                    // initialization, so we need to wait for it.
                    let mut retries_count = 0u16;
                    'retry: loop {
                        if retries_count >= GET_PIECE_MAX_RETRIES_COUNT {
                            debug!(%piece_index, "Max retries number exceeded.");

                            break 'retry;
                        }

                        retries_count += 1;

                        let piece = node_client.piece(piece_index).await;

                        match piece {
                            Ok(Some(piece)) => {
                                piece_cache.lock().await.add_piece(key, piece);
                                trace!(%piece_index, "Got piece for archived segment.");

                                break 'retry;
                            }
                            Ok(None) => {
                                debug!(%piece_index, "Can't get piece. Retrying...");

                                tokio::time::sleep(Duration::from_secs(GET_PIECE_DELAY_IN_SECS))
                                    .await;
                            }
                            Err(err) => {
                                warn!(
                                    piece_index = ?piece_index,
                                    err = ?err,
                                    "Failed to get piece"
                                );
                            }
                        }
                    }
                }

                match node_client.acknowledge_archived_segment_header(segment_index).await {
                    Ok(()) => {
                        debug!(%segment_index, "Acknowledged archived segment.");
                    }
                    Err(err) => {
                        error!(%segment_index, ?err, "Failed to acknowledge archived segment.");
                    }
                };

                debug!(%segment_index, "Finished processing archived segment.");
            }
        }
        Err(err) => {
            error!(?err, "Failed to get archived segments notifications.")
        }
    }
}
