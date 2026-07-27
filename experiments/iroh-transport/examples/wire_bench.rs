// SPDX-License-Identifier: Apache-2.0
use std::{sync::Arc, time::Duration};

use api::heddle::api::v1alpha1::ListRefsResponse;
use heddle_iroh_transport_experiment::{ExperimentRepository, IrohClient, IrohServer};
use objects::{
    object::{Blob, StateId},
    store::{FsStore, ObjectStore},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let payload_mib = parse_arg(1, 256)?;
    let iterations = parse_arg(2, 5)?;
    if iterations == 0 {
        return Err("iterations must be greater than zero".into());
    }
    let payload_bytes = u64::try_from(payload_mib)? * 1024 * 1024;

    let source_dir = tempfile::tempdir()?;
    let source = FsStore::new(source_dir.path());
    let state = StateId::from_content_hash(source.put_blob(&Blob::from("wire benchmark"))?);
    let repo = ExperimentRepository::new(source, ListRefsResponse::default(), state, Vec::new());
    let server = IrohServer::spawn_loopback(Arc::new(repo)).await?;
    let client = IrohClient::connect_loopback(server.addr()).await?;

    client.benchmark_wire(1024 * 1024).await?;
    let mut samples = Vec::with_capacity(iterations);
    for iteration in 1..=iterations {
        let outcome = client.benchmark_wire(payload_bytes).await?;
        let throughput = payload_mib as f64 / outcome.transfer_latency.as_secs_f64();
        println!(
            "sample {iteration}: {} ({throughput:.1} MiB/s)",
            display(outcome.transfer_latency),
        );
        samples.push((throughput, outcome.transfer_latency));
    }
    samples.sort_by(|left, right| left.0.total_cmp(&right.0));
    let median = samples[samples.len() / 2];
    let maximum = samples.last().copied().unwrap_or_default();
    println!(
        "raw Iroh wire: {payload_mib} MiB x {iterations}; median {:.1} MiB/s ({}), max {:.1} MiB/s ({})",
        median.0,
        display(median.1),
        maximum.0,
        display(maximum.1),
    );

    client.close().await;
    server.shutdown().await?;
    Ok(())
}

fn parse_arg(index: usize, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    match std::env::args().nth(index) {
        Some(value) => Ok(value.parse()?),
        None => Ok(default),
    }
}

fn display(duration: Duration) -> String {
    if duration >= Duration::from_secs(1) {
        format!("{:.3} s", duration.as_secs_f64())
    } else if duration >= Duration::from_millis(1) {
        format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
    } else {
        format!("{:.1} us", duration.as_secs_f64() * 1_000_000.0)
    }
}
