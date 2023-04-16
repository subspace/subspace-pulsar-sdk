use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures::prelude::*;
use subspace_sdk::farmer::CacheDescription;
use subspace_sdk::node::{self, Event, Node, RewardsEvent, SubspaceEvent};
use subspace_sdk::{ByteSize, Farmer, PlotDescription, PublicKey};
use tracing_subscriber::prelude::*;

#[cfg(all(
    target_arch = "x86_64",
    target_vendor = "unknown",
    target_os = "linux",
    target_env = "gnu"
))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Subcommand, Debug)]
enum Chain {
    Gemini3D,
    Devnet,
}

/// Mini farmer
#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Set the chain
    #[command(subcommand)]
    chain: Chain,
    #[cfg(feature = "executor")]
    /// Should we run the executor?
    #[arg(short, long)]
    executor: bool,
    /// Address for farming rewards
    #[arg(short, long)]
    reward_address: PublicKey,
    /// Path for all data
    #[arg(short, long)]
    base_path: Option<PathBuf>,
    /// Size of the plot
    #[arg(short, long)]
    plot_size: ByteSize,
    /// Cache size
    #[arg(short, long, default_value_t = ByteSize::gib(1))]
    cache_size: ByteSize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fdlimit::raise_fd_limit();

    #[cfg(tokio_unstable)]
    let registry = tracing_subscriber::registry().with(console_subscriber::spawn());
    #[cfg(not(tokio_unstable))]
    let registry = tracing_subscriber::registry();

    registry
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
        .init();

    let Args {
        chain,
        #[cfg(feature = "executor")]
        executor,
        reward_address,
        base_path,
        plot_size,
        cache_size,
    } = Args::parse();
    let (base_path, _tmp_dir) = base_path.map(|x| (x, None)).unwrap_or_else(|| {
        let tmp = tempfile::tempdir().expect("Failed to create temporary directory");
        (tmp.as_ref().to_owned(), Some(tmp))
    });

    let node_dir = base_path.join("node");
    let node = match chain {
        Chain::Gemini3D => Node::gemini_3d().dsn(
            subspace_sdk::node::DsnBuilder::gemini_3d()
                .provider_storage_path(node_dir.join("provider_storage")),
        ),
        Chain::Devnet => Node::devnet().dsn(
            subspace_sdk::node::DsnBuilder::devnet()
                .provider_storage_path(node_dir.join("provider_storage")),
        ),
    }
    .role(node::Role::Authority);

    #[cfg(feature = "executor")]
    let node = if executor {
        node.system_domain(subspace_sdk::node::domains::ConfigBuilder::new())
    } else {
        node
    };

    let node = node
        .build(
            &node_dir,
            match chain {
                Chain::Gemini3D => node::chain_spec::gemini_3d(),
                Chain::Devnet => node::chain_spec::devnet_config(),
            },
        )
        .await?;

    tokio::select! {
        result = node.sync() => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::error!("Exitting...");
            return node.close().await.context("Failed to close node")
        }
    }
    tracing::error!("Node was synced!");

    let farmer = Farmer::builder()
        .build(
            reward_address,
            &node,
            &[PlotDescription::new(base_path.join("plot"), plot_size)
                .context("Failed to create a plot")?],
            CacheDescription::new(base_path.join("cache"), cache_size).unwrap(),
        )
        .await?;

    let subscriptions = {
        let plot = farmer.iter_plots().await.next().unwrap();

        let node = &node;

        async move {
            plot.subscribe_initial_plotting_progress()
                .await
                .for_each(|progress| async move {
                    tracing::error!(?progress, "Plotting!");
                })
                .await;
            tracing::error!("Finished initial plotting!");

            let mut new_blocks = node.subscribe_new_blocks().await?;
            while let Some(new_block) = new_blocks.next().await {
                let events = node.get_events(Some(new_block.hash)).await?;

                for event in events {
                    match event {
                        Event::Rewards(
                            RewardsEvent::VoteReward { reward, voter: author }
                            | RewardsEvent::BlockReward { reward, block_author: author },
                        ) if author == reward_address.into() =>
                            tracing::error!(%reward, "Received a reward!"),
                        Event::Subspace(SubspaceEvent::FarmerVote {
                            reward_address: author,
                            height: block_number,
                            ..
                        }) if author == reward_address.into() =>
                            tracing::error!(block_number, "Vote counted for block"),
                        _ => (),
                    };
                }

                if let Some(pre_digest) = new_block.pre_digest {
                    if pre_digest.solution.reward_address == reward_address {
                        tracing::error!("We authored a block");
                    }
                }
            }

            anyhow::Ok(())
        }
    };

    tokio::select! {
        _ = subscriptions => {},
        _ = tokio::signal::ctrl_c() => {
            tracing::error!("Exitting...");
        }
    }

    node.close().await.context("Failed to close node")?;
    farmer.close().await.context("Failed to close farmer")?;

    Ok(())
}
