// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    path::{Path, PathBuf},
};

fn collect_proto_files(source_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(source_dir)? {
        let path = entry?.path();
        if path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "nested protobuf directory '{}' violates the flat heddle.v1 contract",
                    path.display()
                ),
            ));
        }
        if path
            .extension()
            .is_some_and(|extension| extension == "proto")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

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
    let source_dir = proto_dir.join("heddle/v1");
    // Heddle v1 is deliberately one flat package. Deriving every build and
    // generator from that directory avoids hand-maintained inventory drift.
    let proto_files = collect_proto_files(&source_dir)?;
    if proto_files.is_empty() {
        return Err("canonical proto tree is empty".into());
    }
    let descriptor_path = PathBuf::from(std::env::var("OUT_DIR")?).join("heddle_descriptor.bin");

    // Watching the directory is what makes adding or removing a schema file
    // rerun this build script; the per-file watches below cover content edits.
    println!("cargo:rerun-if-changed={}", source_dir.display());
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
        .bytes(".heddle.v1.GitCheckpointTransfer.heddle_state_id")
        .bytes(".heddle.v1.GitCheckpointTransfer.git_commit_oid")
        .build_server(true)
        .build_client(true)
        .compile_protos(&proto_files, &[proto_dir])?;

    Ok(())
}
