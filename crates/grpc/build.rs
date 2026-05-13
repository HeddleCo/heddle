// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| std::io::Error::other("failed to locate workspace root"))?;
    let proto_dir = workspace_root.join("proto");
    let proto = proto_dir.join("heddle/v1/service.proto");
    let descriptor_path = manifest_dir
        .join(std::env::var("OUT_DIR")?)
        .join("heddle_descriptor.bin");

    println!("cargo:rerun-if-changed={}", proto.display());

    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[proto_dir])?;

    Ok(())
}