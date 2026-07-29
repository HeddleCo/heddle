use std::{
    fs,
    path::{Path, PathBuf},
};

fn rust_files(root: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            rust_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

#[test]
fn owner_authorization_stays_unreachable_from_current_runtime_paths() {
    let client = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = client
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let owner_module = client.join("src/owner_authorization");
    let client_lib = client.join("src/lib.rs");
    let lib_source = fs::read_to_string(&client_lib).expect("read client lib");
    assert_eq!(
        lib_source.matches("pub mod owner_authorization;").count(),
        1,
        "the inert surface is exposed only as a data/crypto module"
    );

    let mut production_files = Vec::new();
    for relative in [
        "crates/client/src",
        "crates/cli/src",
        "crates/cli-args/src",
        "crates/core/src",
        "crates/repo/src",
    ] {
        rust_files(&workspace.join(relative), &mut production_files);
    }
    let imports = production_files
        .into_iter()
        .filter(|path| !path.starts_with(&owner_module) && path != &client_lib)
        .filter_map(|path| {
            let source = fs::read_to_string(&path).expect("read production Rust");
            source.contains("owner_authorization").then(|| {
                path.strip_prefix(workspace)
                    .expect("workspace path")
                    .to_path_buf()
            })
        })
        .collect::<Vec<_>>();
    assert!(
        imports.is_empty(),
        "owner authorization must remain unreachable until the exclusive cutover; \
         current runtime references: {imports:?}"
    );

    let mut owner_files = Vec::new();
    rust_files(&owner_module, &mut owner_files);
    let owner_source = owner_files
        .iter()
        .map(|path| fs::read_to_string(path).expect("read owner module"))
        .collect::<String>();
    for forbidden_live_edge in [
        "impl HostedClient",
        "CallContext",
        "grant_envelope",
        "MintBiscuitRequest",
        "MintAnonBiscuitRequest",
    ] {
        assert!(
            !owner_source.contains(forbidden_live_edge),
            "inert owner module gained current authorization edge {forbidden_live_edge}"
        );
    }
}
