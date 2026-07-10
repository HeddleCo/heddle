// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    // The proto sources ship inside this crate so the published tarball on
    // crates.io contains everything needed to run `cargo build` from a fresh
    // download.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("proto");
    let proto_files = [
        "heddle/v1/service.proto",
        "heddle/v1/common.proto",
        "heddle/v1/sync.proto",
        "heddle/v1/hosted.proto",
        "heddle/v1/auth.proto",
        "heddle/v1/content.proto",
        "heddle/v1/repo_events.proto",
        "heddle/v1/threads.proto",
        "heddle/v1/review.proto",
        "heddle/v1/feed.proto",
        "heddle/v1/policies.proto",
        "heddle/v1/state_review.proto",
        "heddle/v1/discussion.proto",
        "heddle/v1/signals.proto",
        "heddle/v1/operations.proto",
        "heddle/v1/timeline.proto",
        "heddle/v1/transactions.proto",
        "heddle/v1/hooks.proto",
        "heddle/v1/support.proto",
        "heddle/v1/tree_edit.proto",
        "heddle/v1/search.proto",
        "heddle/v1/import.proto",
    ]
    .map(|path| proto_dir.join(path));
    let descriptor_path = PathBuf::from(std::env::var("OUT_DIR")?).join("heddle_descriptor.bin");

    for proto in &proto_files {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .bytes(".heddle.v1.ObjectChunk.data")
        .bytes(".heddle.v1.PackChunk.data")
        .bytes(".heddle.v1.RedactionTransfer.redactions_blob")
        .bytes(".heddle.v1.StateVisibilityTransfer.state_visibility_blob")
        .bytes(".heddle.v1.GitPackTransfer.pack_chunk")
        .bytes(".heddle.v1.GitPackTransfer.pack_id")
        .bytes(".heddle.v1.GitRefUpdateTransfer.target_oid")
        .bytes(".heddle.v1.GitRefUpdateTransfer.peeled_oid")
        .bytes(".heddle.v1.GitRefUpdateTransfer.expected_target_oid")
        .bytes(".heddle.v1.GitCheckpointTransfer.heddle_change_id")
        .bytes(".heddle.v1.GitCheckpointTransfer.git_commit_oid")
        .build_server(true)
        .build_client(true)
        .compile_protos(&proto_files, &[proto_dir])?;

    Ok(())
}
