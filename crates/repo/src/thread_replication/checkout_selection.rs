//! One-shot path selection constructs the exact supplied source tree. A status
//! observation is only a scan hint and cannot enforce a capture selection.
use std::collections::BTreeMap;

use objects::{
    object::{Tree, TreeEntry, TreeEntryTarget},
    store::ObjectStore,
};

use super::{Error, Result};

pub(super) fn selected_tree(
    store: &impl ObjectStore,
    baseline: Tree,
    working: &Tree,
    paths: &[String],
) -> Result<Tree> {
    let parts = paths
        .iter()
        .map(|path| path.split('/').map(str::to_owned).collect())
        .collect::<Vec<Vec<String>>>();
    select(store, baseline, working, &parts)
}
fn select(
    store: &impl ObjectStore,
    mut baseline: Tree,
    working: &Tree,
    paths: &[Vec<String>],
) -> Result<Tree> {
    let mut groups = BTreeMap::<String, Vec<Vec<String>>>::new();
    for path in paths {
        let (name, rest) = path
            .split_first()
            .ok_or_else(|| Error::Invalid("empty capture selection".into()))?;
        groups.entry(name.clone()).or_default().push(rest.to_vec());
    }
    for (name, rest) in groups {
        if rest.iter().any(Vec::is_empty) {
            baseline.remove(&name);
            if let Some(entry) = working.get(&name) {
                baseline.insert(entry.clone());
            }
            continue;
        }
        let base = subtree(store, baseline.get(&name))?;
        let current = subtree(store, working.get(&name))?;
        let selected = select(store, base, &current, &rest)?;
        baseline.remove(&name);
        if !selected.entries().is_empty() {
            baseline.insert(
                TreeEntry::directory(name, store.put_tree(&selected)?)
                    .map_err(|error| Error::Invalid(error.to_string()))?,
            );
        }
    }
    Ok(baseline)
}
fn subtree(store: &impl ObjectStore, entry: Option<&TreeEntry>) -> Result<Tree> {
    match entry.map(TreeEntry::target) {
        Some(TreeEntryTarget::Tree { hash }) => store
            .get_tree(hash)?
            .ok_or_else(|| Error::Invalid("selected tree missing".into())),
        _ => Ok(Tree::new()),
    }
}
