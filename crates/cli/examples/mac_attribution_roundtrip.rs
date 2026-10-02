//! Local verification utility for an already-captured synthetic harness state.
use objects::store::ObjectStore;
use std::{fs, io::Cursor, path::PathBuf};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    anyhow::ensure!(args.len() == 3, "usage: SOURCE RECEIVER OUTPUT");
    let source = repo::Repository::open(&args[0])?;
    let head = source
        .head()?
        .ok_or_else(|| anyhow::anyhow!("missing head"))?;
    let state = source
        .store()
        .get_state(&head)?
        .ok_or_else(|| anyhow::anyhow!("missing state"))?;
    let evidence = repo::load_attribution_evidence(source.store(), &state)?
        .ok_or_else(|| anyhow::anyhow!("missing attribution evidence"))?;
    fs::create_dir_all(&args[2])?;
    let index = args[2].join("source.idx");
    let builder = objects::store::StreamingPackBuilder::new(
        Cursor::new(Vec::new()),
        index.clone(),
        objects::store::CompressionConfig::default(),
        args[2].join("buckets"),
    )?;
    let (pack, _) = objects::store::pack::build_source_pack(
        builder,
        source.store(),
        &state,
        1000,
        16 * 1024 * 1024,
    )?;
    let bytes = pack.into_inner();
    fs::write(args[2].join("source.pack"), &bytes)?;
    let reader = objects::store::PackReader::from_bytes(bytes, fs::read(index)?)?;
    reader.validate_source_closure(&state, 1000, 16 * 1024 * 1024)?;
    let receiver = repo::Repository::init_default(&args[1])?;
    reader.visit_objects(|_, kind, bytes| {
        use objects::store::pack::ObjectType;
        match kind {
            ObjectType::State => {
                receiver
                    .store()
                    .put_state(&objects::object::State::decode_current_msgpack(bytes)?)?;
            }
            ObjectType::Tree => {
                receiver
                    .store()
                    .put_tree(&objects::object::Tree::decode_canonical(bytes)?)?;
            }
            ObjectType::Blob => {
                receiver
                    .store()
                    .put_blob(&objects::object::Blob::new(bytes.to_vec()))?;
            }
            _ => panic!("unexpected selected source object"),
        }
        Ok(())
    })?;
    let received_state = receiver
        .store()
        .get_state(&state.id())?
        .ok_or_else(|| anyhow::anyhow!("receiver missing state"))?;
    anyhow::ensure!(received_state.id() == state.id(), "state ID changed");
    anyhow::ensure!(
        received_state.attribution_evidence == state.attribution_evidence,
        "evidence digest changed"
    );
    let received = repo::load_attribution_evidence(receiver.store(), &received_state)?
        .ok_or_else(|| anyhow::anyhow!("receiver missing evidence"))?;
    anyhow::ensure!(received == evidence, "evidence changed");
    fs::write(
        args[2].join("received-evidence.txt"),
        format!("{received:#?}"),
    )?;
    println!(
        "State ID and complete evidence unchanged; {} operations roundtripped",
        received.operations.len()
    );
    Ok(())
}
