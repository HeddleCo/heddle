// SPDX-License-Identifier: Apache-2.0

use objects::object::SemanticFileNode;

pub(super) fn contains(source: &SemanticFileNode, ancestor: u32, mut scope: u32) -> bool {
    loop {
        if scope == ancestor {
            return true;
        }
        let Some(parent) = source
            .scopes
            .iter()
            .find(|entry| entry.local_id == scope)
            .and_then(|entry| entry.parent)
        else {
            return false;
        };
        scope = parent;
    }
}

pub(super) fn depth(source: &SemanticFileNode, mut scope: u32) -> usize {
    let mut depth = 0;
    while let Some(parent) = source
        .scopes
        .iter()
        .find(|entry| entry.local_id == scope)
        .and_then(|entry| entry.parent)
    {
        depth += 1;
        scope = parent;
    }
    depth
}
