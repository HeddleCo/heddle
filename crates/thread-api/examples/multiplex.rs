// SPDX-License-Identifier: Apache-2.0
mod support;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use heddle_thread_api::contract::{ObservationMode, ThreadSection};

#[tokio::main]
async fn main() -> Result<()> {
    let count = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1024".into())
        .parse::<usize>()?;
    ensure!(
        (1..=1500).contains(&count),
        "choose 1..=1500 streams (fixture advertises 2048)"
    );
    let peer = support::Peer::start(support::Scenario::Normal).await?;
    let remote = peer.connect().await?;
    // Two rounds prove cancellation releases live server tasks on the same
    // connection instead of merely making the client's handles disappear.
    for round in 1..=2 {
        ensure!(
            peer.active_streams() == 0,
            "previous round leaked observations"
        );
        let start = Instant::now();
        let mut views = Vec::with_capacity(count);
        for _ in 0..count {
            let mut view = remote
                .thread(support::thread_ref())
                .observe(&[ThreadSection::Overview], ObservationMode::Follow, None)
                .await?;
            view.next_commit()
                .await?
                .context("initial committed view")?;
            views.push(view);
        }
        ensure!(
            peer.active_streams() == count,
            "all observations must remain live together"
        );
        println!(
            "round {round}: {} live observations, one Iroh connection, {:?}",
            peer.active_streams(),
            start.elapsed()
        );
        drop(views);
        tokio::time::timeout(Duration::from_secs(5), async {
            while peer.active_streams() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("server observations did not stop after client cancellation")?;
        println!(
            "round {round}: after cancellation, {} live server observations",
            peer.active_streams()
        );
    }
    println!(
        "{} verified v2 RPCs; loopback fixture, not a production capacity benchmark",
        peer.routes()?.len()
    );
    peer.close().await;
    Ok(())
}
