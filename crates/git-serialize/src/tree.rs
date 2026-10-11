// SPDX-License-Identifier: Apache-2.0
use heddle_object_model::object::{Tree, TreeEntry};

use crate::{ObjectFormat, ObjectId, Result, SerializeError};

/// Serialize exact, unframed Git tree bytes with already resolved entry IDs.
///
/// `resolve` is called in recorded Git order (canonical order for native trees).
/// Raw modes, including leading zeroes, are written verbatim. Spoollinks have
/// no Git representation and are omitted, matching local Git export. A caller
/// requiring lossless reconstruction must reject them before calling this.
pub fn serialize_tree(
    tree: &Tree,
    format: ObjectFormat,
    mut resolve: impl FnMut(&TreeEntry) -> Result<ObjectId>,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for entry in tree.git_ordered_entries() {
        let Some(mode) = entry.git_mode() else {
            continue;
        };
        let id = resolve(entry)?;
        if id.format() != format {
            return Err(SerializeError::InvalidObject(format!(
                "tree entry '{}' uses a different Git object format",
                entry.name()
            )));
        }
        mode.write_digits(&mut body);
        body.push(b' ');
        body.extend_from_slice(entry.name().as_bytes());
        body.push(0);
        body.extend_from_slice(id.as_bytes());
    }
    Ok(body)
}
