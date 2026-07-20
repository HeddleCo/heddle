// SPDX-License-Identifier: Apache-2.0
use std::{sync::Arc, time::Duration, time::Instant};

use api::heddle::api::v1alpha1::{
    ListRefsResponse, PullRequest, RefEntry, RepositoryRef, StateId as ProtoStateId,
    repository_ref::Reference,
};
use heddle_iroh_transport_experiment::{ExperimentRepository, IrohClient, IrohServer};
use objects::{
    object::{Blob, StateId},
    store::{FsStore, ObjectStore},
};
use wire::{ObjectId, ObjectInfo, ObjectType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let payload_mib = parse_arg(1, 16)?;
    let list_iterations = parse_arg(2, 200)?;
    let payload_size = payload_mib * 1024 * 1024;
    let source_dir = tempfile::tempdir()?;
    let baseline_dir = tempfile::tempdir()?;
    let network_dir = tempfile::tempdir()?;
    let source = FsStore::new(source_dir.path());
    let baseline_target = FsStore::new(baseline_dir.path());
    let network_target = FsStore::new(network_dir.path());
    let blob = Blob::new(incompressible_payload(payload_size));
    let hash = source.put_blob(&blob)?;
    let state = StateId::from_content_hash(hash);
    let objects = vec![ObjectInfo {
        id: ObjectId::Hash(hash),
        obj_type: ObjectType::Blob,
        size: blob.size() as u64,
        delta_base: None,
    }];

    let baseline_build_started = Instant::now();
    let mut baseline_writer = wire::NativePackStreamingWriter::new_in(source.root(), 1)?;
    baseline_writer.add_object_data(wire::load_object_data(
        &source,
        &objects[0].id,
        objects[0].obj_type,
    )?)?;
    let baseline_pack = baseline_writer.finish()?;
    let baseline_build = baseline_build_started.elapsed();
    let baseline_install_started = Instant::now();
    baseline_target.install_pack_streaming(&baseline_pack.pack_path, &baseline_pack.index_path)?;
    let baseline_install = baseline_install_started.elapsed();

    let repo = ExperimentRepository::new(source, refs(state), state, objects);
    let server_started = Instant::now();
    let server = IrohServer::spawn_loopback(Arc::new(repo)).await?;
    let server_bind = server_started.elapsed();
    let server_addr = server.addr();
    eprintln!("server address: {server_addr:?}");
    let connect_started = Instant::now();
    let client = IrohClient::connect_loopback(server_addr).await?;
    let connect = connect_started.elapsed();

    let wire = client.benchmark_wire(payload_size as u64).await?;
    let wire_mib = wire.bytes as f64 / 1_048_576.0;
    let wire_throughput = wire_mib / wire.transfer_latency.as_secs_f64();

    client.list_refs("/").await?;
    let mut list_latencies = Vec::with_capacity(list_iterations);
    let list_started = Instant::now();
    for _ in 0..list_iterations {
        let operation_started = Instant::now();
        client.list_refs("/").await?;
        list_latencies.push(operation_started.elapsed());
    }
    let list_total = list_started.elapsed();
    list_latencies.sort_unstable();

    let pull_started = Instant::now();
    let outcome = client
        .pull_native(
            &network_target,
            PullRequest {
                repo_path: Some(RepositoryRef {
                    reference: Some(Reference::CanonicalPath("/".to_string())),
                }),
                target_state: Some(ProtoStateId {
                    value: state.as_bytes().to_vec(),
                }),
                ..PullRequest::default()
            },
        )
        .await?;
    let pull = pull_started.elapsed();
    let transferred_mib = (outcome.pack_bytes + outcome.index_bytes) as f64 / 1_048_576.0;
    let end_to_end_throughput = transferred_mib / pull.as_secs_f64();
    let receive_throughput = transferred_mib / outcome.transfer_latency.as_secs_f64();

    println!("payload: {payload_mib} MiB ({payload_size} bytes)");
    println!("framing: operation-stream-v3");
    println!("server bind: {}", display(server_bind));
    println!("client connect: {}", display(connect));
    println!(
        "raw Iroh wire: {} in {} ({:.1} MiB/s)",
        wire.bytes,
        display(wire.transfer_latency),
        wire_throughput,
    );
    println!(
        "established ListRefs: {list_iterations} ops in {}, mean {}, p50 {}, p95 {}",
        display(list_total),
        display(list_total / list_iterations as u32),
        display(percentile(&list_latencies, 50)),
        display(percentile(&list_latencies, 95)),
    );
    println!(
        "local pack: build {}, install {}, {} pack bytes + {} index bytes",
        display(baseline_build),
        display(baseline_install),
        baseline_pack.pack_len,
        baseline_pack.index_len,
    );
    println!(
        "Iroh pull+build+install: {}, {:.1} MiB/s, {} installed objects",
        display(pull),
        end_to_end_throughput,
        outcome.installed.len(),
    );
    println!(
        "pull breakdown: ready {}, transfer {} ({:.1} MiB/s), install {}, completion {}",
        display(outcome.ready_latency),
        display(outcome.transfer_latency),
        receive_throughput,
        display(outcome.install_latency),
        display(outcome.completion_latency),
    );

    client.close().await;
    let shutdown_started = Instant::now();
    server.shutdown().await?;
    println!("server shutdown: {}", display(shutdown_started.elapsed()));
    Ok(())
}

fn parse_arg(index: usize, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    match std::env::args().nth(index) {
        Some(value) => Ok(value.parse()?),
        None => Ok(default),
    }
}

fn refs(state: StateId) -> ListRefsResponse {
    ListRefsResponse {
        head_thread: "main".to_string(),
        head_state: Some(ProtoStateId {
            value: state.as_bytes().to_vec(),
        }),
        refs: vec![RefEntry {
            name: "main".to_string(),
            state_id: Some(ProtoStateId {
                value: state.as_bytes().to_vec(),
            }),
            is_thread: true,
            revision_address: format!("heddle:{}", state.to_string_full()),
        }],
    }
}

fn incompressible_payload(size: usize) -> Vec<u8> {
    let mut value = 0x9e37_79b9_7f4a_7c15_u64;
    let mut payload = Vec::with_capacity(size);
    for _ in 0..size {
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        payload.push(value as u8);
    }
    payload
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let index = samples.len().saturating_sub(1) * percentile / 100;
    samples.get(index).copied().unwrap_or_default()
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
