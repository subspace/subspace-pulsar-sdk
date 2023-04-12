use futures::prelude::*;
use subspace_sdk::farmer::{Info, PlotDescription};

use crate::common::{Farmer, Node};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_info() {
    crate::common::setup();

    let node = Node::dev().build().await;
    let farmer: Farmer = Farmer::dev().build(&node).await;

    let Info { reward_address, plots_info, .. } = farmer.get_info().await.unwrap();
    assert_eq!(reward_address, Default::default());
    assert_eq!(plots_info.len(), 1);
    assert_eq!(plots_info[&farmer.plot_dir()].allocated_space, PlotDescription::MIN_SIZE);

    farmer.close().await;
    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn track_progress() {
    crate::common::setup();

    let node = Node::dev().build().await;
    let n_sectors = 2;
    let farmer = Farmer::dev().n_sectors(n_sectors).build(&node).await;

    let progress = farmer
        .iter_plots()
        .await
        .next()
        .unwrap()
        .subscribe_initial_plotting_progress()
        .await
        .collect::<Vec<_>>()
        .await;
    assert_eq!(progress.len(), n_sectors as usize);

    farmer.close().await;
    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_solution() {
    crate::common::setup();

    let node = Node::dev().build().await;
    let farmer = Farmer::dev().build(&node).await;

    farmer
        .iter_plots()
        .await
        .next()
        .unwrap()
        .subscribe_new_solutions()
        .await
        .next()
        .await
        .expect("Farmer should send new solutions");

    farmer.close().await;
    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_restart() {
    crate::common::setup();

    let node = Node::dev().build().await;
    let farmer = Farmer::dev().build(&node).await;

    let plot = farmer.iter_plots().await.next().unwrap();

    plot.subscribe_initial_plotting_progress().await.for_each(|_| async {}).await;

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        plot.subscribe_initial_plotting_progress().await.for_each(|_| async {}),
    )
    .await
    .unwrap();

    farmer.close().await;
    node.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn farmer_restart() {
    crate::common::setup();

    let node = Node::dev().build().await;

    for _ in 0..10 {
        Farmer::dev().build(&node).await.close().await;
    }

    node.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn farmer_drop() {
    crate::common::setup();

    let node = Node::dev().build().await;
    drop(Farmer::dev().build(&node).await);
    node.close().await;
}