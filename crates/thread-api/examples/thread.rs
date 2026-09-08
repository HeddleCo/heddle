// SPDX-License-Identifier: Apache-2.0
//! Run with `cargo run --locked --example thread` in this experiment.
mod support;

use anyhow::{Context, Result};
use heddle_thread_api::{content::BlobSource, contract::*};

#[tokio::main]
async fn main() -> Result<()> {
    let peer = support::Peer::start(support::Scenario::Normal).await?;
    let remote = peer.connect().await?;
    let thread = remote.thread(support::thread_ref());
    let mut view = thread
        .observe(
            &[
                ThreadSection::Overview,
                ThreadSection::Captures,
                ThreadSection::Review,
                ThreadSection::Collaboration,
            ],
            ObservationMode::Follow,
            None,
        )
        .await?;
    let snapshot = view.next_commit().await?.context("initial snapshot")?;
    println!("Live server observations: {}", peer.active_streams());
    let overview = snapshot
        .changes
        .into_iter()
        .find_map(|change| match change {
            thread_event::Payload::Overview(overview) => Some(overview),
            _ => None,
        })
        .context("Thread overview")?;
    println!(
        "Thread: {} ({} captures)",
        overview.name, overview.capture_count
    );

    let receipt = thread
        .revise_intent(
            "demo-revise-intent-1",
            overview.intent.as_ref().context("observed intent")?,
            ThreadIntent {
                outcome: "One Thread view; exact content on demand".into(),
                ..Default::default()
            },
        )
        .await?;
    println!(
        "Intent edit: {:?}",
        receipt.receipt.context("mutation receipt")?.outcome
    );
    let update = view.next_commit().await?.context("committed update")?;
    println!(
        "Live update: {} committed change(s), replacement={}",
        update.changes.len(),
        update.replace
    );

    let [revision] = overview.source_heads.as_slice() else {
        anyhow::bail!("select a source head before reading content");
    };
    let blobs = remote
        .read_blobs(
            revision.clone(),
            vec![
                BlobSource::Path("README.md".into()),
                BlobSource::ObjectHash(vec![9; 32]),
            ],
        )
        .await?;
    println!(
        "Content: {} blobs, one request, {} total bytes",
        blobs.len(),
        blobs.iter().map(|b| b.bytes.len()).sum::<usize>()
    );
    view.cancel();
    println!("RPCs (including one-time discovery):");
    for route in peer.routes()? {
        println!("  {route}");
    }
    println!("Loopback contract fixture; no live Weft or durable publication involved.");
    peer.close().await;
    Ok(())
}
