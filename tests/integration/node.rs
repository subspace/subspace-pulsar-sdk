use std::sync::Arc;

use futures::prelude::*;
use subspace_sdk::farmer::{CacheDescription, Farmer, PlotDescription};
use tempfile::TempDir;
use tracing_futures::Instrument;

use crate::common::Node;

async fn sync_block_inner() {
    crate::common::setup();

    let node = Node::dev().build().await;
    let (plot_dir, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let farmer = Farmer::builder()
        .build(
            Default::default(),
            &node,
            &[PlotDescription::minimal(plot_dir.as_ref())],
            CacheDescription::minimal(cache_dir.as_ref()),
        )
        .await
        .unwrap();

    let farm_blocks = 4;

    node.subscribe_new_blocks()
        .await
        .unwrap()
        .skip_while(|notification| futures::future::ready(notification.number < farm_blocks))
        .next()
        .await
        .unwrap();

    farmer.close().await.unwrap();

    let other_node = Node::dev()
        .chain(node.chain.clone())
        .boot_nodes(node.listen_addresses().await.unwrap())
        .not_force_synced(true)
        .not_authority(true)
        .build()
        .await;

    other_node.subscribe_syncing_progress().await.unwrap().for_each(|_| async {}).await;
    assert_eq!(other_node.get_info().await.unwrap().best_block.1, farm_blocks);

    node.close().await;
    other_node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(any(tarpaulin, not(target_os = "linux")), ignore = "Slow tests are run only on linux")]
async fn sync_block() {
    tokio::time::timeout(std::time::Duration::from_secs(60 * 60), sync_block_inner()).await.unwrap()
}

async fn sync_plot_inner() {
    crate::common::setup();

    let node_span = tracing::trace_span!("node 1");
    let node = Node::dev().build().instrument(node_span.clone()).await;

    let (plot_dir, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let farmer = Farmer::builder()
        .build(
            Default::default(),
            &node,
            &[PlotDescription::minimal(plot_dir.as_ref())],
            CacheDescription::minimal(cache_dir.as_ref()),
        )
        .instrument(node_span.clone())
        .await
        .unwrap();

    let farm_blocks = 4;

    node.subscribe_new_blocks()
        .await
        .unwrap()
        .skip_while(|notification| futures::future::ready(notification.number < farm_blocks))
        .next()
        .await
        .unwrap();

    let other_node_span = tracing::trace_span!("node 2");
    let other_node = Node::dev()
        .dsn_boot_nodes(node.dsn_listen_addresses().await.unwrap())
        .boot_nodes(node.listen_addresses().await.unwrap())
        .not_force_synced(true)
        .not_authority(true)
        .chain(node.chain.clone())
        .build()
        .instrument(other_node_span.clone())
        .await;

    while other_node.get_info().await.unwrap().best_block.1
        < node.get_info().await.unwrap().best_block.1
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let (plot_dir, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let other_farmer = Farmer::builder()
        .build(
            Default::default(),
            &node,
            &[PlotDescription::minimal(plot_dir.as_ref())],
            CacheDescription::minimal(cache_dir.as_ref()),
        )
        .instrument(other_node_span.clone())
        .await
        .unwrap();

    let plot = other_farmer.iter_plots().await.next().unwrap();
    plot.subscribe_initial_plotting_progress().await.for_each(|_| async {}).await;
    farmer.close().await.unwrap();

    plot.subscribe_new_solutions().await.next().await.expect("Solution stream never ends");

    node.close().await;
    other_node.close().await;
    other_farmer.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(any(tarpaulin, not(target_os = "linux")), ignore = "Slow tests are run only on linux")]
async fn sync_plot() {
    tokio::time::timeout(std::time::Duration::from_secs(60 * 60), sync_plot_inner()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn node_restart() {
    crate::common::setup();
    let dir = Arc::new(TempDir::new().unwrap());

    for i in 0..4 {
        tracing::error!(i, "Running new node");
        Node::dev().path(dir.clone()).build().await.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(any(tarpaulin, not(target_os = "linux")), ignore = "Slow tests are run only on linux")]
async fn node_events() {
    crate::common::setup();

    let dir = TempDir::new().unwrap();
    let node = Node::dev().build().await;
    let farmer = Farmer::builder()
        .build(
            Default::default(),
            &node,
            &[PlotDescription::minimal(dir.path().join("plot"))],
            CacheDescription::minimal(dir.path().join("cache")),
        )
        .await
        .unwrap();

    let events = node
        .subscribe_new_blocks()
        .await
        .unwrap()
        // Skip genesis
        .skip(1)
        .then(|_| node.get_events(None).boxed())
        .take(1)
        .next()
        .await
        .unwrap()
        .unwrap();

    assert!(!events.is_empty());

    farmer.close().await.unwrap();
    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(any(tarpaulin, not(target_os = "linux")), ignore = "Slow tests are run only on linux")]
async fn fetch_block_author() {
    let dir = TempDir::new().unwrap();
    let node = Node::dev().build().await;
    let reward_address = Default::default();
    let farmer = Farmer::builder()
        .build(
            reward_address,
            &node,
            &[PlotDescription::minimal(dir.path().join("plot"))],
            CacheDescription::minimal(dir.path().join("cache")),
        )
        .await
        .unwrap();

    let block = node.subscribe_new_blocks().await.unwrap().skip(1).take(1).next().await.unwrap();
    assert_eq!(block.pre_digest.unwrap().solution.reward_address, reward_address);

    farmer.close().await.unwrap();
    node.close().await;
}
