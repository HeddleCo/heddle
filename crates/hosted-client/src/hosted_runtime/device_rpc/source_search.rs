//! Device-owned source Search extraction. Only the explicit analysis worker
//! invokes this; finite Search reads the persisted projection without parsing.
use std::{collections::BTreeMap, time::{Duration, Instant}};

use anyhow::{Context, Result, ensure};
use objects::{
    object::{ContentHash, StateId},
    store::ObjectStore,
};

use super::analysis;

pub(super) fn index_analyzed_source(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    state: StateId,
) -> Result<()> {
    index_source(repository, replica, state, None)
}

fn index_source(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    state: StateId,
    selected: Option<ContentHash>,
) -> Result<()> {
    let source = repository
        .store()
        .get_state(&state)?
        .context("source Search state absent")?;
    ensure!(source.id() == state, "source Search State identity mismatch");
    let mut documents = Vec::new();
    let mut pending = vec![(String::new(), source.tree, 0usize, Vec::<ContentHash>::new())];
    let mut path_leaves = BTreeMap::<String, Vec<ContentHash>>::new();
    let mut work = 0usize;
    let mut bytes = 0usize;
    let mut content_ready = true;
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some((prefix, hash, depth, parent_leaves)) = pending.pop() {
        work += 1;
        ensure!(
            work <= 4096 && depth <= 128 && Instant::now() < deadline,
            "source Search extraction work bound exceeded"
        );
        let tree = repository
            .store()
            .get_tree(&hash)?
            .context("source Search tree absent")?;
        ensure!(tree.hash() == hash, "source Search tree identity mismatch");
        for (index, entry) in tree.entries().iter().enumerate() {
            work += 1;
            ensure!(work <= 4096, "source Search entry bound exceeded");
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            ensure!(path.len() <= 4096, "source Search path bound exceeded");
            let mut leaf_chain = parent_leaves.clone();
            if tree.scheme() == objects::object::TreeScheme::V4Salted {
                leaf_chain.push(tree.v4_leaf_hash_at(index).context("salted source leaf absent")?);
            }
            if let Some(child) = entry.tree_hash() {
                pending.push((path, child, depth + 1, leaf_chain));
            } else if let Some(blob_hash) = entry.blob_hash() {
                path_leaves.insert(path.clone(), leaf_chain.clone());
                let Some(blob) = repository.store().get_blob(&blob_hash)? else {
                    content_ready = false;
                    continue;
                };
                ensure!(blob.hash() == blob_hash, "source Search blob identity mismatch");
                if blob.content().len() > 65536 {
                    content_ready = false;
                    continue;
                }
                bytes = bytes
                    .checked_add(blob.content().len())
                    .context("source Search byte overflow")?;
                ensure!(
                    bytes <= 8 * 1024 * 1024,
                    "source Search byte budget exceeded"
                );
                if let Ok(text) = std::str::from_utf8(blob.content()) {
                    documents.push(repo::thread_replication::source_search::Document {
                        kind: 3,
                        path,
                        symbol_id: String::new(),
                        symbol_name: String::new(),
                        start_line: None,
                        end_line: None,
                        text: text.to_owned(),
                        leaf_chain,
                    });
                }
            }
        }
    }
    let mut symbols_ready = repository.attached_semantic_index(&state)?.is_some();
    if symbols_ready {
        let mut complete = true;
        let symbols = analysis::semantic_symbols(
            repository,
            state,
            &objects::object::EntryRedactions::default(),
            &api::heddle::api::v2alpha1::ObserveAnalysisRequest::default(),
            &mut complete,
        )?;
        symbols_ready = complete;
        for ((path, address), symbol) in symbols {
            let Some(leaf_chain) = path_leaves.get(&path).cloned() else {
                symbols_ready = false;
                continue;
            };
            documents.push(repo::thread_replication::source_search::Document {
                kind: 4,
                path,
                symbol_id: address,
                symbol_name: symbol.name.clone(),
                start_line: Some(symbol.span.0),
                end_line: Some(symbol.span.1),
                text: symbol.name,
                leaf_chain,
            });
        }
        ensure!(
            documents.len() <= 8192,
            "source Search document bound exceeded"
        );
    }
    let originals = if let Some(operation) = selected {
        vec![operation]
    } else {
        replica.source_operation_page(state, None, 1024)?
    };
    ensure!(!originals.is_empty(), "source Search original absent");
    for operation in originals {
        repo::thread_replication::source_search::publish(
            repository.heddle_dir(),
            replica.thread_id(),
            operation,
            state,
            &documents,
            content_ready,
            symbols_ready,
        )?;
    }
    Ok(())
}

/// Process one durable accepted-source queue row outside the metadata transaction.
/// Failed or incomplete materialization is deferred, allowing later rows to progress.
pub(super) fn index_due_source(directory: &std::path::Path, now: i64) -> Result<Option<i64>> {
    let Some(item) = repo::thread_replication::source_search::due(directory, now, 1)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let outcome = (|| {
        let root = directory
            .parent()
            .context("source Search repository root absent")?;
        let repository = repo::Repository::open(root)?;
        let replica = repo::thread_replication::ThreadReplica::open(directory, item.thread)?;
        index_source(&repository, &replica, item.revision, Some(item.operation))
    })();
    if let Err(error) = outcome {
        tracing::warn!(operation=%item.operation, %error, "native source Search indexing deferred");
        repo::thread_replication::source_search::defer(
            directory,
            item.operation,
            now.saturating_add(60),
        )?;
    }
    Ok(Some(now))
}
